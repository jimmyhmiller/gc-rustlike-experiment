use super::*;
use crate::{
    gc::{Full, Heap, ObjHeader, TypeInfo},
    runtime::{allocation_counter::AllocationCounter, allocation_probe},
};
use std::sync::Arc;
#[test]
fn collector_native_metadata_reference_matches_allocator_requests() {
    let counter = Arc::<AllocationCounter>::default();
    let allocator = MetadataAllocator::new(&counter);
    allocation_probe::start();
    let records = map::<u64>(257, allocator.clone());
    let owners = set(257, allocator.clone());
    let keys = vector::<usize>(257, allocator);
    let allocation = allocation_probe::finish_bytes();
    assert_eq!(allocation.0, 3);
    assert_eq!(counter.bytes(), allocation.2);
    assert_eq!(counter.peak(), allocation.2);
    allocation_probe::start();
    drop((records, owners, keys));
    let freed = allocation_probe::finish_bytes();
    assert_eq!(freed.1, 3);
    assert_eq!(freed.3, allocation.2);
    assert_eq!(counter.bytes(), 0);
}
#[test]
fn collector_native_metadata_reference_buffer_outlives_heap_without_retaining_it() {
    let heap = Arc::new(Heap::new::<Full>(
        8192,
        vec![TypeInfo::for_header(Full::SIZE)],
    ));
    let weak = Arc::downgrade(&heap);
    let counter = heap.native_memory.registry.clone();
    let buffer = vector::<u64>(257, MetadataAllocator::new(&counter));
    assert_eq!(heap.native_registry_bytes(), heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes() + 257 * 8);
    assert_eq!(heap.native_buffer_bytes(), 0);
    drop(heap);
    assert!(weak.upgrade().is_none());
    assert_eq!(counter.bytes(), 257 * 8);
    drop(buffer);
    assert_eq!(counter.bytes(), 0);
}
#[test]
fn collector_native_metadata_reference_metrics_keep_buffer_and_registry_scopes_separate() {
    let heap = Heap::new::<Full>(8192, vec![TypeInfo::for_header(Full::SIZE)]);
    let counter = heap.native_memory.registry.clone();
    let backing = vector::<u64>(257, MetadataAllocator::new(&counter));
    let heap_backing = heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes();
    let expected = heap_backing + 257 * 8;
    let text = heap.metrics_json();
    assert!(text.contains(&format!("\"native_registry_bytes\":{expected}")));
    assert!(text.contains(&format!("\"native_registry_peak_bytes\":{expected}")));
    assert!(text.contains("\"native_buffer_bytes\":0"));
    drop(backing);
    assert_eq!(heap.native_registry_bytes(), heap_backing);
    assert_eq!(heap.native_registry_peak_bytes(), expected);
}
