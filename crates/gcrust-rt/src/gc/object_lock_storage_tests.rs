use super::*;
use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe};
use std::sync::Arc;

#[test]
fn collector_object_lock_storage_tracks_actual_backing_and_warm_reuse() {
    let counter = Arc::<AllocationCounter>::default();
    let allocator = MetadataAllocator::new(&counter);
    allocation_probe::start();
    let locks = ObjectLocks::new_in(&allocator);
    let cold = allocation_probe::finish_bytes();
    assert_eq!(cold, (1, 0, std::mem::size_of::<[Mutex<()>; STRIPES]>(), 0));
    assert_eq!(counter.bytes(), cold.2);
    assert_eq!(counter.peak(), cold.2);
    allocation_probe::start();
    locks.prepare();
    let platform = allocation_probe::finish_bytes();
    // std::Mutex can separately own platform backend allocations. The array's
    // requested layout excludes those opaque controls; do not model their size.
    assert_eq!(counter.bytes(), cold.2);
    let words = [0u64; 2];
    allocation_probe::start();
    locks.prepare();
    {
        let _pair = locks.pair(words.as_ptr().cast(), words[1..].as_ptr().cast());
    }
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    allocation_probe::start();
    drop(locks);
    let released = allocation_probe::finish_bytes();
    assert_eq!(
        (released.0, released.2, released.3),
        (0, 0, cold.2 + platform.2 - platform.3)
    );
    assert_eq!(counter.bytes(), 0);
}

#[test]
fn collector_object_lock_storage_owners_release_independently() {
    let counter = Arc::<AllocationCounter>::default();
    let allocator = MetadataAllocator::new(&counter);
    let first = ObjectLocks::new_in(&allocator);
    let second = ObjectLocks::new_in(&allocator);
    let bytes = std::mem::size_of::<[Mutex<()>; STRIPES]>();
    assert_eq!(counter.bytes(), bytes * 2);
    drop(first);
    assert_eq!(counter.bytes(), bytes);
    let object = [0u64; 1];
    {
        let _guard = second.one(object.as_ptr().cast());
    }
    drop(second);
    assert_eq!(counter.bytes(), 0);
    assert_eq!(counter.peak(), bytes * 2);
}

#[test]
fn collector_object_lock_storage_counter_lifetime_and_unwind_preserve_locking() {
    let counter = Arc::<AllocationCounter>::default();
    let weak = Arc::downgrade(&counter);
    let allocator = MetadataAllocator::new(&counter);
    let locks = ObjectLocks::new_in(&allocator);
    let bytes = std::mem::size_of::<[Mutex<()>; STRIPES]>();
    drop(allocator);
    drop(counter);
    assert_eq!(weak.upgrade().unwrap().bytes(), bytes);
    let words = [0u64; 2];
    let left = words.as_ptr().cast();
    let right = words[1..].as_ptr().cast();
    let panic = std::panic::catch_unwind(|| {
        let _pair = locks.pair(left, right);
        panic!("unwind an object-access scope");
    });
    assert!(panic.is_err());
    {
        let _pair = locks.pair(right, left);
        assert!(locks.is_locked(left));
        assert!(locks.is_locked(right));
    }
    assert!(!locks.is_locked(left));
    assert!(!locks.is_locked(right));
    assert_eq!(weak.upgrade().unwrap().bytes(), bytes);
    drop(locks);
    assert!(weak.upgrade().is_none());
}
