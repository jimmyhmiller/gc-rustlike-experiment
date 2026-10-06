//! Object/card overlap metadata, incrementally extended at world pauses.
//!
//! Initialized arena ranges only grow until reset. An earlier range boundary is
//! an object boundary, so subtracting the previous initialized ranges lets us
//! walk new objects, including prefixes filled into previously unused TLAB tails.
use std::ops::Range;

use super::alloc::AtomicBumpAllocator;
use super::field::{read_type_id, read_varlen_count};
use super::type_info::{TypeInfo, VarLenKind};

#[derive(Default)]
pub(super) struct ObjectStartIndex {
    epoch: Option<usize>,
    ranges: Vec<Range<usize>>,
    starts: Vec<usize>,
}

impl ObjectStartIndex {
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
