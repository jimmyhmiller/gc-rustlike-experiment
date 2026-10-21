//! Job-local claims with a small inline prefix. Claims keep their actual heap
//! lifetime; no claim or live header reference enters reusable worker storage.
use super::{LOCAL_BUFFER_CAPACITY, PendingCopy};
use crate::runtime::metadata_allocator::MetadataAllocator;
use allocator_api2::{alloc::Allocator, vec::Vec as NativeVec};

const INLINE_CLAIMS: usize = 8;

pub(super) struct PendingCopies<'a, A: Allocator = MetadataAllocator> {
    inline: [Option<PendingCopy<'a>>; INLINE_CLAIMS],
    len: usize,
    spill: NativeVec<PendingCopy<'a>, A>,
}
impl<'a, A: Allocator> PendingCopies<'a, A> {
    pub(super) fn with_allocator(allocator: A) -> Self {
        Self {
            inline: std::array::from_fn(|_| None),
            len: 0,
            spill: NativeVec::new_in(allocator),
        }
    }
    #[inline]
    pub(super) fn push(&mut self, copy: PendingCopy<'a>) {
        if self.spill.capacity() != 0 {
            self.spill.push(copy);
        } else if self.len < INLINE_CLAIMS {
            self.inline[self.len] = Some(copy);
            self.len += 1;
        } else {
            // Keep the prefix in place. Reserve the previous full batch capacity
            // so later drains can refill entirely into spill storage without
            // reallocating. Failure leaves all guards owned for rollback.
            self.spill.reserve(LOCAL_BUFFER_CAPACITY);
            self.spill.push(copy);
        }
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        self.len + self.spill.len()
    }
    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    #[inline]
    pub(super) fn iter(&self) -> impl Iterator<Item = &PendingCopy<'a>> {
        self.inline[..self.len]
            .iter()
            .map(|slot| slot.as_ref().unwrap())
            .chain(self.spill.iter())
    }

    #[inline]
    pub(super) fn drain(&mut self) -> Drain<'_, 'a, A> {
        let len = std::mem::take(&mut self.len);
        Drain {
            inline: InlineDrain(self.inline[..len].iter_mut()),
            spill: self.spill.drain(..),
        }
    }
}

impl<'a> Default for PendingCopies<'a> {
    fn default() -> Self {
        Self::with_allocator(MetadataAllocator::default())
    }
}

pub(super) struct Drain<'b, 'a, A: Allocator = MetadataAllocator> {
    pub(super) inline: InlineDrain<'b, 'a>,
    pub(super) spill: allocator_api2::vec::Drain<'b, PendingCopy<'a>, A>,
}
impl<'a, A: Allocator> Iterator for Drain<'_, 'a, A> {
    type Item = PendingCopy<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inline.next().or_else(|| self.spill.next())
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let count = self.inline.len() + self.spill.len();
        (count, Some(count))
    }
}
impl<A: Allocator> ExactSizeIterator for Drain<'_, '_, A> {}

