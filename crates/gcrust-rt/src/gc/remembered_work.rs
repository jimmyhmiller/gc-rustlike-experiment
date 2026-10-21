//! Collection-scoped remembered objects, streamed without a task-vector copy.
use super::Work;
use crate::runtime::metadata_allocator::MetadataAllocator;
use std::{hash::RandomState, iter::FusedIterator};

#[derive(Default)]
pub(super) struct RememberedWork<'a> {
    objects: Option<hashbrown::hash_set::IntoIter<usize, &'a MetadataAllocator>>,
}
impl<'a> RememberedWork<'a> {
    pub(super) fn from_objects(
        objects: hashbrown::HashSet<usize, RandomState, &'a MetadataAllocator>,
    ) -> Self {
        Self {
            objects: Some(objects.into_iter()),
        }
    }
}
impl Iterator for RememberedWork<'_> {
    type Item = Work;
    fn next(&mut self) -> Option<Work> {
        self.objects.as_mut()?.next().map(Work::Object)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.objects.as_ref().map_or(0, ExactSizeIterator::len);
        (len, Some(len))
    }
}
impl ExactSizeIterator for RememberedWork<'_> {}
impl FusedIterator for RememberedWork<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe};
    use std::sync::Arc;

    #[test]
    fn collector_remembered_work_empty_allocates_nothing() {
        allocation_probe::start();
        let mut work = RememberedWork::default();
        assert_eq!(work.size_hint(), (0, Some(0)));
        assert_eq!(work.len(), 0);
        assert!(work.next().is_none());
        assert!(work.next().is_none());
        drop(work);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    }

    #[test]
    fn collector_remembered_work_stream_preserves_exact_size_and_backing_charge() {
        for count in [0, 1, 17, 4096] {
            let counter = Arc::<AllocationCounter>::default();
            let allocator = MetadataAllocator::new(&counter);
            let mut objects = hashbrown::HashSet::with_hasher_in(RandomState::new(), &allocator);
            allocation_probe::start();
            objects.reserve(count);
            let cold = allocation_probe::finish_bytes();
            assert_eq!(counter.bytes(), cold.2 - cold.3);
            for index in 0..count {
                assert!(objects.insert(index * 4096));
            }
            let bytes = counter.bytes();
            let mut seen = Vec::with_capacity(count);
            allocation_probe::start();
            let mut work = RememberedWork::from_objects(objects);
            for remaining in (1..=count).rev() {
                assert_eq!(work.size_hint(), (remaining, Some(remaining)));
                assert_eq!(work.len(), remaining);
                let Some(Work::Object(address)) = work.next() else {
                    panic!("missing object task")
                };
                seen.push(address);
            }
            assert_eq!(work.size_hint(), (0, Some(0)));
            assert!(work.next().is_none());
            assert!(work.next().is_none());
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(
                counter.bytes(),
                bytes,
                "exhausted owning iterator retains its backing until drop"
            );
            assert_eq!(Arc::strong_count(&counter), 2);
            seen.sort_unstable();
            assert!(seen.into_iter().eq((0..count).map(|index| index * 4096)));
            drop(work);
            assert_eq!(counter.bytes(), 0);
            assert_eq!(Arc::strong_count(&counter), 2);
        }
    }

    #[test]
    fn collector_remembered_work_partial_drop_releases_borrowed_backing() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let mut objects = hashbrown::HashSet::with_hasher_in(RandomState::new(), &allocator);
        objects.extend(0..257);
        let mut work = RememberedWork::from_objects(objects);
        for _ in 0..3 {
            assert!(matches!(work.next(), Some(Work::Object(_))));
        }
        assert_eq!(work.len(), 254);
        assert!(counter.bytes() > 0);
        assert_eq!(Arc::strong_count(&counter), 2);
        drop(work);
        assert_eq!(counter.bytes(), 0);
        drop(allocator);
        assert_eq!(Arc::strong_count(&counter), 1);
    }
}
