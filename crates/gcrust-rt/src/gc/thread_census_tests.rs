use super::*;
use crate::gc::Full;

struct CensusObservation {
    baseline: usize,
    state_backing: usize,
    worker_backing: usize,
    after_observer_drop_bytes: usize,
    before_bytes: usize,
    requested_before: usize,
    after_bytes: usize,
    requested_after: usize,
    after_capacity: usize,
    after_retirement_bytes: usize,
    after_drop_bytes: usize,
    identities_preserved: bool,
}
fn observe() -> CensusObservation {
    let heap = Heap::new::<Full>(4096, vec![]);
    let counter = heap.native_memory.registry.clone();
    let baseline = counter.bytes();
    let states: Vec<_> = (0..128).map(|_| heap.register_thread().0).collect();
    let requested_before =
        heap.threads.lock().unwrap().capacity() * std::mem::size_of::<crate::gc::ThreadStateHandle>();
    let state_backing = states.iter().map(|state| unsafe { state.native_backing_bytes() } + crate::gc::ThreadStateHandle::allocation_bytes()).sum::<usize>();
    let before_bytes = counter.bytes();
    for state in &states[3..] {
        unsafe {
            heap.safe_deregister_thread(state);
        }
    }
    let identities: Vec<_> = states[..3].iter().map(|state| state.trace_id()).collect();
    unsafe {
        heap.mutator_triggered_gc::<IdentityPtrPolicy>(&states[0]);
    }
    let after_bytes = counter.bytes();
    let worker_backing = heap.collector_worker_backing_bytes();
    let (after_capacity, identities_preserved) = {
        let census = heap.threads.lock().unwrap();
        (
            census.capacity(),
            census
                .iter()
                .all(|state| states[..3].iter().any(|old| crate::gc::ThreadStateHandle::ptr_eq(old, state)))
                && census
                    .iter()
                    .map(|state| state.trace_id())
                    .collect::<std::collections::BTreeSet<_>>()
                    == identities.into_iter().collect(),
        )
    };
    let requested_after = after_capacity * std::mem::size_of::<crate::gc::ThreadStateHandle>();
    for state in &states[..3] {
        unsafe {
            heap.safe_deregister_thread(state);
        }
    }
    let after_retirement_bytes = counter.bytes();
    drop(heap);
    // External state observers still exist, but census backing is Heap-owned.
    let after_drop_bytes = counter.bytes();
    drop(states);
    let after_observer_drop_bytes = counter.bytes();
    CensusObservation {
        baseline,
        state_backing,
        worker_backing,
        after_observer_drop_bytes,
        before_bytes,
        requested_before,
        after_bytes,
        requested_after,
        after_capacity,
        after_retirement_bytes,
        after_drop_bytes,
        identities_preserved,
    }
}
#[test]
fn collector_thread_census_backing_is_accounted_until_physical_release() {
    let observation = observe();
    assert_eq!(
        observation.before_bytes - observation.baseline - observation.state_backing,
        observation.requested_before
    );
    assert_eq!(
        observation.after_bytes - observation.baseline - observation.state_backing - observation.worker_backing,
        observation.requested_after
    );
    assert_eq!(observation.after_retirement_bytes, observation.baseline + observation.state_backing + observation.worker_backing);
    assert_eq!(observation.after_drop_bytes, observation.state_backing);
    assert_eq!(observation.after_observer_drop_bytes, 0);
    assert!(observation.identities_preserved);
}
#[test]
fn collector_thread_census_sparse_capacity_is_reclaimed_after_collection() {
    let observation = observe();
    assert!(
        observation.after_capacity <= 8,
        "three live states retained capacity {}",
        observation.after_capacity
    );
    assert!(observation.identities_preserved);
}