pub(super) struct InlineDrain<'b, 'a>(std::slice::IterMut<'b, Option<PendingCopy<'a>>>);
impl<'a> Iterator for InlineDrain<'_, 'a> {
    type Item = PendingCopy<'a>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|slot| slot.take().unwrap())
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}
impl ExactSizeIterator for InlineDrain<'_, '_> {}
impl Drop for InlineDrain<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        // A partially consumed drain still owns its remaining inline guards.
        for slot in self.0.by_ref() {
            drop(slot.take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Claim;
    use super::*;
    use crate::{gc::FORWARDING_BIT, runtime::allocation_probe};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn copy(header: &AtomicU64, index: usize) -> PendingCopy<'_> {
        header.store(FORWARDING_BIT, Ordering::Relaxed);
        PendingCopy {
            old: std::ptr::null_mut(),
            slot: std::ptr::null_mut(),
            size: index,
            align_log2: 3,
            needs_scan: true,
            claim: Claim {
                header,
                original: index as u64 + 8,
                published: false,
            },
        }
    }

    #[test]
    fn collector_pending_copies_match_order_and_reuse_after_draining() {
        for count in [0, 1, 7, 8, 9, 32, LOCAL_BUFFER_CAPACITY] {
            let headers: Vec<_> = (0..count).map(|_| AtomicU64::new(0)).collect();
            let mut pending = PendingCopies::default();
            for _ in 0..3 {
                for (index, header) in headers.iter().enumerate() {
                    pending.push(copy(header, index));
                }
                assert_eq!(pending.len(), count);
                assert_eq!(pending.is_empty(), count == 0);
                assert_eq!(
                    pending.iter().map(|copy| copy.size).collect::<Vec<_>>(),
                    (0..count).collect::<Vec<_>>()
                );
                let mut drain = pending.drain();
                for remaining in (1..=count).rev() {
                    assert_eq!(drain.len(), remaining);
                    let item = drain.next().unwrap();
                    assert_eq!(item.size, count - remaining);
                    drop(item);
                }
                assert_eq!(drain.next().map(|item| item.size), None);
                drop(drain);
                assert!(pending.is_empty());
                for (index, header) in headers.iter().enumerate() {
                    assert_eq!(header.load(Ordering::Relaxed), index as u64 + 8);
                }
            }
        }
    }

    #[test]
    fn collector_pending_copies_partial_drain_restores_unpublished_headers() {
        for count in [1, 8, 9, 32, LOCAL_BUFFER_CAPACITY] {
            for consumed in [0, 1, count / 2, count] {
                let headers: Vec<_> = (0..count).map(|_| AtomicU64::new(0)).collect();
                let mut pending = PendingCopies::default();
                for (index, header) in headers.iter().enumerate() {
                    pending.push(copy(header, index));
                }
                let mut drain = pending.drain();
                for _ in 0..consumed {
                    drop(drain.next().unwrap());
                }
                drop(drain);
                assert!(pending.is_empty());
                // Check before PendingCopies itself drops: drain owns rollback.
                for (index, header) in headers.iter().enumerate() {
                    assert_eq!(header.load(Ordering::Relaxed), index as u64 + 8);
                }
            }
        }
    }

    #[test]
    fn collector_pending_copies_unwind_preserves_published_and_restores_remaining() {
        for count in [1, 8, 9, LOCAL_BUFFER_CAPACITY] {
            let headers: Vec<_> = (0..count).map(|_| AtomicU64::new(0)).collect();
            let mut pending = PendingCopies::default();
            for (index, header) in headers.iter().enumerate() {
                pending.push(copy(header, index));
            }
            let forwarded = 4096 | FORWARDING_BIT;
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut drain = pending.drain();
                    let mut published = drain.next().unwrap();
                    published.claim.header.store(forwarded, Ordering::Relaxed);
                    published.claim.published = true;
                    drop(published);
                    panic!("injected publication failure");
                }))
                .is_err()
            );
            assert!(pending.is_empty());
            assert_eq!(headers[0].load(Ordering::Relaxed), forwarded);
            for (index, header) in headers.iter().enumerate().skip(1) {
                assert_eq!(header.load(Ordering::Relaxed), index as u64 + 8);
            }
        }
    }

    #[test]
    fn collector_pending_copies_drop_restores_all_owned_headers() {
        for count in [0, 8, 9, LOCAL_BUFFER_CAPACITY] {
            let headers: Vec<_> = (0..count).map(|_| AtomicU64::new(0)).collect();
            {
                let mut pending = PendingCopies::default();
                for (index, header) in headers.iter().enumerate() {
                    pending.push(copy(header, index));
                }
            }
            for (index, header) in headers.iter().enumerate() {
                assert_eq!(header.load(Ordering::Relaxed), index as u64 + 8);
            }
        }
    }

    #[test]
    fn collector_pending_copies_spill_reserves_once_and_reuses() {
        let headers: Vec<_> = (0..LOCAL_BUFFER_CAPACITY)
            .map(|_| AtomicU64::new(0))
            .collect();
        allocation_probe::start();
        let mut pending = PendingCopies::default();
        for _ in 0..3 {
            for (index, header) in headers.iter().enumerate() {
                pending.push(copy(header, index));
            }
            for item in pending.drain() {
                drop(item);
            }
        }
        drop(pending);
        let traffic = allocation_probe::finish_bytes();
        let bytes = LOCAL_BUFFER_CAPACITY * std::mem::size_of::<PendingCopy<'_>>();
        assert_eq!(traffic, (1, 1, bytes, bytes));
        eprintln!(
            "claim_bytes={} inline_container_bytes={} spill_request_bytes={}",
            std::mem::size_of::<PendingCopy<'_>>(),
            std::mem::size_of::<PendingCopies<'_>>(),
            bytes
        );
        assert!(std::mem::size_of::<PendingCopies<'_>>() <= 512);
    }

    #[test]
    fn collector_pending_copies_inline_has_zero_allocator_traffic() {
        let headers: [AtomicU64; 8] = std::array::from_fn(|_| AtomicU64::new(0));
        allocation_probe::start();
        let mut pending = PendingCopies::default();
        for (index, header) in headers.iter().enumerate() {
            pending.push(copy(header, index));
        }
        for item in pending.drain() {
            drop(item);
        }
        drop(pending);
        let traffic = allocation_probe::finish_bytes();
        assert_eq!(traffic, (0, 0, 0, 0));
    }

    #[test]
    fn collector_pending_copies_borrowed_spill_tracks_backing_and_partial_drain() {
        use crate::runtime::allocation_counter::AllocationCounter;
        use std::sync::Arc;
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let headers: [AtomicU64; 9] = std::array::from_fn(|_| AtomicU64::new(0));
        allocation_probe::start();
        let mut pending = PendingCopies::with_allocator(&allocator);
        for (index, header) in headers[..8].iter().enumerate() {
            pending.push(copy(header, index));
        }
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 2);
        allocation_probe::start();
        pending.push(copy(&headers[8], 8));
        let traffic = allocation_probe::finish_bytes();
        let bytes = pending.spill.capacity() * std::mem::size_of::<PendingCopy<'_>>();
        assert!(bytes > 0);
        assert_eq!(traffic, (1, 0, bytes, 0));
        assert_eq!(counter.bytes(), bytes);
        assert_eq!(Arc::strong_count(&counter), 2);
        let mut drain = pending.drain();
        for index in 0..3 {
            let item = drain.next().unwrap();
            assert_eq!(item.size, index);
            drop(item);
        }
        drop(drain);
        assert!(pending.is_empty());
        for (index, header) in headers.iter().enumerate() {
            assert_eq!(header.load(Ordering::Relaxed), index as u64 + 8);
        }
        assert_eq!(counter.bytes(), bytes, "draining does not release backing");
        allocation_probe::start();
        for _ in 0..32 {
            for (index, header) in headers.iter().enumerate() {
                pending.push(copy(header, index));
            }
            for item in pending.drain() {
                drop(item);
            }
        }
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(counter.bytes(), bytes);
        drop(pending);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 2);
        drop(allocator);
        assert_eq!(Arc::strong_count(&counter), 1);
    }
}
