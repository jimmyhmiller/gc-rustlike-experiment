//! Unique writable root addresses for one paused evacuation.
//! Small collections avoid allocating a hash table. Once spilled, addresses
//! retain the hash set's bounded storage even if sources repeat many slots.
use crate::runtime::allocation_counter::AllocationCounter;
use crate::runtime::metadata_allocator::MetadataAllocator;
use hashbrown::HashSet;
use std::collections::hash_map::RandomState;
use std::sync::Arc;

const INLINE_ROOTS: usize = 16;

#[derive(Default)]
pub(super) struct RootSlots<'a> {
    inline: [usize; INLINE_ROOTS],
    len: usize,
    spill: HashSet<usize, RandomState, MetadataAllocator>,
    counter: Option<&'a Arc<AllocationCounter>>,
}

impl<'a> RootSlots<'a> {
    /// Reserve for a known number of distinct slots without changing the
    /// allocation-free inline path. Further root families still grow normally.
    #[cfg(test)]
    pub(super) fn with_capacity(distinct_slots: usize) -> Self {
        let mut roots = Self::default();
        if distinct_slots > INLINE_ROOTS {
            roots.reserve_spill(distinct_slots);
        }
        roots
    }

    pub(super) fn with_counter(distinct_slots: usize, counter: &'a Arc<AllocationCounter>) -> Self {
        let mut roots = Self {
            counter: Some(counter),
            ..Self::default()
        };
        if distinct_slots > INLINE_ROOTS {
            roots.reserve_spill(distinct_slots);
        }
        roots
    }

    // Borrow the Heap's counter while inline. Acquire an independent owner only
    // when backing is needed; the consuming iterator can then outlive the borrow.
    fn reserve_spill(&mut self, capacity: usize) {
        if let Some(counter) = self.counter.take() {
            debug_assert_eq!(self.spill.capacity(), 0);
            self.spill = HashSet::with_hasher_in(
                self.spill.hasher().clone(),
                MetadataAllocator::new(counter),
            );
        }
        self.spill.reserve(capacity);
    }

    #[inline]
    pub(super) fn insert(&mut self, slot: usize) {
        if !self.spill.is_empty() {
            self.spill.insert(slot);
        } else if !self.inline[..self.len].contains(&slot) {
            if self.len < INLINE_ROOTS {
                self.inline[self.len] = slot;
                self.len += 1;
            } else {
                self.reserve_spill(INLINE_ROOTS + 1);
                self.spill.extend(self.inline);
                self.spill.insert(slot);
                self.len = 0;
            }
        }
    }
}

