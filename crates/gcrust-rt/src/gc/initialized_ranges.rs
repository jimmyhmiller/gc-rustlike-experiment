//! Immutable initialized regions. Small snapshots need no backing allocation.
//! Larger snapshots reuse one buffer for sorted holes and their complement.
use super::{BufferExtent, shared_metadata::SharedMetadata};
use allocator_api2::{alloc::Allocator, vec::Vec as NativeVec};
use std::{
    ops::{Deref, Range},
    sync::atomic::Ordering,
};

pub(crate) enum InitializedRanges<A: Allocator> {
    Inline {
        ranges: [Range<usize>; 3],
        len: usize,
    },
    Backed(NativeVec<Range<usize>, A>),
}
impl<A: Allocator> InitializedRanges<A> {
    pub(super) fn capture(used: usize, buffers: &[SharedMetadata<BufferExtent>], allocator: A) -> Self {
        let mut inline = std::array::from_fn(|_| 0..0);
        let mut count = 0;
        for (index, buffer) in buffers.iter().enumerate() {
            let Some(hole) = Self::hole(buffer, used) else {
                continue;
            };
            if count < 2 {
                inline[count] = hole;
                count += 1;
                continue;
            }
            // Only scan the suffix twice once inline capacity is exceeded.
            // Quiescent callers keep initialized prefixes stable throughout capture.
            let remaining = &buffers[index + 1..];
            let tails = remaining
                .iter()
                .filter(|b| Self::hole(b, used).is_some())
                .count();
            let capacity = tails
                .checked_add(4)
                .expect("initialized range snapshot capacity overflow");
            let mut holes = NativeVec::with_capacity_in(capacity, allocator);
            holes.extend_from_slice(&inline[..2]);
            holes.push(hole);
            for buffer in remaining {
                if let Some(hole) = Self::hole(buffer, used) {
                    holes.push(hole);
                }
            }
            holes.sort_unstable_by_key(|r| r.start);
            let (len, tail) = Self::prefixes(&mut holes, used);
            holes.truncate(len);
            if let Some(tail) = tail {
                holes.push(tail);
            }
            if holes.len() <= inline.len() {
                let len = holes.len();
                for (dst, src) in inline.iter_mut().zip(holes.iter()) {
                    *dst = src.clone();
                }
                // No backing is retained when coalescing leaves a small result.
                drop(holes);
                return Self::Inline {
                    ranges: inline,
                    len,
                };
            }
            return Self::Backed(holes);
        }
        inline[..count].sort_unstable_by_key(|r| r.start);
        let (mut len, tail) = Self::prefixes(&mut inline[..count], used);
        if let Some(tail) = tail {
            inline[len] = tail;
            len += 1;
        }
        Self::Inline {
            ranges: inline,
            len,
        }
    }

    fn hole(buffer: &BufferExtent, used: usize) -> Option<Range<usize>> {
        let initialized = buffer.initialized.load(Ordering::Relaxed);
        assert!(
            buffer.start <= initialized && initialized <= buffer.end && buffer.end <= used,
            "invalid initialized TLAB extent"
        );
        (initialized < buffer.end).then_some(initialized..buffer.end)
    }

