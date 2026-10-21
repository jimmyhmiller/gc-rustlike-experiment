//! Release execution-owned registry storage outside collector-visible locks.
use super::{
    AtomicCellRegistry, ChannelRegistry, CompletionRegistry, Heap,
    native_metadata::MetadataAllocator,
};
use std::sync::Mutex;

fn detach<T>(registry: &Mutex<T>, replacement: T) -> T {
    std::mem::replace(&mut *registry.lock().unwrap(), replacement)
}

/// # Safety
/// Execution children/reaper have fully joined and this caller's registrations
/// are BLOCKED. Managed entry cannot resume until all detached records retire.
/// Borrowed native handles from the closing execution must no longer be used.
pub(super) unsafe fn clear(heap: &Heap) {
    // Empty replacements retain this heap's accounting owner. Prepare their
    // hasher/allocator state outside registry locks, before detaching anything.
    let allocator = MetadataAllocator::new(&heap.native_memory.registry);
    let completions = CompletionRegistry::with_allocator(allocator.clone());
    let atomics = AtomicCellRegistry::with_allocator(allocator.clone());
    let channels = ChannelRegistry::with_allocator(allocator);
    #[cfg(test)]
    tests::checkpoint(heap, tests::Phase::BeforeDetach);
    // Registry enumeration can finish before parallel forwarding writes. Wait
    // through the collector's complete lifetime before unpublishing its slots.
    let (mut completions, mut atomics, mut channels) = unsafe {
        heap.with_native_registry_exclusion(|| {
            (
                detach(&heap.thread_completions, completions),
                detach(&heap.atomic_cells, atomics),
                detach(&heap.channels, channels),
            )
        })
    };
    #[cfg(test)]
    tests::checkpoint(heap, tests::Phase::AfterDetach);
    // clear transfers records to their prepared retirement queues, then frees
    // the detached backing. No registry/coordinator lock spans either action.
    completions.clear();
    atomics.clear();
    channels.clear();
}

#[cfg(test)]
#[path = "registry_teardown_tests.rs"]
mod tests;
