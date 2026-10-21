//! Moving-GC pressure with an independently maintained mutation ledger.
use super::*;
use crate::gc::{ObjHeader, PtrPolicy};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};

#[test]
fn collector_concurrent_allocation_pressure_preserves_slot_ledger() {
    const CHILD: &str = "GCR_CONCURRENT_ALLOCATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        for mode in ["semi", "generational"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::concurrent_allocation_tests::collector_concurrent_allocation_pressure_preserves_slot_ledger", "--nocapture"])
            .env(CHILD, mode).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(45);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!(
                        "allocation pressure hung: {:?}",
                        child.wait_with_output().unwrap()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if line.starts_with("allocation pressure:") {
                    println!("{line}");
                }
            }
        }
        return;
    }
    let mode = std::env::var(CHILD).unwrap();
    assert!(["semi", "generational"].contains(&mode.as_str()));
    let generational = mode == "generational";
    let seed = std::env::var("GCR_CONCURRENT_ALLOCATION_SEED")
        .map(|value| value.parse::<u64>().expect("invalid pressure seed"))
        .unwrap_or(0x9e3779b97f4a7c15);
    assert_ne!(seed, 0, "xorshift pressure seed must be nonzero");
    const SLOTS: usize = 32;
    const CYCLES: usize = 256;
    const ALLOCATIONS: usize = 8192;
    static HEAP: AtomicPtr<Heap> = AtomicPtr::new(std::ptr::null_mut());
    static SCANNING: AtomicBool = AtomicBool::new(false);
    static STARTED: AtomicBool = AtomicBool::new(false);
    struct YieldingIdentity;
    impl PtrPolicy for YieldingIdentity {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            let pointer = IdentityPtrPolicy::try_decode_ptr(bits);
            if pointer.is_some() {
                let heap = HEAP.load(Ordering::Acquire);
                if !heap.is_null() {
                    let heap = unsafe { &*heap };
                    if heap.barriers_active() && !heap.gc_requested() {
                        SCANNING.store(true, Ordering::Release);
                        // Only the first mutation rendezvous waits. The
                        // prefilled graph leaves room for that one allocation;
                        // no collector/access lock is held here. Subsequent
                        // pressure cycles never pin the coordinator.
                        while !STARTED.load(Ordering::Acquire) {
                            std::thread::yield_now();
                        }
                        // Enlarge the real copy phase without pinning the
                        // coordinator: allocation may need its lock to finish.
                        std::thread::yield_now();
                    }
                }
            }
            pointer
        }
        fn encode_ptr(pointer: *mut u8) -> u64 {
            IdentityPtrPolicy::encode_ptr(pointer)
        }
    }
    let array_info = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
    let leaf_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    let (ready, receive) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let worker_stop = stop.clone();
    let worker_active = active.clone();
    let worker_array_info = array_info.clone();
    let worker_leaf_info = leaf_info.clone();
    let worker = std::thread::spawn(move || {
        let array_info = worker_array_info;
        let leaf_info = worker_leaf_info;
        let mut runtime = if generational {
            RuntimeContext::new_generational(1024, 4096, vec![array_info.clone(), leaf_info.clone()])
        } else {
            RuntimeContext::new(4096, vec![array_info.clone(), leaf_info.clone()])
        };
        let heap = runtime.heap().clone();
        let thread = runtime.thread_ptr();
        let execute = || {
            let array = unsafe { heap.alloc_obj::<Full>(&array_info, SLOTS) };
            assert!(!array.is_null());
            let initial = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
            assert!(!initial.is_null());
            unsafe {
                initial.add(Full::SIZE).cast::<u64>().write(0);
                for index in 0..SLOTS {
                    array
                        .add(array_info.varlen_element_offset(index))
                        .cast::<*mut u8>()
                        .write(initial);
                }
            }
            let root = unsafe { heap.globals.add(array as u64) };
            ready.send((heap.clone(), root)).unwrap();
            // Admit the first mutation only after a real post-pause scan begins.
            // The first publication fits without requesting another collection.
            while !SCANNING.load(Ordering::Acquire) {
                if heap.gc_requested() {
                    unsafe {
                        ai_gc_pollcheck_slow(thread);
                    }
                }
                std::thread::yield_now();
            }
            let mut ledger = [Some(0); SLOTS];
            let mut sequence = 0u64;
            let mut random = seed;
            while !worker_stop.load(Ordering::Acquire) || sequence < ALLOCATIONS as u64 {
                if heap.gc_requested() {
                    unsafe {
                        ai_gc_pollcheck_slow(thread);
                    }
                }
                let during_scan =
                    SCANNING.load(Ordering::Acquire) && heap.barriers_active() && !heap.gc_requested();
                let leaf = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
                sequence += 1;
                unsafe {
                    leaf.add(Full::SIZE).cast::<u64>().write(sequence);
                }
                // Allocation may park behind a concurrent cycle or trigger its
                // own collection. Reload the owner only after it returns.
                let array = heap.globals.get(root) as *mut u8;
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let index = random as usize % SLOTS;
                let offset = array_info.varlen_element_offset(index);
                let old = unsafe { slots::ai_slot_bits64_load(thread, array, offset) } as *mut u8;
                if let Some(expected) = ledger[index] {
                    assert_eq!(
                        unsafe { slots::ai_slot_bits64_load(thread, old, Full::SIZE) },
                        expected
                    );
                } else {
                    assert!(old.is_null());
                }
                let state = unsafe { &*(*thread).dyna_thread };
                assert!(
                    unsafe { heap.compare_exchange_reference(state, array, offset, old, leaf) }.is_ok()
                );
                ledger[index] = Some(sequence);
                assert!(
                    unsafe { heap.compare_exchange_reference(state, array, offset, array, old) }
                        .is_err()
                );
                let duplicate = (index + 7) % SLOTS;
                unsafe {
                    slots::ai_slot_ref_store(
                        thread,
                        array,
                        array_info.varlen_element_offset(duplicate),
                        leaf,
                    );
                }
                ledger[duplicate] = Some(sequence);
                if during_scan {
                    worker_active.fetch_add(1, Ordering::Relaxed);
                    STARTED.store(true, Ordering::Release);
                }
            }
            (ledger, sequence)
        };
        unsafe { crate::runtime::with_current_thread(thread, execute) }
    });
    let (heap, root) = receive.recv().unwrap();
    HEAP.store(Arc::as_ptr(&heap).cast_mut(), Ordering::Release);
    for _ in 0..CYCLES {
        SCANNING.store(false, Ordering::Release);
        unsafe {
            heap.concurrent_collect::<YieldingIdentity>();
        }
    }
    HEAP.store(std::ptr::null_mut(), Ordering::Release);
    stop.store(true, Ordering::Release);
    let (ledger, sequence) = worker.join().unwrap();
    assert!(sequence >= ALLOCATIONS as u64);
    assert!(
        active.load(Ordering::Acquire) > 0,
        "no mutation interval overlapped actual concurrent scanning"
    );
    if generational {
        assert!(
            heap.minor_collections() > 0,
            "pressure did not exercise automatic nursery collection"
        );
        // After retirement there is no owning runtime Thread. Register the
        // coordinator to promote remaining rooted young objects before the
        // final old-space census, without retaining derived pointers.
        let (state, _) = heap.register_thread();
        unsafe {
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state);
            heap.safe_deregister_thread(&state);
        }
        assert_eq!(heap.nursery_used(), 0);
    }
    // Reclaim SATB floating garbage after the worker retires. Only the exact
    // graph described by the independent native ledger may now survive.
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    let array = heap.globals.get(root) as *const u8;
    let mut pointers = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for (index, expected) in ledger.into_iter().enumerate() {
        let leaf = unsafe {
            array
                .add(array_info.varlen_element_offset(index))
                .cast::<*const u8>()
                .read()
        };
        match expected {
            None => assert!(leaf.is_null()),
            Some(id) => {
                assert!(heap.from_space_contains(leaf));
                assert_eq!(unsafe { leaf.add(Full::SIZE).cast::<u64>().read() }, id);
                if let Some(previous) = pointers.insert(id, leaf) {
                    assert_eq!(leaf, previous);
                }
                ids.insert(id);
            }
        }
    }
    assert_eq!(
        heap.from_used(),
        array_info.allocation_size(SLOTS) + ids.len() * leaf_info.allocation_size(0)
    );
    assert!(heap.collections() >= CYCLES + 1);
    assert!(!heap.gc_requested());
    println!(
        "allocation pressure: concurrent_cycles={CYCLES} allocations={sequence} active_iterations={} total_collections={} live_bytes={} mode={mode} seed={seed} minor_collections={}",
        active.load(Ordering::Acquire),
        heap.collections(),
        heap.from_used(),
        heap.minor_collections()
    );
}