    fn prefixes(holes: &mut [Range<usize>], used: usize) -> (usize, Option<Range<usize>>) {
        let mut start = 0;
        let mut written = 0;
        for read in 0..holes.len() {
            let hole = holes[read].clone();
            assert!(start <= hole.start, "overlapping uninitialized TLAB tails");
            if start < hole.start {
                // Each hole yields at most one prefix: writes never overtake reads.
                holes[written] = start..hole.start;
                written += 1;
            }
            start = hole.end;
        }
        (written, (start < used).then_some(start..used))
    }
}
impl<A: Allocator> Deref for InitializedRanges<A> {
    type Target = [Range<usize>];
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Inline { ranges, len } => &ranges[..*len],
            Self::Backed(ranges) => ranges,
        }
    }
}
impl<A: Allocator> AsRef<[Range<usize>]> for InitializedRanges<A> {
    fn as_ref(&self) -> &[Range<usize>] {
        self
    }
}
impl<'a, A: Allocator> IntoIterator for &'a InitializedRanges<A> {
    type Item = &'a Range<usize>;
    type IntoIter = std::slice::Iter<'a, Range<usize>>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        allocation_counter::AllocationCounter, allocation_probe,
        metadata_allocator::MetadataAllocator,
    };
    use std::sync::{Arc, atomic::AtomicUsize};
    fn buffer(start: usize, end: usize, initialized: usize) -> SharedMetadata<BufferExtent> {
        SharedMetadata::new_in(BufferExtent {
            start,
            end,
            initialized: AtomicUsize::new(initialized),
        }, &MetadataAllocator::default())
    }

    #[test]
    fn collector_range_snapshot_inline_cases_allocate_nothing() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let cases = [
            (0, vec![], vec![]),
            (64, vec![], vec![0..64]),
            (64, vec![buffer(0, 64, 32)], vec![0..32]),
            (64, vec![buffer(8, 24, 16)], vec![0..16, 24..64]),
            (
                64,
                vec![buffer(40, 56, 48), buffer(8, 24, 16)],
                vec![0..16, 24..48, 56..64],
            ),
        ];
        for (used, buffers, expected) in cases {
            allocation_probe::start();
            let snapshot = InitializedRanges::capture(used, &buffers, &allocator);
            assert_eq!(snapshot.as_ref(), expected);
            drop(snapshot);
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(counter.bytes(), 0);
            assert_eq!(Arc::strong_count(&counter), 2);
        }
    }

    #[test]
    fn collector_range_snapshot_backing_owns_charge_after_source_release() {
        let counter = Arc::<AllocationCounter>::default();
        let weak = Arc::downgrade(&counter);
        let buffers = vec![
            buffer(72, 88, 80),
            buffer(0, 16, 8),
            buffer(48, 64, 56),
            buffer(24, 40, 32),
        ];
        allocation_probe::start();
        let snapshot = InitializedRanges::capture(128, &buffers, MetadataAllocator::new(&counter));
        let traffic = allocation_probe::finish_bytes();
        assert_eq!(traffic.0, 1);
        assert_eq!(traffic.1, 0);
        assert_eq!(counter.bytes(), traffic.2 - traffic.3);
        assert!(counter.bytes() > 0);
        assert!(buffers.iter().all(|b| SharedMetadata::strong_count(b) == 1));
        drop(buffers);
        assert_eq!(snapshot.as_ref(), &[0..8, 16..32, 40..56, 64..80, 88..128]);
        drop(counter);
        assert!(weak.upgrade().is_some());
        drop(snapshot);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn collector_range_snapshot_compacts_and_releases_temporary_backing() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let buffers: Vec<_> = (0..4)
            .map(|i| buffer(i * 16, (i + 1) * 16, i * 16))
            .collect();
        allocation_probe::start();
        let snapshot = InitializedRanges::capture(64, &buffers, &allocator);
        let traffic = allocation_probe::finish_bytes();
        assert!(snapshot.is_empty());
        assert_eq!((traffic.0, traffic.1), (1, 1));
        assert_eq!(traffic.2, traffic.3);
        assert_eq!(counter.bytes(), 0);
        assert!(counter.peak() > 0);
        assert_eq!(Arc::strong_count(&counter), 2);
    }

    #[test]
    fn collector_range_snapshot_matches_seeded_initialized_byte_model() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let mut seed = 17u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed >> 32
        };
        let mut inline = 0;
        let mut backed = 0;
        for _ in 0..128 {
            let count = (next() % 9) as usize;
            let mut buffers = Vec::new();
            let mut cursor = 0;
            for _ in 0..count {
                cursor += (next() % 3) as usize * 8;
                let units = (next() % 5 + 1) as usize;
                let end = cursor + units * 8;
                let initialized = cursor + (next() as usize % (units + 1)) * 8;
                buffers.push(buffer(cursor, end, initialized));
                cursor = end;
            }
            let used = cursor + (next() % 3) as usize * 8;
            let mut model = vec![true; used];
            for b in &buffers {
                for byte in &mut model[b.initialized.load(Ordering::Relaxed)..b.end] {
                    *byte = false;
                }
            }
            for i in (1..buffers.len()).rev() {
                let j = next() as usize % (i + 1);
                buffers.swap(i, j);
            }
            let mut expected = Vec::new();
            let mut i = 0;
            while i < used {
                if !model[i] {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < used && model[i] {
                    i += 1;
                }
                expected.push(start..i);
            }
            let snapshot = InitializedRanges::capture(used, &buffers, &allocator);
            assert_eq!(snapshot.as_ref(), expected);
            if snapshot.len() <= 3 {
                inline += 1;
                assert_eq!(counter.bytes(), 0);
            } else {
                backed += 1;
                assert!(counter.bytes() > 0);
            }
            assert_eq!(Arc::strong_count(&counter), 2);
            drop(snapshot);
            assert_eq!(counter.bytes(), 0);
        }
        assert!(inline > 0 && backed > 0);
    }

    #[test]
    fn collector_range_snapshot_invalid_metadata_unwind_releases_owner() {
        let counter = Arc::<AllocationCounter>::default();
        let cases = vec![
            vec![buffer(8, 16, 20)],
            vec![buffer(8, 16, 4)],
            vec![buffer(0, 40, 32)],
            vec![
                buffer(0, 24, 8),
                buffer(8, 32, 16),
                buffer(40, 56, 48),
                buffer(56, 64, 56),
            ],
        ];
        for buffers in cases {
            let used = if buffers.len() == 1 { 32 } else { 64 };
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    InitializedRanges::capture(used, &buffers, MetadataAllocator::new(&counter))
                }))
                .is_err()
            );
            assert_eq!(counter.bytes(), 0);
            assert_eq!(Arc::strong_count(&counter), 1);
        }
    }
}
