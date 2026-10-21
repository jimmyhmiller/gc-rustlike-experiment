use super::*;
use crate::runtime::metadata_allocator::MetadataAllocator;
use std::sync::Arc;

#[test]
fn collector_satb_accounted_chunk_keeps_its_allocator_through_queue_and_iterator_handoff() {
    let counter = Arc::new(Default::default());
    let mut buffer = SATBBuffer::with_allocator(8, MetadataAllocator::new(&counter));
    buffer.log(11);
    buffer.log(22);
    let address = buffer.buf.as_ptr();
    assert_eq!(counter.bytes(), 8 * 8);
    let queue = SATBQueue::new();
    queue.flush_buffer(&mut buffer);
    assert!(buffer.is_empty());
    assert_eq!(counter.bytes(), 8 * 8);
    // A replacement allocation retains the same accounting owner while the
    // original allocation is still queued. Both physical backings are charged.
    buffer.log(33);
    assert_eq!(counter.bytes(), 8 * 8 + buffer.backing_bytes());
    drop(buffer);
    let mut chunks = queue.drain_chunks();
    drop(queue);
    assert_eq!(chunks[0].as_ptr(), address);
    assert_eq!(counter.bytes(), 8 * 8);
    let mut values = chunks.pop().unwrap().into_iter();
    assert_eq!(values.next(), Some(11));
    assert_eq!(counter.bytes(), 8 * 8);
    drop(values);
    assert_eq!(counter.bytes(), 0);
}

#[test]
fn collector_satb_public_snapshot_leaves_accounted_local_backing_with_its_owner() {
    let counter = Arc::new(Default::default());
    let mut buffer = SATBBuffer::with_allocator(8, MetadataAllocator::new(&counter));
    buffer.log(11);
    buffer.log(22);
    let snapshot = buffer.drain();
    assert_eq!(snapshot, [11, 22]);
    assert!(buffer.is_empty());
    assert_eq!(counter.bytes(), 8 * 8);
    drop(buffer);
    assert_eq!(counter.bytes(), 0);
    assert_eq!(snapshot, [11, 22]);
}

#[test]
fn collector_satb_public_snapshot_resets_configured_local_reservation() {
    let counter = Arc::new(Default::default());
    let mut buffer = SATBBuffer::with_allocator(8, MetadataAllocator::new(&counter));
    for value in 0..1024 { buffer.log(value); }
    assert!(counter.bytes() >= 1024 * 8);
    let snapshot = buffer.drain();
    assert_eq!(snapshot, (0..1024).collect::<Vec<_>>());
    assert!(buffer.is_empty());
    assert_eq!(buffer.backing_bytes(), 8 * 8);
    assert_eq!(counter.bytes(), 8 * 8);
    drop(buffer);
    assert_eq!(counter.bytes(), 0);
    assert_eq!(snapshot[1023], 1023);
}

#[test]
fn collector_satb_queue_accounts_public_backing_through_drained_iterator() {
    let counter = Arc::new(Default::default());
    let queue = SATBQueue::with_allocator(MetadataAllocator::new(&counter));
    let values = vec![11; 1024];
    let address = values.as_ptr();
    queue.push(values);
    let stats = queue.stats();
    assert_eq!(counter.bytes(), stats.pending_capacity_bytes + stats.chunk_metadata_bytes);
    assert_eq!(stats.pending_capacity_bytes, 8192);
    let mut chunks = queue.drain_chunks();
    assert_eq!(chunks[0].as_ptr(), address);
    let mut values = chunks.pop().unwrap().into_iter();
    drop(queue);
    assert_eq!(counter.bytes(), 8192 + stats.chunk_metadata_bytes);
    drop(chunks);
    assert_eq!(counter.bytes(), 8192);
    assert_eq!(values.next(), Some(11));
    drop(values);
    assert_eq!(counter.bytes(), 0);
}

#[test]
fn collector_satb_queue_accounts_bounded_bitmap_and_payload_lifetimes() {
    let counter = Arc::new(Default::default());
    let queue = SATBQueue::with_allocator(MetadataAllocator::new(&counter));
    let source = [0u64; 1024];
    let base = source.as_ptr();
    queue.begin_cycle::<crate::gc::IdentityPtrPolicy>(base.cast(), std::mem::size_of_val(&source));
    assert_eq!(counter.bytes(), queue.stats().bitmap_bytes);
    queue.push((0..1024).map(|i| unsafe { base.add(i) } as u64).collect());
    let stats = queue.stats();
    assert_eq!(stats.pending_values, 1024);
    assert_eq!(counter.bytes(), stats.bitmap_bytes + stats.pending_capacity_bytes + stats.chunk_metadata_bytes);
    // Duplicate publications retain no new storage or accounting charge.
    let charged = counter.bytes();
    queue.push((0..1024).map(|i| unsafe { base.add(i) } as u64).collect());
    assert_eq!(counter.bytes(), charged);
    let chunks = queue.drain_chunks();
    queue.finish_cycle();
    drop(queue);
    assert_eq!(counter.bytes(), stats.pending_capacity_bytes + stats.chunk_metadata_bytes);
    drop(chunks);
    assert_eq!(counter.bytes(), 0);
}
