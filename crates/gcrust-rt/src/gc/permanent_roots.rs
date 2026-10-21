//! Grow permanent-source storage without pinning collector progress.
use super::{Heap, RootSource, ScopedExtraSource, abort_on_collection_panic};
use crate::runtime::{
    allocation_counter::AllocationCounter, metadata_allocator::MetadataAllocator,
};
use std::sync::{Arc, Mutex};
pub(super) type Sources = allocator_api2::vec::Vec<*const dyn RootSource, MetadataAllocator>;

pub(super) fn new_registry(counter: &Arc<AllocationCounter>) -> Mutex<Sources> {
    let registry = Mutex::new(Sources::new_in(MetadataAllocator::new(counter)));
    drop(registry.lock().unwrap());
    registry
}
fn request(sources: &Sources) -> usize {
    if sources.capacity() > sources.len() {
        return 0;
    }
    sources
        .len()
        .checked_add(1)
        .and_then(|n| n.checked_mul(2))
        .expect("permanent root registry capacity overflow")
}
fn commit(sources: &mut Sources, staged: &mut Sources, source: *const dyn RootSource) -> bool {
    assert!(staged.is_empty());
    if sources.capacity() <= sources.len() {
        if staged.capacity() <= sources.len()
            || !sources.allocator().same_counter(staged.allocator())
        {
            return false;
        }
        staged.extend(sources.drain(..));
        std::mem::swap(sources, staged);
    }
    sources.push(source);
    true
}

/// # Safety
/// The source and its slots satisfy Heap::register_permanent_extra's complete
/// lifetime/publication contract, including frame publication before parking.
pub(super) unsafe fn register(heap: &Heap, source: *const dyn RootSource) {
    {
        let mut sources = heap.permanent_extras.lock().unwrap();
        if sources.capacity() > sources.len() {
            sources.push(source);
            return;
        }
    }
    let roots: &[&dyn RootSource] = &[unsafe { &*source }];
    let pending = std::pin::pin!(ScopedExtraSource::new(roots));
    // This bridge ends in permanent publication. It must not mark future
    // snapshots incomplete as externally supplied per-call roots can do.
    let mut pending = unsafe { heap.scoped_extras.register(pending.as_ref()) };
    let mut callers = heap.suspend_current_registrations();
    abort_on_collection_panic(|| {
        loop {
            let (capacity, allocator) = {
                let sources = heap.permanent_extras.lock().unwrap();
                (request(&sources), sources.allocator().clone())
            };
            #[cfg(test)]
            if capacity != 0 {
                tests::checkpoint(heap, tests::Phase::Allocate);
            }
            let mut staged = Sources::with_capacity_in(capacity, allocator);
            let coordinator = heap.lock_coordinator();
            let mut sources = heap.permanent_extras.lock().unwrap();
            if !commit(&mut sources, &mut staged, source) {
                drop(sources);
                drop(coordinator);
                drop(staged);
                continue;
            }
            drop(sources);
            // Do not enumerate these same slots twice during the handoff. Exclusion
            // also completes every write that borrowed the temporary registration.
            drop(pending.take());
            drop(coordinator);
            #[cfg(test)]
            if staged.capacity() != 0 {
                tests::checkpoint(heap, tests::Phase::Dispose);
            }
            drop(staged);
            // Native disposal permits another collection. Restore all originally
            // RUNNING registrations together only after every forwarding write.
            let _coordinator = heap.lock_coordinator();
            callers.restore();
            break;
        }
    });
}

#[cfg(test)]
#[path = "permanent_roots_tests.rs"]
mod tests;
