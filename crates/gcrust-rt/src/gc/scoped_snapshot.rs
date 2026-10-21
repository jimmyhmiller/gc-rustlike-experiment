//! Immutable collection-scoped owners/epochs. Inline storage avoids allocation
//! for small censuses; larger backing belongs to the collector's allocator.
use allocator_api2::{alloc::Allocator, vec::Vec as NativeVec};
use std::iter::FusedIterator;

pub(super) enum ScopedSnapshot<T, A: Allocator> {
    Inline { values: [Option<T>; 4], len: usize },
    Backed(NativeVec<T, A>),
}
impl<T, A: Allocator> ScopedSnapshot<T, A> {
    pub(super) fn capture(values: impl IntoIterator<Item = T>, allocator: A) -> Self {
        let mut source = values.into_iter();
        let lower = source.size_hint().0;
        if lower > 4 {
            let mut values = NativeVec::with_capacity_in(lower, allocator);
            values.extend(source);
            return Self::Backed(values);
        }
        let mut values = std::array::from_fn(|_| None);
        let mut len = 0;
        while let Some(value) = source.next() {
            if len == 4 {
                let mut backing = NativeVec::with_capacity_in(8, allocator);
                for value in &mut values {
                    backing.push(value.take().expect("initialized snapshot prefix"));
                }
                backing.push(value);
                backing.extend(source);
                return Self::Backed(backing);
            }
            values[len] = Some(value);
            len += 1;
        }
        Self::Inline { values, len }
    }
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => *len,
            Self::Backed(v) => v.len(),
        }
    }
    pub(super) fn iter(&self) -> Iter<'_, T> {
        match self {
            Self::Inline { values, len } => Iter::Inline(values[..*len].iter()),
            Self::Backed(v) => Iter::Backed(v.iter()),
        }
    }
}
pub(super) enum Iter<'a, T> {
    Inline(std::slice::Iter<'a, Option<T>>),
    Backed(std::slice::Iter<'a, T>),
}
impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Inline(v) => v
                .next()
                .map(|v| v.as_ref().expect("initialized snapshot prefix")),
            Self::Backed(v) => v.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Inline(v) => v.size_hint(),
            Self::Backed(v) => v.size_hint(),
        }
    }
}
impl<T> ExactSizeIterator for Iter<'_, T> {}
impl<T> FusedIterator for Iter<'_, T> {}
impl<'a, T, A: Allocator> IntoIterator for &'a ScopedSnapshot<T, A> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;
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
    use std::sync::Arc;
    #[test]
    fn collector_thread_snapshot_inline_preserves_owners_without_allocating() {
        for count in 0..=4 {
            let counter = Arc::<AllocationCounter>::default();
            let allocator = MetadataAllocator::new(&counter);
            let source: Vec<_> = (0..count).map(Arc::new).collect();
            let owner_count = Arc::strong_count(&counter);
            allocation_probe::start();
            let snapshot = ScopedSnapshot::capture(source.iter().cloned(), &allocator);
            assert_eq!(snapshot.len(), count);
            for (index, value) in snapshot.iter().enumerate() {
                assert_eq!(**value, index);
                assert_eq!(Arc::strong_count(value), 2);
            }
            drop(snapshot);
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(Arc::strong_count(&counter), owner_count);
            assert!(source.iter().all(|v| Arc::strong_count(v) == 1));
        }
    }
    #[test]
    fn collector_thread_snapshot_backing_lives_with_last_snapshot_owner() {
        for count in [5, 7, 64] {
            let counter = Arc::<AllocationCounter>::default();
            let allocator = MetadataAllocator::new(&counter);
            let source: Vec<_> = (0..count).map(Arc::new).collect();
            let weak: Vec<_> = source.iter().map(Arc::downgrade).collect();
            let owners = Arc::strong_count(&counter);
            allocation_probe::start();
            let snapshot = ScopedSnapshot::capture(source.iter().cloned(), &allocator);
            let observed = allocation_probe::finish_bytes();
            assert_eq!(observed.0, 1);
            assert_eq!(observed.1, 0);
            assert_eq!(counter.bytes(), observed.2);
            assert_eq!(observed.3, 0);
            drop(source);
            assert_eq!(snapshot.len(), count);
            assert!(
                snapshot
                    .iter()
                    .enumerate()
                    .all(|(i, v)| **v == i && Arc::strong_count(v) == 1)
            );
            assert_eq!(Arc::strong_count(&counter), owners);
            drop(snapshot);
            assert_eq!(counter.bytes(), 0);
            assert!(weak.iter().all(|v| v.upgrade().is_none()));
        }
    }
    #[test]
    fn collector_thread_snapshot_filtered_and_epoch_iterators_preserve_order() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        for count in 0..=65 {
            let snapshot = ScopedSnapshot::capture((0..count).filter(|i| i % 2 == 0), &allocator);
            let expected: Vec<_> = (0..count).filter(|i| i % 2 == 0).collect();
            assert_eq!(snapshot.len(), expected.len());
            let mut iter = snapshot.iter();
            for (i, value) in expected.iter().enumerate() {
                assert_eq!(iter.len(), expected.len() - i);
                assert_eq!(iter.next(), Some(value));
            }
            assert!(iter.next().is_none());
            assert!(iter.next().is_none());
            drop(snapshot);
            assert_eq!(counter.bytes(), 0);
        }
        allocation_probe::start();
        let epochs = ScopedSnapshot::capture((0..4).map(|i| i as u64 + 100), &allocator);
        assert!(epochs.iter().copied().eq(100..104));
        drop(epochs);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    }
    #[test]
    fn collector_thread_snapshot_iterator_unwind_releases_partial_owners() {
        for exact in [false, true] {
            for panic_at in [0, 3, 4, 5, 9] {
                let counter = Arc::<AllocationCounter>::default();
                let allocator = MetadataAllocator::new(&counter);
                let source: Vec<_> = (0..10).map(Arc::new).collect();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let iter = source.iter().enumerate().map(|(i, v)| {
                        assert_ne!(i, panic_at, "injected iterator panic");
                        v.clone()
                    });
                    if exact {
                        drop(ScopedSnapshot::capture(iter, &allocator));
                    } else {
                        drop(ScopedSnapshot::capture(iter.filter(|_| true), &allocator));
                    }
                }));
                assert!(result.is_err());
                assert_eq!(counter.bytes(), 0);
                assert!(source.iter().all(|v| Arc::strong_count(v) == 1));
            }
        }
    }
}