impl IntoIterator for RootSlots<'_> {
    type Item = usize;
    type IntoIter = std::iter::Chain<
        std::iter::Take<std::array::IntoIter<usize, INLINE_ROOTS>>,
        hashbrown::hash_set::IntoIter<usize, MetadataAllocator>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.inline.into_iter().take(self.len).chain(self.spill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::allocation_probe;

    #[test]
    fn collector_root_slots_known_capacity_avoids_growth() {
        for count in [0, 1, 16, 17, 257, 4096] {
            allocation_probe::start();
            let mut slots = RootSlots::with_capacity(count);
            let initial = allocation_probe::finish_bytes();
            assert_eq!(initial.0, usize::from(count > INLINE_ROOTS));
            allocation_probe::start();
            for _ in 0..3 {
                for index in 0..count {
                    slots.insert(index * 4096);
                }
            }
            let growth = allocation_probe::finish_bytes();
            assert_eq!(growth, (0, 0, 0, 0));
            assert_eq!(slots.into_iter().count(), count);
        }
        // Other sources may add unique slots beyond the known global count,
        // while repeated registrations may revisit all existing addresses.
        for hint in [0, 16, 17, 257] {
            let mut slots = RootSlots::with_capacity(hint);
            for index in 0..4096 {
                slots.insert(index * 4096);
                slots.insert(index * 4096);
            }
            let mut actual: Vec<_> = slots.into_iter().collect();
            actual.sort_unstable();
            assert_eq!(actual, (0..4096).map(|i| i * 4096).collect::<Vec<_>>());
        }
    }

    #[test]
    fn collector_root_slots_match_unique_addresses_and_exact_size() {
        for count in [0, 1, 15, 16, 17, 32, 1024] {
            for repeats in [1, 2, 64] {
                let mut slots = RootSlots::default();
                for pass in 0..repeats {
                    for index in 0..count {
                        // Reverse alternate passes, including zero and addresses
                        // with equal low bits. Deduplicate addresses, not values.
                        let index = if pass % 2 == 0 {
                            index
                        } else {
                            count - 1 - index
                        };
                        slots.insert(index * 4096);
                    }
                }
                let mut iter = slots.into_iter();
                for remaining in (1..=count).rev() {
                    assert_eq!(iter.size_hint(), (remaining, Some(remaining)));
                    assert!(iter.next().is_some());
                }
                assert_eq!(iter.size_hint(), (0, Some(0)));
                assert_eq!(iter.next(), None);
                assert_eq!(iter.next(), None);

                let mut slots = RootSlots::default();
                for index in (0..count).rev() {
                    slots.insert(index * 4096);
                    slots.insert(index * 4096);
                }
                let mut actual: Vec<_> = slots.into_iter().collect();
                actual.sort_unstable();
                assert_eq!(actual, (0..count).map(|i| i * 4096).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn collector_root_slots_repeated_spilled_addresses_do_not_allocate() {
        let mut slots = RootSlots::default();
        for index in 0..=INLINE_ROOTS {
            slots.insert(index * 4096);
        }
        allocation_probe::start();
        for _ in 0..1024 {
            for index in (0..=INLINE_ROOTS).rev() {
                slots.insert(index * 4096);
            }
        }
        let traffic = allocation_probe::finish_bytes();
        assert_eq!(traffic, (0, 0, 0, 0));
        assert_eq!(slots.len, 0);
        assert_eq!(slots.spill.len(), INLINE_ROOTS + 1);
        assert_eq!(slots.into_iter().count(), INLINE_ROOTS + 1);
    }

    #[test]
    fn collector_root_slots_moving_collection_updates_distinct_alias_slots() {
        use crate::gc::{Compact, Full, Heap, IdentityPtrPolicy, ObjHeader, RootSet, TypeInfo};
        for full in [false, true] {
            let size = if full { Full::SIZE } else { Compact::SIZE };
            let info = TypeInfo::for_header(size).with_raw_bytes(8);
            let heap = if full {
                Heap::new::<Full>(1 << 20, vec![info.clone()])
            } else {
                Heap::new::<Compact>(1 << 20, vec![info.clone()])
            };
            for count in [0, 1, 15, 16, 17, 257] {
                let mut roots = RootSet::new();
                if count > 0 {
                    let object = if full {
                        unsafe { heap.alloc_obj::<Full>(&info, 0) }
                    } else {
                        unsafe { heap.alloc_obj::<Compact>(&info, 0) }
                    };
                    unsafe {
                        object.add(info.raw_data_offset()).cast::<u64>().write(42);
                    }
                    for _ in 0..count {
                        roots.add(object as u64);
                    }
                }
                for _ in 0..3 {
                    unsafe {
                        heap.collect::<IdentityPtrPolicy>(&[&roots, &roots]);
                    }
                    assert_eq!(
                        heap.from_used(),
                        usize::from(count > 0) * info.allocation_size(0)
                    );
                    for index in 0..count {
                        assert_eq!(roots.get(index), roots.get(0));
                        let object = roots.get(index) as *const u8;
                        assert_eq!(
                            unsafe { object.add(info.raw_data_offset()).cast::<u64>().read() },
                            42
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn collector_root_slots_inline_enrollment_does_not_allocate() {
        let mut slots = RootSlots::default();
        allocation_probe::start();
        for _ in 0..64 {
            for index in 0..INLINE_ROOTS {
                slots.insert(index * 8);
            }
        }
        let mut count = 0;
        for _ in slots {
            count += 1;
        }
        let traffic = allocation_probe::finish_bytes();
        assert_eq!(count, INLINE_ROOTS);
        assert_eq!(traffic, (0, 0, 0, 0));
    }
}

#[cfg(test)]
mod accounting_tests {
    use super::*;
    use crate::runtime::allocation_probe;
    use std::sync::Arc;

    #[test]
    fn collector_root_dedup_backing_tracks_requests_through_consuming_iterator() {
        for hint in [0, 16, 257] {
            let counter = Arc::new(Default::default());
            allocation_probe::start();
            let mut slots = RootSlots::with_counter(hint, &counter);
            for index in 0..257 {
                slots.insert(index * 8);
            }
            let traffic = allocation_probe::finish_bytes();
            let charged = counter.bytes();
            assert!(charged > 0);
            assert_eq!(
                charged,
                traffic.2 - traffic.3,
                "bucket/control request bytes must match physical allocator traffic"
            );
            allocation_probe::start();
            for _ in 0..8 {
                for index in 0..257 {
                    slots.insert(index * 8);
                }
            }
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(counter.bytes(), charged);
            let mut remaining = slots.into_iter();
            let first = remaining.next().unwrap();
            assert_eq!(
                counter.bytes(),
                charged,
                "consuming iteration still owns backing"
            );
            let mut values: Vec<_> = remaining.by_ref().collect();
            values.push(first);
            values.sort_unstable();
            assert_eq!(values, (0..257).map(|i| i * 8).collect::<Vec<_>>());
            drop(remaining);
            assert_eq!(counter.bytes(), 0);
        }
    }

    #[test]
    fn collector_root_dedup_inline_accounted_owner_allocates_nothing() {
        let counter = Arc::new(Default::default());
        allocation_probe::start();
        let mut slots = RootSlots::with_counter(INLINE_ROOTS, &counter);
        assert_eq!(
            Arc::strong_count(&counter),
            1,
            "inline roots must not acquire an allocator owner"
        );
        for _ in 0..8 {
            for index in 0..INLINE_ROOTS {
                slots.insert(index * 8);
            }
        }
        assert_eq!(slots.into_iter().count(), INLINE_ROOTS);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(counter.bytes(), 0);
    }
}

#[cfg(test)]
mod borrowed_counter_tests {
    use super::*;

    #[test]
    fn collector_root_dedup_spill_iterator_outlives_borrowed_counter() {
        let counter = Arc::new(Default::default());
        let weak = Arc::downgrade(&counter);
        let mut remaining = {
            let mut slots = RootSlots::with_counter(17, &counter);
            for index in 0..17 {
                slots.insert(index * 8);
            }
            slots.into_iter()
        };
        drop(counter);
        assert!(
            weak.upgrade().is_some(),
            "spill backing must retain its independent owner"
        );
        assert!(remaining.next().is_some());
        drop(remaining);
        assert!(
            weak.upgrade().is_none(),
            "iterator release must drop the final allocator owner"
        );
    }
}