#[test]
fn collector_allocation_reclaims_floating_garbage_after_competing_concurrent_cycle() {
    const CHILD: &str = "GCR_FLOATING_CAPACITY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        for capacity in [4096, 8192] {
            let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::concurrent_allocation_tests::collector_allocation_reclaims_floating_garbage_after_competing_concurrent_cycle", "--nocapture"])
            .env(CHILD, capacity.to_string()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!(
                        "floating-capacity regression hung: {:?}",
                        child.wait_with_output().unwrap()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "capacity={capacity}: {output:?}");
        }
        return;
    }
    static HEAP: AtomicPtr<Heap> = AtomicPtr::new(std::ptr::null_mut());
    static STATE: AtomicPtr<ThreadState> = AtomicPtr::new(std::ptr::null_mut());
    static GO: AtomicBool = AtomicBool::new(false);
    struct AwaitExhaustion;
    impl PtrPolicy for AwaitExhaustion {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            let pointer = IdentityPtrPolicy::try_decode_ptr(bits);
            let heap = HEAP.load(Ordering::Acquire);
            if pointer.is_some() && !heap.is_null() {
                let heap = unsafe { &*heap };
                if heap.barriers_active()
                    && !heap.gc_requested()
                    && !GO.swap(true, Ordering::AcqRel)
                {
                    // Owner access is already released here. Let the worker
                    // remove every edge, then request allocation while the
                    // coordinator still owns gc_lock. Its BLOCKED state proves
                    // that the capacity request is waiting behind this cycle.
                    let state = unsafe { &*STATE.load(Ordering::Acquire) };
                    while !state.is_blocked() {
                        std::thread::yield_now();
                    }
                }
            }
            pointer
        }
        fn encode_ptr(pointer: *mut u8) -> u64 {
            IdentityPtrPolicy::encode_ptr(pointer)
        }
    }
    let capacity: usize = std::env::var(CHILD).unwrap().parse().unwrap();
    assert!([4096, 8192].contains(&capacity));
    let array_info = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
    let leaf_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    assert_eq!(
        array_info.allocation_size(128) + 127 * leaf_info.allocation_size(0),
        4096
    );
    let (ready, receive) = std::sync::mpsc::channel();
    let worker_array_info = array_info.clone();
    let worker_leaf_info = leaf_info.clone();
    let worker = std::thread::spawn(move || {
        let array_info = worker_array_info;
        let leaf_info = worker_leaf_info;
        let mut runtime = RuntimeContext::new(capacity, vec![array_info.clone(), leaf_info.clone()]);
        let heap = runtime.heap().clone();
        let thread = runtime.thread_ptr();
        let execute = || {
            let array = unsafe { heap.alloc_obj::<Full>(&array_info, 128) };
            assert!(!array.is_null());
            let mut first = std::ptr::null_mut();
            for index in 0..127 {
                let leaf = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
                assert!(!leaf.is_null());
                unsafe {
                    leaf.add(Full::SIZE).cast::<u64>().write(index as u64);
                    array
                        .add(array_info.varlen_element_offset(index))
                        .cast::<*mut u8>()
                        .write(leaf);
                }
                if index == 0 {
                    first = leaf;
                }
            }
            unsafe {
                array
                    .add(array_info.varlen_element_offset(127))
                    .cast::<*mut u8>()
                    .write(first);
            }
            let root = unsafe { heap.globals.add(array as u64) };
            assert_eq!(heap.from_used(), 4096);
            // Unreachable allocation fills the larger source arena too. That
            // cycle will have room after SATB retention, so it must coalesce.
            while heap.from_remaining() >= leaf_info.allocation_size(0) {
                let garbage = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
                assert!(!garbage.is_null());
            }
            assert!(heap.from_remaining() < leaf_info.allocation_size(0));
            let state = unsafe { (*thread).dyna_thread };
            ready.send((heap.clone(), state as usize, root)).unwrap();
            while !GO.load(Ordering::Acquire) {
                if heap.gc_requested() {
                    unsafe {
                        ai_gc_pollcheck_slow(thread);
                    }
                }
                std::thread::yield_now();
            }
            let array = heap.globals.get(root) as *mut u8;
            for index in 0..128 {
                unsafe {
                    slots::ai_slot_ref_store(
                        thread,
                        array,
                        array_info.varlen_element_offset(index),
                        std::ptr::null_mut(),
                    );
                }
            }
            // SATB retains all 127 disconnected leaves. The 4 KiB destination is
            // full and requires a fresh STW cycle; the 8 KiB case must coalesce.
            let result = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
            unsafe {
                result.add(Full::SIZE).cast::<u64>().write(42);
            }
            unsafe { heap.globals.add(result as u64) }
        };
        unsafe { crate::runtime::with_current_thread(thread, execute) }
    });
    let (heap, state, array_root) = receive.recv().unwrap();
    STATE.store(state as *mut ThreadState, Ordering::Release);
    HEAP.store(Arc::as_ptr(&heap).cast_mut(), Ordering::Release);
    unsafe {
        heap.concurrent_collect::<AwaitExhaustion>();
    }
    HEAP.store(std::ptr::null_mut(), Ordering::Release);
    STATE.store(std::ptr::null_mut(), Ordering::Release);
    let result_root = worker.join().unwrap();
    assert!(GO.load(Ordering::Acquire));
    let expected_collections = if capacity == 4096 { 2 } else { 1 };
    assert_eq!(heap.collections(), expected_collections);
    // Retiring a TLAB leaves its reserved tail until collection. Remove that
    // accounting slack before checking exact object bytes.
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    assert_eq!(heap.collections(), expected_collections + 1);
    assert_eq!(
        heap.from_used(),
        array_info.allocation_size(128) + leaf_info.allocation_size(0)
    );
    let result = heap.globals.get(result_root) as *const u8;
    assert_eq!(unsafe { result.add(Full::SIZE).cast::<u64>().read() }, 42);
    let array = heap.globals.get(array_root) as *const u8;
    for index in 0..128 {
        assert_eq!(
            unsafe {
                array
                    .add(array_info.varlen_element_offset(index))
                    .cast::<u64>()
                    .read()
            },
            0
        );
    }
}
