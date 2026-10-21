use super::*;
use crate::gc::{Compact, ObjHeader, PtrPolicy};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

struct Tagged;
impl PtrPolicy for Tagged {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        if bits == 0 {
            None
        } else {
            Some((bits & !3) as *mut u8)
        }
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64 | 3
    }
}

fn assert_retired(heap: &Heap, weak: &std::sync::Weak<JoinHandle>) {
    // GC removes registry ownership immediately. A background probe may still
    // finish releasing a bounded native borrow; no additional GC is needed.
    assert!(!heap.thread_completions.lock().unwrap().records.contains_key(&(weak.as_ptr() as usize)));
    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
    while weak.upgrade().is_some() {
        assert!(std::time::Instant::now()<deadline,"retired native borrow did not finish");
        std::thread::yield_now();
    }
}

fn complete(owner: usize, result: u64) -> Arc<JoinHandle> {
    Arc::new(JoinHandle {
        completion_notice: false,
        native_joined: std::sync::atomic::AtomicBool::new(false),
        pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
        reaper_registration: std::sync::OnceLock::new(),
        retirement: std::sync::OnceLock::new(),
        state: Mutex::new(JoinState::Complete(result)),
        owner: AtomicUsize::new(owner),
    })
}

fn collect<P: PtrPolicy>(heap: &Heap, state: &ThreadState, mode: usize) {
    unsafe {
        match mode {
            0 => {
                state.enter_blocked();
                heap.stw_collect::<P>();
                state.exit_blocked(heap);
            }
            1 => heap.mutator_triggered_gc::<P>(state),
            2 | 4 => heap.mutator_triggered_minor_gc::<P>(state),
            3 => {
                state.enter_blocked();
                heap.concurrent_collect::<P>();
                state.exit_blocked(heap);
            }
            _ => unreachable!(),
        }
    }
}

