//! Object/card overlap metadata, incrementally extended at world pauses.
//!
//! Initialized arena ranges only grow until reset. An earlier range boundary is
//! an object boundary, so subtracting the previous initialized ranges lets us
//! walk new objects, including prefixes filled into previously unused TLAB tails.
use crate::runtime::metadata_allocator::MetadataAllocator;
use allocator_api2::vec::Vec as NativeVec;
use std::ops::Range;

use super::alloc::AtomicBumpAllocator;
use super::field::{read_type_id, read_varlen_count};
use super::type_info::{TypeInfo, VarLenKind};

pub(super) struct ObjectStartIndex {
    epoch: Option<usize>,
    ranges: NativeVec<Range<usize>, MetadataAllocator>,
    starts: NativeVec<usize, MetadataAllocator>,
}

#[cfg(test)]
impl Default for ObjectStartIndex {
    fn default() -> Self {
        Self::with_allocator(MetadataAllocator::default())
    }
}

impl ObjectStartIndex {
    /// Both reusable buffers retain their accounting owner until physical release.
    pub(super) fn with_allocator(allocator: MetadataAllocator) -> Self {
        Self {
            epoch: None,
            ranges: NativeVec::new_in(allocator.clone()),
            starts: NativeVec::new_in(allocator),
        }
    }

    /// The allocator must be stopped, all reservations initialized, and the
    /// type table must describe its objects. Keep this index bound to one arena.
    pub(super) unsafe fn update(
        &mut self,
        space: &AtomicBumpAllocator,
        ranges: &[Range<usize>],
        types: &[TypeInfo],
        card_size: usize,
    ) {
        assert!(card_size.is_power_of_two());
        if self.epoch != Some(space.epoch()) {
            self.starts.clear();
            self.ranges.clear();
            self.epoch = Some(space.epoch());
        }
        self.starts
            .resize(space.used().div_ceil(card_size), usize::MAX);
        for range in ranges {
            let mut offset = range.start;
            let mut previous = self.ranges.partition_point(|r| r.end <= offset);
            while offset < range.end {
                if let Some(old) = self.ranges.get(previous) {
                    if old.start <= offset {
                        offset = old.end;
                        previous += 1;
                        continue;
                    }
                }
                let end = self
                    .ranges
                    .get(previous)
                    .map_or(range.end, |old| old.start.min(range.end));
                while offset < end {
                    let obj = unsafe { space.base().add(offset) };
                    let info =
                        &types[unsafe { read_type_id(obj, space.type_id_offset()) } as usize];
                    let len = match info.varlen {
                        VarLenKind::None => 0,
                        _ => unsafe { read_varlen_count(obj, info) },
                    };
                    let size = info.allocation_size(len);
                    assert!(
                        size <= end - offset,
                        "object crosses initialized index extent"
                    );
                    for start in
                        &mut self.starts[offset / card_size..=(offset + size - 1) / card_size]
                    {
                        *start = (*start).min(offset);
                    }
                    offset += size;
                }
            }
        }
        self.ranges.clear();
        self.ranges.extend_from_slice(ranges);
    }

