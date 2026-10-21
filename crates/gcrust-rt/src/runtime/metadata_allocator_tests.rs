use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;
#[test]
fn collector_metadata_allocator_reference_layout_alignment_and_zeroed() {
    let counter = Arc::<AllocationCounter>::default();
    let allocator = MetadataAllocator::new(&counter);
    for (size, align) in [(0, 1), (0, 256), (1, 1), (17, 8), (257, 256)] {
        let layout = Layout::from_size_align(size, align).unwrap();
        let block = allocator.allocate_zeroed(layout).unwrap();
        assert_eq!(block.len(), size);
        assert_eq!(block.as_ptr().cast::<u8>().addr() % align, 0);
        assert_eq!(counter.bytes(), size);
        for i in 0..size {
            assert_eq!(unsafe { block.as_ptr().cast::<u8>().add(i).read() }, 0);
        }
        unsafe {
            allocator.deallocate(block.cast(), layout);
        }
        assert_eq!(counter.bytes(), 0);
    }
    assert!(counter.peak() >= 257);
}
#[test]
fn collector_metadata_allocator_reference_grow_shrink_preserve_data_and_counts() {
    let counter = Arc::<AllocationCounter>::default();
    let allocator = MetadataAllocator::new(&counter);
    let small = Layout::from_size_align(17, 8).unwrap();
    let large = Layout::from_size_align(257, 8).unwrap();
    let block = allocator.allocate(small).unwrap();
    unsafe {
        block.as_ptr().cast::<u8>().write_bytes(42, 17);
    }
    let block = unsafe { allocator.grow(block.cast(), small, large) }.unwrap();
    assert_eq!(counter.bytes(), 257);
    for i in 0..17 {
        assert_eq!(unsafe { block.as_ptr().cast::<u8>().add(i).read() }, 42);
    }
    let block = unsafe { allocator.shrink(block.cast(), large, small) }.unwrap();
    assert_eq!(counter.bytes(), 17);
    for i in 0..17 {
        assert_eq!(unsafe { block.as_ptr().cast::<u8>().add(i).read() }, 42);
    }
    unsafe {
        allocator.deallocate(block.cast(), small);
    }
    assert_eq!(counter.bytes(), 0);
}
#[derive(Clone)]
struct Failing;
// SAFETY: no allocation succeeds, so no valid deallocation can reach this base.
unsafe impl Allocator for Failing {
    fn allocate(&self, _: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Err(AllocError)
    }
    unsafe fn deallocate(&self, _: NonNull<u8>, _: Layout) {
        unreachable!()
    }
}
#[test]
fn collector_metadata_allocator_reference_failure_leaves_no_charge() {
    let counter = Arc::<AllocationCounter>::default();
    let allocator = MetadataAllocator {
        counter: Some(counter.clone()),
        base: Failing,
    };
    assert!(allocator.allocate(Layout::new::<u64>()).is_err());
    assert_eq!(counter.bytes(), 0);
    assert_eq!(counter.peak(), 0);
}
#[derive(Clone)]
struct Counting {
    allocated: Arc<AtomicUsize>,
    freed: Arc<AtomicUsize>,
}
// SAFETY: forwards identical blocks/layouts to Global; counters do not touch them.
unsafe impl Allocator for Counting {
    fn allocate(&self, l: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let b = Global.allocate(l)?;
        self.allocated.fetch_add(1, Ordering::Relaxed);
        Ok(b)
    }
    unsafe fn deallocate(&self, p: NonNull<u8>, l: Layout) {
        unsafe {
            Global.deallocate(p, l);
        }
        self.freed.fetch_add(1, Ordering::Relaxed);
    }
}
#[test]
fn collector_metadata_allocator_reference_overflow_frees_unpublished_allocation() {
    let counter = Arc::<AllocationCounter>::default();
    assert!(counter.try_charge(usize::MAX));
    let allocated = Arc::new(AtomicUsize::new(0));
    let freed = Arc::new(AtomicUsize::new(0));
    let allocator = MetadataAllocator {
        counter: Some(counter.clone()),
        base: Counting {
            allocated: allocated.clone(),
            freed: freed.clone(),
        },
    };
    assert!(allocator.allocate(Layout::new::<u64>()).is_err());
    assert_eq!(allocated.load(Ordering::Relaxed), 1);
    assert_eq!(freed.load(Ordering::Relaxed), 1);
    assert_eq!(counter.bytes(), usize::MAX);
    counter.release(usize::MAX);
    assert_eq!(counter.bytes(), 0);
}
struct Paused {
    ready: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}