fn ownership_case<H: ObjHeader, P: PtrPolicy>(mode: usize) {
    let info = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
    let heap = Heap::new_generational::<H>(1024, 8192, vec![info.clone()]);
    // Force real minor pauses even after the first one empties the nursery.
    heap.set_gc_every_alloc(matches!(mode, 2 | 4));
    let (state, _) = heap.register_thread();
    let allocate = || unsafe {
        if matches!(mode, 2 | 4) {
            heap.alloc_nursery_obj::<H>(&info, 0)
        } else {
            heap.alloc_obj::<H>(&info, 0)
        }
    };
    let live = allocate();
    let dead = allocate();
    let pending_owner = allocate();
    let root = unsafe { heap.globals.add(P::encode_ptr(live)) };
    let live_record = complete(live as usize, 42);
    let live_weak = Arc::downgrade(&live_record);
    let dead_record = complete(dead as usize, 13);
    let dead_weak = Arc::downgrade(&dead_record);
    let legacy = complete(1, 9);
    let (release, wait) = std::sync::mpsc::channel();
    let pending = Arc::new(JoinHandle {
        completion_notice: false,
        native_joined: std::sync::atomic::AtomicBool::new(false),
        pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
        reaper_registration: std::sync::OnceLock::new(),
        retirement: std::sync::OnceLock::new(),
        state: Mutex::new(JoinState::Running(std::thread::spawn(move || {
            wait.recv().unwrap();
            77
        }))),
        owner: AtomicUsize::new(pending_owner as usize),
    });
    let pending_weak = Arc::downgrade(&pending);
    for completion in [live_record, dead_record, legacy, pending.clone()] {
        heap.thread_completions.lock().unwrap().register(completion, &heap.native_retirement);
    }
    if mode == 4 {
        collect::<P>(&heap, &state, 1);
        assert_eq!(
            live_weak.upgrade().unwrap().owner.load(Ordering::Relaxed),
            live as usize
        );
        assert!(
            dead_weak.upgrade().is_some(),
            "major cannot decide nursery liveness"
        );
        assert_eq!(heap.from_used(), 0);
    }
    collect::<P>(&heap, &state, mode);
    let moved = P::try_decode_ptr(heap.globals.get(root)).unwrap();
    assert_ne!(moved, live);
    assert_eq!(
        live_weak.upgrade().unwrap().owner.load(Ordering::Relaxed),
        moved as usize
    );
    assert_retired(&heap, &dead_weak);
    assert_eq!(heap.from_used(), info.allocation_size(0));
    assert_eq!(
        pending.owner.load(Ordering::Relaxed),
        0,
        "dead owners must not survive arena reuse"
    );
    assert_eq!(heap.thread_completions.lock().unwrap().records.len(), 3);
    // A second collection must not dereference the now-dead source address.
    collect::<P>(&heap, &state, mode);
    assert_eq!(pending.owner.load(Ordering::Relaxed), 0);
    release.send(()).unwrap();
    assert_eq!(pending.join(), 77);
    drop(pending);
    collect::<P>(&heap, &state, mode);
    assert_retired(&heap, &pending_weak);
    unsafe {
        heap.globals.set(root, 0);
    }
    if matches!(mode, 2 | 4) {
        collect::<P>(&heap, &state, 2);
        assert!(
            live_weak.upgrade().is_some(),
            "a minor cannot decide old-object liveness"
        );
        collect::<P>(&heap, &state, 1);
    } else {
        collect::<P>(&heap, &state, mode);
    }
    assert_retired(&heap, &live_weak);
    assert_eq!(heap.from_used(), 0);
    let records = heap.thread_completions.lock().unwrap();
    assert_eq!(
        records.records.len(),
        1,
        "unbound raw handles retain execution lifetime"
    );
    assert_eq!(records.records.values().next().unwrap().join(), 9);
    assert_eq!(records.records.values().next().unwrap().join(), 9);
    drop(records);
    unsafe {
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_completion_weak_owners_follow_every_copy_path_and_pointer_policy() {
    if !child_or_run(
        "runtime::completion_owner_tests::collector_completion_weak_owners_follow_every_copy_path_and_pointer_policy",
        "GCR_COMPLETION_MATRIX_CHILD",
    ) {
        return;
    }
    for mode in 0..5 {
        ownership_case::<Full, IdentityPtrPolicy>(mode);
        ownership_case::<Compact, IdentityPtrPolicy>(mode);
        ownership_case::<Full, Tagged>(mode);
        ownership_case::<Compact, Tagged>(mode);
    }
}

unsafe extern "C" fn payload(_thread: *mut Thread, env: *mut u8) -> u64 {
    unsafe { env.add(Full::SIZE).cast::<u64>().read() }
}

#[test]
fn collector_completion_joined_churn_reclaims_native_records_with_dead_owners() {
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let mut runtime = RuntimeContext::new(4096, vec![info]);
    let heap = runtime.heap().clone();
    let thread = runtime.thread_ptr();
    let execute = || {
        let root = unsafe { heap.globals.add(0) };
        for _ in 0..128 {
            let owner = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
            unsafe {
                owner.add(Full::SIZE).cast::<u64>().write(42);
                heap.globals.set(root, owner as u64);
            }
            let handle = unsafe { ai_thread_spawn(thread, owner, payload as *const u8) };
            unsafe {
                ai_thread_bind_owner(thread, handle, owner);
            }
            // A native observer may borrow the record beyond registry unlink.
            // Hold that borrow deterministically instead of racing the reaper.
            let native_borrow = heap
                .thread_completions
                .lock()
                .unwrap()
                .records
                .values()
                .next()
                .unwrap()
                .clone();
            let weak = Arc::downgrade(&native_borrow);
            assert_eq!(unsafe { ai_thread_join(thread, handle) }, 42);
            unsafe {
                runtime.force_collect(&*thread);
            }
            assert_eq!(heap.thread_completions.lock().unwrap().records.len(), 1);
            assert_eq!(unsafe { ai_thread_join(thread, handle) }, 42);
            unsafe {
                heap.globals.set(root, 0);
                runtime.force_collect(&*thread);
            }
            let (records, pending, owners) = {
                let registry = heap.thread_completions.lock().unwrap();
                (
                    registry.records.len(),
                    registry.pending.len(),
                    registry.owned.len(),
                )
            };
            assert_eq!(records, 0);
            assert_eq!(pending, 0);
            assert_eq!(owners, 0);
            assert!(weak.upgrade().is_some(), "native borrow must survive unlink");
            drop(native_borrow);
            // GC must not wait for native observers. After unlink, wait outside
            // the mutator/collector for the reaper's transient borrow to end.
            unsafe {
                blocking_region(thread, || {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while weak.upgrade().is_some() {
                        assert!(std::time::Instant::now() < deadline, "unlinked completion still retained after native observers finished");
                        std::thread::yield_now();
                    }
                });
            }
        }
        let (records, owners) = {
            let registry = heap.thread_completions.lock().unwrap();
            (registry.records.capacity(), registry.owned.capacity())
        };
        assert!(records <= 4);
        assert!(owners <= 4);
        println!("completion owner churn: spawns=128 collections=256 final_native_records=0");
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}

fn child_or_run(name: &str, key: &str) -> bool {
    if std::env::var_os(key).is_some() {
        return true;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(key, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!(
                "completion ownership deadlock: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    false
}

#[test]
fn collector_completion_sweep_does_not_wait_for_join_observer() {
    if !child_or_run(
        "runtime::completion_owner_tests::collector_completion_sweep_does_not_wait_for_join_observer",
        "GCR_COMPLETION_SWEEP_CHILD",
    ) {
        return;
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new::<Full>(4096, vec![info.clone()]);
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let record = complete(owner as usize, 42);
    let weak = Arc::downgrade(&record);
    heap.thread_completions
        .lock()
        .unwrap()
        .register(record.clone(), &heap.native_retirement);
    let (ready, started) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        let _state = record.state.lock().unwrap();
        ready.send(()).unwrap();
        wait.recv().unwrap();
    });
    started.recv().unwrap();
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    assert_eq!(heap.thread_completions.lock().unwrap().records.len(), 1);
    assert_eq!(weak.upgrade().unwrap().owner.load(Ordering::Relaxed), 0);
    release.send(()).unwrap();
    observer.join().unwrap();
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    assert_retired(&heap, &weak);
}

#[test]
fn collector_completion_shutdown_follows_descendants_after_registry_pruning() {
    if !child_or_run(
        "runtime::completion_owner_tests::collector_completion_shutdown_follows_descendants_after_registry_pruning",
        "GCR_COMPLETION_DRAIN_CHILD",
    ) {
        return;
    }
    struct Plan {
        parent: AtomicPtr<JoinHandle>,
        descendant: Mutex<Option<Arc<JoinHandle>>>,
        first: std::sync::Weak<JoinHandle>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        returned: std::sync::atomic::AtomicBool,
        done: std::sync::atomic::AtomicBool,
    }
    static PLAN: AtomicPtr<Plan> = AtomicPtr::new(core::ptr::null_mut());
    unsafe extern "C" fn descendant(thread: *mut Thread, _env: *mut u8) -> u64 {
        let plan = unsafe { &*PLAN.load(Ordering::Acquire) };
        unsafe {
            blocking_region(thread, || plan.release.lock().unwrap().recv().unwrap());
        }
        plan.done.store(true, Ordering::Release);
        0
    }
    unsafe extern "C" fn parent(thread: *mut Thread, _env: *mut u8) -> u64 {
        let plan = unsafe { &*PLAN.load(Ordering::Acquire) };
        let heap = unsafe { &*(*thread).heap };
        let completion = loop {
            let pointer = plan.parent.load(Ordering::Acquire);
            if !pointer.is_null() {
                break heap
                    .thread_completions
                    .lock()
                    .unwrap()
                    .records
                    .values()
                    .find(|record| Arc::as_ptr(record) == pointer)
                    .unwrap()
                    .clone();
            }
            ai_thread_yield(thread);
        };
        // Root teardown must have passed the first record and be joining us.
        while completion.state.try_lock().is_ok() {
            ai_thread_yield(thread);
        }
        unsafe {
            ai_gc_alloc_fixed(thread, 0, 0);
        } // collect the first record's owner
        assert!(
            !heap.thread_completions.lock().unwrap().records.contains_key(&(plan.first.as_ptr() as usize)),
            "fixture must really prune an earlier registry entry"
        );
        let env = unsafe { (&*(*thread).dyna_thread).scratch_at(0) };
        let handle = unsafe { ai_thread_spawn(thread, env, descendant as *const u8) };
        let record = heap
            .thread_completions
            .lock()
            .unwrap()
            .records
            .values()
            .find(|record| Arc::as_ptr(record) == handle)
            .unwrap()
            .clone();
        *plan.descendant.lock().unwrap() = Some(record);
        0
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let mut context = RuntimeContext::new(4096, vec![info.clone()]);
    let heap = context.heap().clone();
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let first = complete(owner as usize, 17);
    let weak = Arc::downgrade(&first);
    heap.thread_completions.lock().unwrap().register(first, &heap.native_retirement);
    let (release, wait) = std::sync::mpsc::channel();
    let plan = Arc::new(Plan {
        parent: AtomicPtr::new(core::ptr::null_mut()),
        descendant: Mutex::new(None),
        first: weak,
        release: Mutex::new(wait),
        returned: std::sync::atomic::AtomicBool::new(false),
        done: std::sync::atomic::AtomicBool::new(false),
    });
    PLAN.store(Arc::as_ptr(&plan).cast_mut(), Ordering::Release);
    let watcher_plan = plan.clone();
    let watcher = std::thread::spawn(move || {
        let completion = loop {
            if let Some(record) = watcher_plan.descendant.lock().unwrap().clone() {
                break record;
            }
            std::thread::yield_now();
        };
        // The correct drain blocks in descendant join. The old index walk
        // instead returns early. Release the child in either case so failures
        // remain bounded, and keep an Arc so even that broken path cannot UAF.
        while !watcher_plan.returned.load(Ordering::Acquire) && completion.state.try_lock().is_ok()
        {
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        completion.join();
    });
    let env = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    heap.set_gc_every_alloc(true);
    let handle = context.enter(|thread| unsafe { ai_thread_spawn(thread, env, parent as *const u8) });
    plan.parent.store(handle, Ordering::Release);
    unsafe {
        context.finish_threads();
    }
    let finished_at_return = plan.done.load(Ordering::Acquire);
    plan.returned.store(true, Ordering::Release);
    watcher.join().unwrap();
    PLAN.store(core::ptr::null_mut(), Ordering::Release);
    assert!(
        finished_at_return,
        "root teardown skipped a descendant after registry pruning"
    );
    assert!(heap.thread_completions.lock().unwrap().records.is_empty());
}

#[test]
fn collector_completion_owner_can_be_bound_during_concurrent_copying() {
    if !child_or_run(
        "runtime::completion_owner_tests::collector_completion_owner_can_be_bound_during_concurrent_copying",
        "GCR_COMPLETION_COPY_BIND_CHILD",
    ) {
        return;
    }
    static HEAP: AtomicPtr<Heap> = AtomicPtr::new(core::ptr::null_mut());
    static COPYING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static BOUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static FINISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    struct CopyRendezvous;
    impl PtrPolicy for CopyRendezvous {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            let pointer = IdentityPtrPolicy::try_decode_ptr(bits);
            let heap = HEAP.load(Ordering::Acquire);
            if pointer.is_some() && !heap.is_null() {
                let heap = unsafe { &*heap };
                if heap.barriers_active()
                    && !heap.gc_requested()
                    && !COPYING.swap(true, Ordering::AcqRel)
                {
                    // The worker has ample allocation space and does not spawn
                    // or join during this rendezvous (registration waits on GC).
                    while !BOUND.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                }
            }
            pointer
        }
        fn encode_ptr(pointer: *mut u8) -> u64 {
            pointer as u64
        }
    }
    for generational in [false, true] {
        COPYING.store(false, Ordering::Relaxed);
        BOUND.store(false, Ordering::Relaxed);
        FINISHED.store(false, Ordering::Relaxed);
        let (ready, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let node = TypeInfo::for_header(Full::SIZE).with_fields(1);
            let leaf = TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_raw_bytes(8);
            let mut context = if generational {
                RuntimeContext::new_generational(1024, 16 * 1024, vec![node.clone(), leaf.clone()])
            } else {
                RuntimeContext::new(16 * 1024, vec![node.clone(), leaf.clone()])
            };
            let heap = context.heap().clone();
            let thread = context.thread_ptr();
            let execute = || {
                let parent = unsafe { heap.alloc_obj::<Full>(&node, 0) };
                let value = unsafe { heap.alloc_obj::<Full>(&leaf, 0) };
                unsafe {
                    parent.add(Full::SIZE).cast::<*mut u8>().write(value);
                    value.add(Full::SIZE).cast::<u64>().write(42);
                    heap.globals.add(parent as u64);
                }
                let handle = unsafe { ai_thread_spawn(thread, value, payload as *const u8) };
                assert_eq!(unsafe { ai_thread_join(thread, handle) }, 42);
                let weak = Arc::downgrade(
                    &heap
                        .thread_completions
                        .lock()
                        .unwrap()
                        .records
                        .values()
                        .next()
                        .unwrap(),
                );
                let atomic = unsafe { ai_atomic_i64_new(thread, 17) };
                let atomic_weak = Arc::downgrade(&heap.atomic_cells.lock().unwrap().records[&(atomic as usize)].0);
                let channel = super::channel_owner_tests::queue(value as u64, &heap.native_retirement);
                let channel_weak = Arc::downgrade(&channel);
                let channel = heap.channels.lock().unwrap().register(channel, &heap.native_memory);
                ready.send(heap.clone()).unwrap();
                while !COPYING.load(Ordering::Acquire) {
                    if heap.gc_requested() {
                        unsafe {
                            ai_gc_pollcheck_slow(thread);
                        }
                    }
                    std::thread::yield_now();
                }
                let owner = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
                unsafe {
                    owner.add(Full::SIZE).cast::<u64>().write(77);
                }
                let root = unsafe { heap.globals.add(owner as u64) };
                unsafe {
                    ai_thread_bind_owner(thread, handle, owner);
                    ai_atomic_i64_bind_owner(thread, atomic, owner);
                    ai_chan_bind_owner(thread, channel, owner);
                }
                BOUND.store(true, Ordering::Release);
                while !FINISHED.load(Ordering::Acquire) {
                    if heap.gc_requested() {
                        unsafe {
                            ai_gc_pollcheck_slow(thread);
                        }
                    }
                    std::thread::yield_now();
                }
                let moved = heap.globals.get(root) as *mut u8;
                let record = weak
                    .upgrade()
                    .expect("late owner was lost during final pause");
                assert_eq!(record.owner.load(Ordering::Relaxed), moved as usize);
                assert_eq!(heap.atomic_cells.lock().unwrap().records[&(atomic as usize)].1, Some(moved as usize));
                assert_eq!(heap.channels.lock().unwrap().records[&(channel as usize)].0.owner.load(Ordering::Acquire), moved as usize);
                if generational {
                    assert_eq!(moved, owner, "old major must not relocate nursery owner");
                    unsafe {
                        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&context.thread.registration.dyna);
                    }
                    let promoted = heap.globals.get(root) as *mut u8;
                    assert_ne!(promoted, owner);
                    assert_eq!(record.owner.load(Ordering::Relaxed), promoted as usize);
                    assert_eq!(heap.channels.lock().unwrap().records[&(channel as usize)].0.owner.load(Ordering::Acquire), promoted as usize);
                    assert_eq!(heap.atomic_cells.lock().unwrap().records[&(atomic as usize)].1, Some(promoted as usize));
                } else {
                    assert_ne!(moved, owner, "late source allocation must be evacuated");
                }
                assert_eq!(unsafe { ai_thread_join(thread, handle) }, 42);
                assert_eq!(unsafe { ai_atomic_i64_fetch_add(thread, atomic, 25) }, 17);
                assert_eq!(unsafe { ai_atomic_i64_load(thread, atomic) }, 42);
                drop(record);
                unsafe {
                    heap.globals.set(root, 0);
                    context.force_collect(&*thread);
                }
                assert_retired(&heap, &weak);
                assert!(!heap.atomic_cells.lock().unwrap().records.contains_key(&(atomic as usize)),
                    "dead atomic owner remained registered after collection");
                unsafe { blocking_region(thread, || heap.native_retirement.drain()); }
                assert!(atomic_weak.upgrade().is_none());
                assert!(channel_weak.upgrade().is_none());
            };
            unsafe { crate::runtime::with_current_thread(thread, execute) }
        });
        let heap = receive.recv().unwrap();
        HEAP.store(Arc::as_ptr(&heap).cast_mut(), Ordering::Release);
        unsafe {
            heap.concurrent_collect::<CopyRendezvous>();
        }
        FINISHED.store(true, Ordering::Release);
        worker.join().unwrap();
        HEAP.store(core::ptr::null_mut(), Ordering::Release);
        assert!(BOUND.load(Ordering::Acquire));
    }
}

#[test]
fn collector_completion_sweep_visits_only_managed_owner_records() {
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new::<Full>(4096, vec![info.clone()]);
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let mut registry = CompletionRegistry::default();
    for value in 0..10_000 {
        registry.register(complete(1, value), &heap.native_retirement);
    }
    registry.register(complete(owner as usize, 42), &heap.native_retirement);
    let mut visited = 0;
    registry.sweep(|_| {
        visited += 1;
        true
    });
    assert_eq!(
        visited, 1,
        "raw completion history must not add per-GC owner tracing"
    );
    assert_eq!(registry.records.len(), 10_001);
    assert_eq!(registry.owned.len(), 1);
    // Dense iteration follows the single bound owner, not the unused
    // reservation that permits raw records to bind later without allocating.
    assert!(registry.owned.capacity() >= registry.records.len());
    assert!(registry.owned.capacity() <= registry.records.len().saturating_mul(4));
}

#[test]
fn collector_completion_joined_owner_reclaims_while_native_observer_holds_state() {
    let retirement = native_retirement::Retirement::default();
    let record = completion_registry_growth::new_record(std::thread::spawn(|| 42), 0, &retirement);
    assert_eq!(record.join(), 42);
    record.owner.store(0, Ordering::Relaxed);
    let observer = record.clone();
    let weak = Arc::downgrade(&record);
    let mut registry = CompletionRegistry::default();
    registry.register(record, &retirement);
    let state = observer.state.lock().unwrap();
    allocation_probe::start();
    registry.sweep(|record| !record.relocate_owner_and_reclaimable(|_| None));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert!(registry.records.is_empty(), "joined native state must not depend on observer mutex availability");
    assert!(registry.pending.is_empty() && registry.owned.is_empty());
    assert!(observer.is_complete());
    assert!(matches!(*state, JoinState::Complete(42)));
    drop(state);
    assert_eq!(observer.join(), 42);
    drop(observer);
    retirement.drain();
    assert!(weak.upgrade().is_none());
}