    pub(super) fn get(&self, card: usize) -> Option<usize> {
        self.starts
            .get(card)
            .copied()
            .filter(|&offset| offset != usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::alloc::{HeapWalker, Tlab, alloc_obj};
    use crate::gc::field::init_header;
    use crate::gc::header::{Full, ObjHeader};

    fn verify(index: &mut ObjectStartIndex, space: &AtomicBumpAllocator, types: &[TypeInfo]) {
        let ranges = space.initialized_ranges();
        unsafe {
            index.update(space, &ranges, types, 512);
        }
        let mut objects = Vec::new();
        unsafe {
            space.walk(types, &mut |obj, info| {
                let len = if info.varlen == VarLenKind::None {
                    0
                } else {
                    read_varlen_count(obj, info)
                };
                objects.push((
                    obj as usize - space.base() as usize,
                    info.allocation_size(len),
                ));
            });
        }
        for card in 0..space.size().div_ceil(512) {
            let expected = objects
                .iter()
                .filter(|&&(start, size)| start < (card + 1) * 512 && start + size > card * 512)
                .map(|&(start, _)| start)
                .min();
            assert_eq!(index.get(card), expected, "card {card}");
        }
    }

    #[test]
    fn appended_objects_large_overlaps_and_reset() {
        let space = AtomicBumpAllocator::new::<Full>(16 * 1024);
        let types = [
            TypeInfo::for_header(Full::SIZE),
            TypeInfo {
                type_id: 1,
                ..TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0)
            },
        ];
        let mut index = ObjectStartIndex::default();
        verify(&mut index, &space, &types);
        for _ in 0..35 {
            assert!(!unsafe { alloc_obj::<Full>(&space, &types[0], 0) }.is_null());
        }
        verify(&mut index, &space, &types);
        assert!(!unsafe { alloc_obj::<Full>(&space, &types[1], 8193) }.is_null());
        verify(&mut index, &space, &types);
        verify(&mut index, &space, &types);
        unsafe {
            space.reset();
        }
        assert!(!unsafe { alloc_obj::<Full>(&space, &types[0], 0) }.is_null());
        verify(&mut index, &space, &types);
    }

    #[test]
    fn active_and_retired_tlab_holes_fill_out_of_address_order() {
        let space = AtomicBumpAllocator::new::<Full>(16 * 1024);
        let types = [TypeInfo::for_header(Full::SIZE).with_fields(1)];
        let mut first = Tlab::new();
        let mut second = Tlab::new();
        let mut index = ObjectStartIndex::default();
        for tlab in [&mut first, &mut second] {
            let obj = tlab.alloc(&space, &types[0], 0);
            assert!(!obj.is_null());
            unsafe {
                init_header::<Full>(obj, 0);
            }
        }
        verify(&mut index, &space, &types);
        for _ in 0..100 {
            let obj = first.alloc(&space, &types[0], 0);
            assert!(!obj.is_null());
            unsafe {
                init_header::<Full>(obj, 0);
            }
        }
        verify(&mut index, &space, &types);
        drop(first);
        for _ in 0..100 {
            let obj = second.alloc(&space, &types[0], 0);
            assert!(!obj.is_null());
            unsafe {
                init_header::<Full>(obj, 0);
            }
        }
        verify(&mut index, &space, &types);
        unsafe {
            space.reset();
        }
        let obj = second.alloc(&space, &types[0], 0);
        assert!(!obj.is_null());
        unsafe {
            init_header::<Full>(obj, 0);
        }
        verify(&mut index, &space, &types);
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe};
    use std::sync::Arc;

    #[test]
    fn collector_object_index_empty_owned_buffers_allocate_nothing() {
        let counter = Arc::<AllocationCounter>::default();
        allocation_probe::start();
        let index = ObjectStartIndex::with_allocator(MetadataAllocator::new(&counter));
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 3);
        assert_eq!(index.get(0), None);
        drop(index);
        assert_eq!(Arc::strong_count(&counter), 1);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    }

    #[test]
    fn collector_object_index_retains_actual_backing_charge_across_warm_reuse() {
        let counter = Arc::<AllocationCounter>::default();
        let mut index = ObjectStartIndex::with_allocator(MetadataAllocator::new(&counter));
        // Exercise both independently grown buffer layouts, without allocating
        // managed arena storage or relying on a standard Vec's private layout.
        allocation_probe::start();
        index.ranges.extend((0..9).map(|i| i * 32..i * 32 + 16));
        index.starts.resize(257, usize::MAX);
        index.starts[256] = 42;
        let cold = allocation_probe::finish_bytes();
        assert!(cold.0 >= 2);
        assert_eq!(counter.bytes(), cold.2 - cold.3);
        assert!(
            counter.bytes()
                >= 9 * std::mem::size_of::<Range<usize>>() + 257 * std::mem::size_of::<usize>()
        );
        let retained = counter.bytes();
        assert_eq!(index.get(256), Some(42));
        allocation_probe::start();
        index.ranges.clear();
        index.starts.clear();
        assert_eq!(index.get(256), None);
        index.ranges.push(0..16);
        index.starts.resize(3, usize::MAX);
        index.starts[1] = 16;
        assert_eq!(index.get(1), Some(16));
        assert_eq!(counter.bytes(), retained);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        drop(index);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
    }

    #[test]
    fn collector_object_index_owns_counter_after_constructor_scope_ends() {
        let (index, observer) = {
            let counter = Arc::<AllocationCounter>::default();
            let mut index = ObjectStartIndex::with_allocator(MetadataAllocator::new(&counter));
            index.ranges.push(0..512);
            index.starts.resize(64, 0);
            let observer = Arc::downgrade(&counter);
            (index, observer)
        };
        {
            let counter = observer.upgrade().expect("index owns accounting state");
            assert!(counter.bytes() >= 64 * std::mem::size_of::<usize>());
            assert_eq!(Arc::strong_count(&counter), 3);
            assert_eq!(index.get(63), Some(0));
        }
        drop(index);
        assert!(observer.upgrade().is_none());
    }
}
