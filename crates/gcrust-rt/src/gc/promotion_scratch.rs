// Collection-scoped marking survives through all combined-pause consumers.
// Borrow the Heap's cached allocator so backing is charged without extra owner
// clones and cannot outlive the Heap that supplies the arena addresses.
pub(crate) type NurseryLive<'a> = hashbrown::HashSet<
    usize,
    std::hash::RandomState,
    &'a crate::runtime::metadata_allocator::MetadataAllocator,
>;
pub(super) type PromotionPending<'a> = allocator_api2::vec::Vec<
    *mut u8,
    &'a crate::runtime::metadata_allocator::MetadataAllocator,
>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe, metadata_allocator::MetadataAllocator};
    use std::{hash::RandomState, sync::Arc};

    #[test]
    fn collector_promotion_scratch_empty_borrows_without_allocating() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let hasher = RandomState::new();
        allocation_probe::start();
        let live = NurseryLive::with_hasher_in(hasher, &allocator);
        let pending = PromotionPending::new_in(&allocator);
        drop((live, pending));
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 2);
    }

    #[test]
    fn collector_promotion_scratch_charges_actual_backing_through_mark_handoff() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let mut live = NurseryLive::with_hasher_in(RandomState::new(), &allocator);
        let mut pending = PromotionPending::new_in(&allocator);
        let storage = [0u8; 257];
        allocation_probe::start();
        for index in 0..storage.len() {
            assert!(live.insert(index));
            pending.push(storage.as_ptr().wrapping_add(index).cast_mut());
        }
        let traffic = allocation_probe::finish_bytes();
        assert_eq!(counter.bytes(), traffic.2 - traffic.3);
        assert!(counter.bytes() > pending.capacity() * std::mem::size_of::<*mut u8>());
        assert_eq!(Arc::strong_count(&counter), 2);
        // LIFO traversal remains independent of randomized set iteration.
        for index in (0..storage.len()).rev() {
            assert_eq!(pending.pop(), Some(storage.as_ptr().wrapping_add(index).cast_mut()));
        }
        let both = counter.bytes();
        let vector_bytes = pending.capacity() * std::mem::size_of::<*mut u8>();
        drop(pending);
        assert_eq!(counter.bytes(), both - vector_bytes);
        // Returning marking state to the major collector keeps its backing
        // charged, including capacity retained after filtering nursery members.
        live.retain(|index| index % 2 == 0);
        assert_eq!(counter.bytes(), both - vector_bytes);
        assert_eq!(live.len(), 129);
        assert!((0..257).all(|index| live.contains(&index) == (index % 2 == 0)));
        drop(live);
        assert_eq!(counter.bytes(), 0);
    }

    #[test]
    fn collector_promotion_scratch_unwind_releases_both_buffers() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let result = std::panic::catch_unwind(|| {
            let mut live = NurseryLive::with_hasher_in(RandomState::new(), &allocator);
            let mut pending = PromotionPending::new_in(&allocator);
            let object = 0u8;
            for index in 0..257 {
                live.insert(index);
                pending.push((&object as *const u8).cast_mut());
            }
            assert!(counter.bytes() > 0);
            panic!("marking fixture unwind");
        });
        assert!(result.is_err());
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 2);
    }
}