// SAFETY: forwards identical blocks/layouts; the pause only delays physical free.
unsafe impl Allocator for Paused {
    fn allocate(&self, l: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Global.allocate(l)
    }
    unsafe fn deallocate(&self, p: NonNull<u8>, l: Layout) {
        self.ready.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        unsafe {
            Global.deallocate(p, l);
        }
    }
}
#[test]
fn collector_metadata_allocator_reference_charge_survives_until_physical_free() {
    let counter = Arc::<AllocationCounter>::default();
    let (ready, waiting) = mpsc::channel();
    let (release, go) = mpsc::channel();
    let allocator = MetadataAllocator {
        counter: Some(counter.clone()),
        base: Paused {
            ready,
            release: Mutex::new(go),
        },
    };
    let values = allocator_api2::vec::Vec::<u8, _>::with_capacity_in(1024, allocator);
    let worker = std::thread::spawn(move || drop(values));
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let observed = counter.bytes();
    release.send(()).unwrap();
    worker.join().unwrap();
    assert_eq!(observed, 1024);
    assert_eq!(counter.bytes(), 0);
}
#[test]
fn collector_metadata_allocator_reference_containers_and_clones_release_backing() {
    let counter = Arc::<AllocationCounter>::default();
    let allocator = MetadataAllocator::new(&counter);
    let mut vector = allocator_api2::vec::Vec::with_capacity_in(257, allocator.clone());
    let mut map = hashbrown::HashMap::with_capacity_and_hasher_in(
        257,
        std::collections::hash_map::RandomState::new(),
        allocator.clone(),
    );
    let mut set = hashbrown::HashSet::with_capacity_and_hasher_in(
        257,
        std::collections::hash_map::RandomState::new(),
        allocator,
    );
    for i in 0..257 {
        vector.push(i);
        map.insert(i, i + 42);
        set.insert(i);
    }
    let before = counter.bytes();
    assert!(before > 257 * std::mem::size_of::<usize>());
    let copied = vector.clone();
    assert!(counter.bytes() > before);
    drop(copied);
    assert_eq!(counter.bytes(), before);
    for i in 0..257 {
        assert_eq!(vector[i], i);
        assert_eq!(map[&i], i + 42);
        assert!(set.contains(&i));
    }
    drop((vector, map, set));
    assert_eq!(counter.bytes(), 0);
    assert!(counter.peak() >= before);
}
#[test]
fn collector_metadata_allocator_reference_concurrent_ownership_is_not_lost() {
    let counter = Arc::<AllocationCounter>::default();
    let gate = Arc::new(std::sync::Barrier::new(5));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let counter = counter.clone();
        let gate = gate.clone();
        workers.push(std::thread::spawn(move || {
            let values = allocator_api2::vec::Vec::<u64, _>::with_capacity_in(
                257,
                MetadataAllocator::new(&counter),
            );
            gate.wait();
            gate.wait();
            drop(values);
        }));
    }
    gate.wait();
    assert_eq!(counter.bytes(), 4 * 257 * 8);
    assert!(counter.peak() >= counter.bytes());
    gate.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(counter.bytes(), 0);
}

// This base only supports zeroed allocation: falling back to allocate is observable.
struct ZeroedOnly {
    freed: AtomicUsize,
}
// SAFETY: successful allocations and all frees use Global with identical layouts.
unsafe impl Allocator for ZeroedOnly {
    fn allocate(&self, _: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Err(AllocError)
    }
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Global.allocate_zeroed(layout)
    }
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { Global.deallocate(ptr, layout) };
        self.freed.fetch_add(1, Ordering::Relaxed);
    }
}
#[test]
fn collector_metadata_allocator_reference_zeroed_base_path_and_overflow_cleanup() {
    let counter = Arc::<AllocationCounter>::default();
    let allocator = MetadataAllocator {
        counter: Some(counter.clone()),
        base: ZeroedOnly {
            freed: AtomicUsize::new(0),
        },
    };
    let layout = Layout::from_size_align(257, 256).unwrap();
    let block = allocator.allocate_zeroed(layout).unwrap();
    assert_eq!(counter.bytes(), layout.size());
    assert_eq!(block.cast::<u8>().as_ptr().addr() % layout.align(), 0);
    assert!(
        unsafe { std::slice::from_raw_parts(block.cast::<u8>().as_ptr(), layout.size()) }
            .iter()
            .all(|byte| *byte == 0)
    );
    unsafe { allocator.deallocate(block.cast(), layout) };
    assert_eq!(counter.bytes(), 0);
    assert_eq!(allocator.base.freed.load(Ordering::Relaxed), 1);
    assert!(counter.try_charge(usize::MAX));
    assert!(allocator.allocate_zeroed(layout).is_err());
    assert_eq!(counter.bytes(), usize::MAX);
    assert_eq!(allocator.base.freed.load(Ordering::Relaxed), 2);
    counter.release(usize::MAX);

    let failing = MetadataAllocator {
        counter: Some(counter.clone()),
        base: Failing,
    };
    assert!(failing.allocate_zeroed(layout).is_err());
    assert_eq!(counter.bytes(), 0);
}

#[test]
fn collector_metadata_empty_and_consumed_hashmap_release_allocator_owner() {
    for capacity in [0, 1, 17] {
        for consume in [false, true] {
            let counter = Arc::<AllocationCounter>::default();
            let weak = Arc::downgrade(&counter);
            let mut map = hashbrown::HashMap::with_capacity_and_hasher_in(
                capacity, std::collections::hash_map::RandomState::new(), MetadataAllocator::new(&counter),
            );
            for index in 0..capacity { map.insert(index, index + 1); }
            let mut iter = map.into_iter();
            if consume { assert_eq!(iter.by_ref().count(), capacity); } else { iter.next(); }
            drop(iter);
            assert_eq!(counter.bytes(), 0);
            assert_eq!(Arc::strong_count(&counter), 1, "HashMap iterator leaked its allocator owner at capacity {capacity}");
            drop(counter);
            assert!(weak.upgrade().is_none());
        }
    }
}

#[test]
fn collector_metadata_empty_and_consumed_hashset_release_allocator_owner() {
    for capacity in [0, 1, 17] {
        for consume in [false, true] {
            let counter = Arc::<AllocationCounter>::default();
            let weak = Arc::downgrade(&counter);
            let mut set = hashbrown::HashSet::with_capacity_and_hasher_in(
                capacity, std::collections::hash_map::RandomState::new(), MetadataAllocator::new(&counter),
            );
            for index in 0..capacity { set.insert(index); }
            let mut iter = set.into_iter();
            if consume { assert_eq!(iter.by_ref().count(), capacity); } else { iter.next(); }
            drop(iter);
            assert_eq!(counter.bytes(), 0);
            assert_eq!(Arc::strong_count(&counter), 1, "HashSet iterator leaked its allocator owner at capacity {capacity}");
            drop(counter);
            assert!(weak.upgrade().is_none());
        }
    }
}
