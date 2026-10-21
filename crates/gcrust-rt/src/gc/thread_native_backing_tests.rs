use super::*;
use crate::gc::Full;

#[test]
fn collector_registration_buffers_remain_accounted_until_last_observer_release() {
    let heap = Heap::new::<Full>(4096, vec![]);
    let counter = heap.native_memory.registry.clone();
    let baseline = counter.bytes();
    let (state, _) = heap.register_thread();
    let record = ThreadStateHandle::allocation_bytes();
    let census = heap.threads.lock().unwrap().capacity() * std::mem::size_of::<crate::gc::ThreadStateHandle>();
    assert_eq!(counter.bytes() - baseline, record + census + 256 * 8,
        "registered SATB backing is outside the native ledger");
    unsafe { state.record_alloc(4095, 4096); }
    let profile_bytes = unsafe { &*state.alloc_sites.get() }.capacity() * std::mem::size_of::<SiteCounter>();
    assert!(profile_bytes >= 4096 * std::mem::size_of::<SiteCounter>());
    assert_eq!(counter.bytes() - baseline, record + census + 256 * 8 + profile_bytes);
    let snapshot = unsafe { state.alloc_site_counters() };
    unsafe { heap.safe_deregister_thread(&state); }
    assert_eq!(counter.bytes() - baseline, record + 256 * 8 + profile_bytes + heap.retired_profile_backing_bytes());
    drop(heap);
    assert_eq!(counter.bytes(), record + 256 * 8 + profile_bytes,
        "external observers still own the registration buffers after Heap drop");
    drop(state);
    assert_eq!(counter.bytes(), 0);
    assert_eq!(snapshot[4095].count, 1);
    assert_eq!(snapshot[4095].bytes, 4096);
}

#[test]
fn collector_registration_warm_profile_writes_do_not_allocate_or_recharge() {
    let heap = Heap::new::<Full>(4096, vec![]);
    let counter = heap.native_memory.registry.clone();
    let (state, _) = heap.register_thread();
    unsafe { state.record_alloc(15, 8); }
    let before = counter.bytes();
    crate::runtime::allocation_probe::start();
    for _ in 0..1024 { unsafe { state.record_alloc(15, 8); } }
    let traffic = crate::runtime::allocation_probe::finish_bytes();
    assert_eq!(traffic, (0, 0, 0, 0));
    assert_eq!(counter.bytes(), before);
    assert_eq!(unsafe { state.alloc_site_counters() }[15].count, 1025);
    unsafe { heap.safe_deregister_thread(&state); }
}

#[test]
fn collector_registration_profile_backing_is_included_in_native_ledger() {
    let heap = Heap::new::<Full>(4096, vec![]);
    let counter = heap.native_memory.registry.clone();
    let (state, _) = heap.register_thread();
    let before = counter.bytes();
    unsafe { state.record_alloc(4095, 4096); }
    let profile_bytes = unsafe { &*state.alloc_sites.get() }.capacity() * std::mem::size_of::<SiteCounter>();
    let charged = counter.bytes() - before;
    unsafe { heap.safe_deregister_thread(&state); }
    assert_eq!(charged, profile_bytes, "allocation-site backing is outside the native ledger");
}

#[test]
fn collector_registration_profile_window_survives_growth_and_owner_writes() {
    let heap = Heap::new::<Full>(4096, vec![]);
    let (state, _) = heap.register_thread();
    unsafe {
        let window = state.inline_tlab_ptr();
        state.record_alloc(15, 8);
        let first = (*window).counters;
        state.record_alloc(15, 8);
        // Generated allocation code writes through this exported raw window.
        std::ptr::addr_of_mut!((*first.add(15)).count).write(40);
        state.record_alloc(4095, 16);
        let grown = (*window).counters;
        assert!((*window).counters_len >= 4096);
        state.record_alloc(4095, 16);
        std::ptr::addr_of_mut!((*grown.add(4095)).count).write(42);
        let snapshot = state.alloc_site_counters();
        assert_eq!(snapshot[15].count, 40);
        assert_eq!(snapshot[4095].count, 42);
        state.record_alloc(4095, 16);
        assert_eq!(grown.add(4095).read().count, 43);
        assert_eq!((*window).counters, grown);
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_registration_record_and_control_have_physical_allocation_charges() {
    let heap = Heap::new::<Full>(4096, vec![]);
    // Warm platform lazy synchronization backing separately: this regression
    // measures registration ownership rather than unrelated first-lock costs.
    let (warm, _) = heap.register_thread();
    unsafe { heap.safe_deregister_thread(&warm); }
    drop(warm);
    let counter = heap.native_memory.registry.clone();
    let baseline = counter.bytes();
    crate::runtime::allocation_probe::start();
    let (state, _) = heap.register_thread();
    let traffic = crate::runtime::allocation_probe::finish_bytes();
    let retained_requested = traffic.2.checked_sub(traffic.3).unwrap();
    let charged = counter.bytes() - baseline;
    unsafe { heap.safe_deregister_thread(&state); }
    assert_eq!(charged, retained_requested,
        "thread registration retained allocator requests outside its native ledger: {traffic:?}");
}

#[test]
fn collector_registration_weak_observer_keeps_only_physical_record_charge() {
    assert_eq!(std::mem::size_of::<ThreadStateHandle>(), std::mem::size_of::<usize>());
    assert_eq!(std::mem::size_of::<Option<ThreadStateHandle>>(), std::mem::size_of::<usize>());
    let heap = Heap::new::<Full>(4096, vec![]);
    let counter = heap.native_memory.registry.clone();
    let (state, _) = heap.register_thread();
    crate::runtime::allocation_probe::start();
    let weak = ThreadStateHandle::downgrade(&state);
    let other_weak = weak.clone();
    let owner = weak.upgrade().unwrap();
    assert!(ThreadStateHandle::ptr_eq(&state, &owner));
    assert_eq!(ThreadStateHandle::strong_count(&state), 3);
    drop(owner);
    assert_eq!(crate::runtime::allocation_probe::finish_bytes(), (0, 0, 0, 0));
    unsafe { heap.safe_deregister_thread(&state); }
    drop(heap);
    assert_eq!(counter.bytes(), ThreadStateHandle::allocation_bytes() + 256 * 8);
    drop(state);
    assert!(weak.upgrade().is_none());
    assert_eq!(counter.bytes(), ThreadStateHandle::allocation_bytes());
    drop(weak);
    assert_eq!(counter.bytes(), ThreadStateHandle::allocation_bytes());
    drop(other_weak);
    assert_eq!(counter.bytes(), 0);
}
