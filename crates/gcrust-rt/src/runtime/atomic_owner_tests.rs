use super::*;
use crate::gc::{Compact, ObjHeader, PtrPolicy};

struct Tagged;
impl PtrPolicy for Tagged {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        (bits != 0).then_some((bits & !3) as *mut u8)
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64 | 3
    }
}

fn ownership<H: ObjHeader, P: PtrPolicy>(mode: usize) {
    let info = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
    let heap = Heap::new_generational::<H>(1024, 8192, vec![info.clone()]);
    heap.set_gc_every_alloc(matches!(mode, 2 | 4));
    let (state, _) = heap.register_thread();
    let owner = unsafe {
        if matches!(mode, 2 | 4) {
            heap.alloc_nursery_obj::<H>(&info, 0)
        } else {
            heap.alloc_obj::<H>(&info, 0)
        }
    };
    let root = unsafe { heap.globals.add(P::encode_ptr(owner)) };
    let cell = native_retirement::AtomicCell::new(42, &heap.native_retirement);
    let weak = Arc::downgrade(&cell);
    let pointer = heap.atomic_cells.lock().unwrap().register(cell);
    heap.atomic_cells.lock().unwrap().bind(pointer, owner);
    let collect = || unsafe {
        match mode {
            0 => {
                state.enter_blocked();
                heap.stw_collect::<P>();
                state.exit_blocked(&heap);
            }
            1 => heap.mutator_triggered_gc::<P>(&state),
            2 | 4 => heap.mutator_triggered_minor_gc::<P>(&state),
            3 => {
                state.enter_blocked();
                heap.concurrent_collect::<P>();
                state.exit_blocked(&heap);
            }
            _ => unreachable!(),
        }
    };
    let nursery_dead = if mode == 4 {
        let owner = unsafe { heap.alloc_nursery_obj::<H>(&info, 0) };
        let cell = native_retirement::AtomicCell::new(9, &heap.native_retirement);
        let weak = Arc::downgrade(&cell);
        let pointer = heap.atomic_cells.lock().unwrap().register(cell);
        heap.atomic_cells.lock().unwrap().bind(pointer, owner);
        unsafe {
            heap.mutator_triggered_gc::<P>(&state);
        }
        assert!(
            weak.upgrade().is_some(),
            "old major decided nursery liveness"
        );
        Some(weak)
    } else {
        None
    };
    for _ in 0..3 {
        collect();
        let expected = P::try_decode_ptr(heap.globals.get(root)).unwrap() as usize;
        let actual = heap
            .atomic_cells
            .lock()
            .unwrap()
            .records
            .get(&(pointer as usize))
            .map(|r| r.1);
        assert_eq!(actual, Some(Some(expected)), "live atomic owner relocation");
        assert_eq!(
            weak.upgrade()
                .expect("live cell retired")
                .load(Ordering::SeqCst),
            42
        );
    }
    if let Some(weak) = nursery_dead {
        unsafe { state.enter_blocked(); }
        heap.native_retirement.drain();
        unsafe { state.exit_blocked(&heap); }
        assert!(weak.upgrade().is_none());
    }
    unsafe {
        heap.globals.set(root, 0);
    }
    collect();
    if matches!(mode, 2 | 4) {
        assert!(weak.upgrade().is_some(), "minor decided old-owner liveness");
        unsafe {
            heap.mutator_triggered_gc::<P>(&state);
        }
    }
    unsafe { state.enter_blocked(); }
    heap.native_retirement.drain();
    unsafe { state.exit_blocked(&heap); }
    assert!(weak.upgrade().is_none(), "dead owner retained native cell");
    collect();
    assert!(heap.atomic_cells.lock().unwrap().records.is_empty());
    unsafe {
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_atomic_weak_owners_follow_all_copy_paths_and_pointer_policies() {
    for mode in 0..5 {
        ownership::<Full, IdentityPtrPolicy>(mode);
        ownership::<Compact, IdentityPtrPolicy>(mode);
        ownership::<Full, Tagged>(mode);
        ownership::<Compact, Tagged>(mode);
    }
}

#[test]
fn collector_atomic_churn_reclaims_native_cells_before_execution_shutdown() {
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let mut context = RuntimeContext::new(8192, vec![info]);
    let heap = context.heap().clone();
    let thread = context.thread_ptr();
    let execute = || {
        let root = unsafe { heap.globals.add(0) };
        for _ in 0..128 {
            let owner = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
            unsafe {
                heap.globals.set(root, owner as u64);
            }
            let cell = unsafe { ai_atomic_i64_new(thread, 42) };
            let weak = Arc::downgrade(&heap.atomic_cells.lock().unwrap().records[&(cell as usize)].0);
            unsafe {
                ai_atomic_i64_bind_owner(thread, cell, owner);
                context.force_collect(&*thread);
            }
            assert!(weak.upgrade().is_some());
            unsafe {
                assert_eq!(ai_atomic_i64_fetch_add(thread, cell, 1), 42);
                assert_eq!(ai_atomic_i64_compare_and_set(thread, cell, 43, 81), 1);
                assert_eq!(ai_atomic_i64_compare_and_set(thread, cell, 43, 82), 0);
                assert_eq!(ai_atomic_i64_load(thread, cell), 81);
                heap.globals.set(root, 0);
                context.force_collect(&*thread);
            }
            unsafe { blocking_region(thread, || heap.native_retirement.drain()); }
            assert!(weak.upgrade().is_none());
            let sizes = {
                let registry = heap.atomic_cells.lock().unwrap();
                (
                    registry.records.len(),
                    registry.owned.len(),
                    registry.records.capacity(),
                    registry.owned.capacity(),
                )
            };
            assert_eq!((sizes.0, sizes.1), (0, 0));
            assert!(
                sizes.2 <= 4 && sizes.3 <= 4,
                "native registry grew with churn: {sizes:?}"
            );
        }
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}

#[test]
fn collector_atomic_owner_sweep_excludes_legacy_raw_history() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = AtomicCellRegistry::default();
    for _ in 0..10_000 {
        registry.register(native_retirement::AtomicCell::new(0, &retirement));
    }
    let pointer = registry.register(native_retirement::AtomicCell::new(42, &retirement));
    registry.bind(pointer, 8usize as *mut u8);
    let mut visited = 0;
    registry.sweep(|owner| {
        assert_eq!(owner as usize, 8);
        visited += 1;
        None
    });
    assert_eq!(visited, 1);
    assert_eq!(registry.records.len(), 10_000);
    assert!(registry.owned.is_empty());
}

#[test]
fn collector_native_owner_indices_shrink_after_managed_bursts() {
    for raw in [0, 8, 1000] {
        let retirement = native_retirement::Retirement::default();
        let mut atomics = AtomicCellRegistry::default();
        let mut threads = CompletionRegistry::default();
        for i in 0..(raw + 10_000) {
            let owner = if i < raw { 1 } else { 8 };
            let cell = atomics.register(native_retirement::AtomicCell::new(0, &retirement));
            if owner != 1 {
                atomics.bind(cell, owner as *mut u8);
            }
            threads.register(Arc::new(JoinHandle {
                completion_notice: false,
                native_joined: std::sync::atomic::AtomicBool::new(false),
                pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
                reaper_registration: std::sync::OnceLock::new(),
                retirement: std::sync::OnceLock::new(),
                state: std::sync::Mutex::new(JoinState::Complete(0)),
                owner: std::sync::atomic::AtomicUsize::new(owner),
            }), &retirement);
        }
        atomics.sweep(|_| None);
        threads.sweep(|_| false);
        let native = Mutex::new(std::mem::take(&mut atomics));
        registry_compaction::compact_atomics(&native);
        atomics = native.into_inner().unwrap();
        let native = Mutex::new(std::mem::take(&mut threads));
        registry_compaction::compact_completions(&native);
        threads = native.into_inner().unwrap();
        assert_eq!(atomics.records.len(), raw);
        assert_eq!(threads.records.len(), raw);
        assert!(threads.pending.len() <= raw);
        assert!(threads.owned.capacity() >= raw && threads.owned.capacity() <= (4 * raw).max(8));
        // Unbound cells may bind after GC without allocation. Their dense
        // reservation must remain bounded by live records, not the burst peak.
        assert!(atomics.owned.capacity() >= raw);
        assert!(atomics.owned.capacity() <= (4 * raw).max(8));
        let bound = (4 * raw).max(8);
        assert!(
            atomics.records.capacity() <= bound,
            "atomic burst table retained"
        );
        assert!(
            threads.records.capacity() <= bound,
            "completion burst table retained"
        );
        atomics.clear();
        threads.clear();
        assert!(atomics.records.capacity() <= 8 && threads.records.capacity() <= 8);
        assert!(atomics.owned.capacity() <= 8 && threads.owned.capacity() <= 8);
        assert!(threads.pending.is_empty());
    }
}
