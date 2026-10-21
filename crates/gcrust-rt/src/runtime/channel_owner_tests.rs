use super::*;
use crate::gc::{Compact, ObjHeader, PtrPolicy};

#[test]
fn collector_channel_groups_reference_old_owner_workspace() {
    use crate::gc::ConditionalChannels;
    let mut measurements = Vec::new();
    for count in [0, 1, 32, 1024] {
        for per_owner in [1, 8, usize::MAX] {
            let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
            let heap = Heap::new_generational::<Full>(4096, 1 << 20, vec![info.clone()]);
            let mut owners = Vec::new();
            // Exclusive quiescent fixture: every owner/message is tenured.
            for index in 0..count {
                let owner = if index % per_owner != 0 {
                    *owners.last().unwrap()
                } else {
                    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
                    assert!(!owner.is_null());
                    owners.push(owner);
                    owner
                };
                let message = unsafe { heap.alloc_obj::<Full>(&info, 0) };
                assert!(!message.is_null());
                unsafe {
                    message.add(Full::SIZE).cast::<u64>().write(index as u64);
                }
                let mut registry = heap.channels.lock().unwrap();
                let handle = registry.register(queue(message as u64, &heap.native_retirement), &heap.native_memory);
                registry.bind(
                    handle,
                    ChannelOwner {
                        address: owner as usize,
                        type_id: 0,
                    },
                );
            }
            drop(heap.channels.lock().unwrap());
            let ledger_before = heap.native_registry_bytes();
            let counter_owners = Arc::strong_count(&heap.native_memory.registry);
            allocation_probe::start();
            let groups = unsafe { ConditionalChannels::new(&heap, true, None) };
            let counts = allocation_probe::finish_bytes();
            assert_eq!(heap.native_registry_bytes() + counts.3, ledger_before + counts.2, "old-owner scratch: count={count} per_owner={per_owner}");
            assert_eq!(Arc::strong_count(&heap.native_memory.registry), counter_owners);
            assert!(!groups.has_source_owners());
            assert_eq!(groups.initial_groups().len(), owners.len());
            assert!(
                owners
                    .iter()
                    .all(|&owner| groups.source_group(owner, 0).is_none())
            );
            let mut values = Vec::with_capacity(count);
            allocation_probe::start();
            for index in groups.initial_groups() {
                unsafe {
                    groups.scan(index, &mut |slot| {
                        values.push(((*slot) as *const u8).add(Full::SIZE).cast::<u64>().read());
                    });
                }
            }
            assert_eq!(allocation_probe::finish(), (0, 0));
            values.sort_unstable();
            assert_eq!(values, (0..count as u64).collect::<Vec<_>>());
            eprintln!(
                "OLD_OWNER_ALLOC count={count} per_owner={per_owner} owners={} counts={counts:?}",
                owners.len()
            );
            drop(groups);
            assert_eq!(heap.native_registry_bytes(), ledger_before);
            measurements.push((count, owners.len(), counts));
        }
    }
    for (count, owners, counts) in measurements {
        assert!(
            counts.0 <= 3 && counts.1 == usize::from(count != 0),
            "{counts:?}"
        );
        // One Arc per queue, at most three words per distinct group, and a
        // small constant allowance. Initial-only owners need no lookup table.
        let limit = count * std::mem::size_of::<Arc<ChanCtrl>>()
            + owners * 3 * std::mem::size_of::<usize>()
            + 256;
        assert!(
            counts.2 >= counts.3 && counts.2 - counts.3 <= limit,
            "{counts:?}"
        );
    }
}

#[test]
fn collector_channel_groups_reference_generation_partition() {
    use crate::gc::ConditionalChannels;
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new_generational::<Full>(4096, 16384, vec![info.clone()]);
    // Exclusive, quiescent fixture. Same-type owners in different generations
    // must remain distinguishable even when the type filter admits both.
    let old = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let young = unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) };
    assert!(!old.is_null() && !young.is_null());
    for owner in [old, young] {
        let mut registry = heap.channels.lock().unwrap();
        let handle = registry.register(queue(owner as u64, &heap.native_retirement), &heap.native_memory);
        registry.bind(
            handle,
            ChannelOwner {
                address: owner as usize,
                type_id: 0,
            },
        );
    }
    for minor in [false, true] {
        let groups = unsafe { ConditionalChannels::new(&heap, minor, None) };
        assert!(groups.has_source_owners());
        let (source, initial) = if minor { (young, old) } else { (old, young) };
        let source_index = groups.source_group(source, 0).unwrap();
        assert_eq!(groups.source_group(initial, 0), None);
        let mut initial_groups = groups.initial_groups();
        assert_eq!(initial_groups.len(), 1);
        let initial_index = initial_groups.next().unwrap();
        assert_eq!(initial_groups.len(), 0);
        assert_eq!(initial_groups.next(), None);
        for (index, owner) in [(source_index, source), (initial_index, initial)] {
            let mut values = Vec::new();
            unsafe {
                groups.scan(index, &mut |slot| values.push(*slot));
            }
            assert_eq!(values, vec![owner as u64]);
        }
    }
    let live = hashbrown::HashSet::with_hasher_in(
        std::hash::RandomState::new(), heap.native_memory.registry_allocator(),
    );
    let groups = unsafe { ConditionalChannels::new(&heap, false, Some(&live)) };
    assert!(groups.source_group(old, 0).is_some());
    assert_eq!(groups.source_group(young, 0), None);
    assert_eq!(groups.initial_groups().len(), 0);
}

#[test]
fn collector_channel_groups_reference_bounded_allocations_and_exact_edges() {
    use crate::gc::{ChannelEdges, ConditionalChannels};
    let mut measurements = Vec::new();
    for count in [0, 1, 32, 1024] {
        for per_owner in [1, 8, usize::MAX] {
            let shared = per_owner == usize::MAX;
            let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
            // Exclusive quiescent heap: no registered mutators or copy workers.
            let heap = Heap::new::<Full>(1 << 20, vec![info.clone()]);
            let mut owners = Vec::new();
            let mut observers = Vec::new();
            for index in 0..count {
                let owner = if index % per_owner != 0 {
                    *owners.last().unwrap()
                } else {
                    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
                    assert!(!owner.is_null());
                    owners.push(owner);
                    owner
                };
                let message = unsafe { heap.alloc_obj::<Full>(&info, 0) };
                assert!(!message.is_null());
                unsafe {
                    message.add(Full::SIZE).cast::<u64>().write(index as u64);
                }
                let control = queue(message as u64, &heap.native_retirement);
                observers.push(Arc::downgrade(&control));
                let mut registry = heap.channels.lock().unwrap();
                let handle = registry.register(control, &heap.native_memory);
                registry.bind(
                    handle,
                    ChannelOwner {
                        address: owner as usize,
                        type_id: 0,
                    },
                );
            }
            drop(heap.channels.lock().unwrap());
            let ledger_before = heap.native_registry_bytes();
            let counter_owners = Arc::strong_count(&heap.native_memory.registry);
            allocation_probe::start();
            let groups = unsafe { ConditionalChannels::new(&heap, false, None) };
            let conditional_counts = allocation_probe::finish_bytes();
            assert_eq!(heap.native_registry_bytes() + conditional_counts.3, ledger_before + conditional_counts.2, "conditional scratch: count={count} per_owner={per_owner}");
            assert_eq!(Arc::strong_count(&heap.native_memory.registry), counter_owners);
            let mut values = Vec::with_capacity(count);
            allocation_probe::start();
            for &owner in &owners {
                let index = groups.source_group(owner, 0).expect("source group missing");
                unsafe {
                    groups.scan(index, &mut |slot| {
                        let message = (*slot) as *const u8;
                        values.push(message.add(Full::SIZE).cast::<u64>().read());
                    });
                }
            }
            let scan_counts = allocation_probe::finish();
            assert_eq!(scan_counts, (0, 0));
            values.sort_unstable();
            assert_eq!(values, (0..count as u64).collect::<Vec<_>>());
            assert_eq!(groups.has_source_owners(), count != 0);
            assert!(observers.iter().all(|weak| weak.strong_count() == 2));
            drop(groups);
            assert_eq!(heap.native_registry_bytes(), ledger_before);
            assert!(observers.iter().all(|weak| weak.strong_count() == 1));

            allocation_probe::start();
            let edges = ChannelEdges::new(&heap);
            let edge_counts = allocation_probe::finish_bytes();
            assert_eq!(heap.native_registry_bytes() + edge_counts.3, ledger_before + edge_counts.2, "edge scratch: count={count} per_owner={per_owner}");
            assert_eq!(Arc::strong_count(&heap.native_memory.registry), counter_owners);
            for old_roots in [false, true] {
                values.clear();
                allocation_probe::start();
                let mut visit = |slot: *mut u64| unsafe {
                    values.push(((*slot) as *const u8).add(Full::SIZE).cast::<u64>().read());
                };
                unsafe {
                    if old_roots {
                        edges.scan_old_roots(&heap, &mut visit);
                    } else {
                        for &owner in &owners {
                            edges.trace(owner, &mut visit);
                        }
                    }
                }
                let scan_counts = allocation_probe::finish();
                assert_eq!(scan_counts, (0, 0));
                values.sort_unstable();
                assert_eq!(values, (0..count as u64).collect::<Vec<_>>());
            }
            assert!(observers.iter().all(|weak| weak.strong_count() == 2));
            drop(edges);
            assert_eq!(heap.native_registry_bytes(), ledger_before);
            assert!(observers.iter().all(|weak| weak.strong_count() == 1));
            eprintln!(
                "GROUP_ALLOC count={count} shared={shared} per_owner={per_owner} conditional={conditional_counts:?} edges={edge_counts:?}"
            );
            measurements.push((count, shared, conditional_counts, edge_counts));
        }
    }
    // A fixed number of contiguous buffers replaces per-owner allocations.
    for (count, shared, conditional, edges) in measurements {
        assert!(
            conditional.0 <= 5 && conditional.1 == usize::from(count != 0),
            "count={count}, shared={shared}: {conditional:?}"
        );
        assert!(
            edges.0 <= 3 && edges.1 == usize::from(count != 0),
            "count={count}, shared={shared}: {edges:?}"
        );
        if shared {
            // This fixture has one owner/type: retained per-channel storage
            // should be one Arc, not worst-case owner metadata per queue.
            let limit = count * std::mem::size_of::<Arc<ChanCtrl>>() + 256;
            assert!(conditional.2 >= conditional.3 && conditional.2 - conditional.3 <= limit);
            assert!(edges.2 >= edges.3 && edges.2 - edges.3 <= limit);
        }
        if count == 0 {
            assert_eq!((conditional, edges), ((0, 0, 0, 0), (0, 0, 0, 0)));
        }
    }
}

#[test]
fn collector_channel_owner_binding_preserves_allocation_reservation() {
    let retirement = native_retirement::Retirement::default();
    let memory = Arc::new(native_memory::NativeMemory::default());
    let mut registry = ChannelRegistry::default();
    for count in [1, 17, 257] {
        // First bind directly after registration, with no collection warming
        // the owner index. The second wave binds only after a sparse sweep.
        for sweep_first in [false, true] {
            let dead: Vec<_> = (0..count * 8)
                .map(|_| registry.register(queue(0, &retirement), &memory))
                .collect();
            let pending: Vec<_> = (0..count)
                .map(|_| registry.register(queue(0, &retirement), &memory))
                .collect();
            allocation_probe::start();
            for &handle in &dead {
                registry.bind(
                    handle,
                    ChannelOwner {
                        address: 8,
                        type_id: 1,
                    },
                );
            }
            let initial_counts = allocation_probe::finish();
            assert_eq!(initial_counts, (0, 0), "initial bind count={count}");
            if sweep_first {
                registry.sweep(|_| None);
                let native = Mutex::new(std::mem::take(&mut registry));
                registry_compaction::compact_channels(&native);
                registry = native.into_inner().unwrap();
                assert!(registry.owned.is_empty());
            }
            allocation_probe::start();
            for &handle in &pending {
                registry.bind(
                    handle,
                    ChannelOwner {
                        address: 16,
                        type_id: 1,
                    },
                );
            }
            let counts = allocation_probe::finish();
            assert_eq!(counts, (0, 0), "count={count}, sweep_first={sweep_first}");
            assert!(registry.unbound.is_empty());
            for handle in pending {
                assert_eq!(registry.records[&(handle as usize)].1, Some(1));
                assert_eq!(unsafe { &*handle }.owner.load(Ordering::Acquire), 16);
            }
            registry.sweep(|_| None);
            assert!(registry.is_empty());
            let native = Mutex::new(std::mem::take(&mut registry));
            registry_compaction::compact_channels(&native);
            registry = native.into_inner().unwrap();
            assert!(registry.owned.capacity() <= 8);
            retirement.drain();
            assert_eq!(memory.bytes(), 0);
        }
    }
}
struct Tagged;
impl PtrPolicy for Tagged {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        (bits != 0).then_some((bits & !3) as *mut u8)
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64 | 3
    }
}

fn child(name: &str) -> bool {
    if std::env::var_os("GCR_CHANNEL_OWNER_CHILD").is_some() {
        return true;
    }
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    for workers in [1, 4] {
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("runtime::channel_owner_tests::{name}"),
                "--nocapture",
            ])
            .env("GCR_CHANNEL_OWNER_CHILD", "1")
            .env("GCR_GC_WORKERS", workers.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while process.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                process.kill().unwrap();
                panic!(
                    "channel ownership hung: {:?}",
                    process.wait_with_output().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = process.wait_with_output().unwrap();
        assert!(output.status.success(), "workers={workers}: {output:?}");
    }
    false
}

pub(super) fn queue(message: u64, retirement: &native_retirement::Retirement) -> Arc<ChanCtrl> {
    queue_with_link(message, native_retirement::RecordLink::new(retirement))
}
pub(super) fn queue_with_link(message: u64, retirement: native_retirement::RecordLink<ChanCtrl>) -> Arc<ChanCtrl> {
    Arc::new(ChanCtrl {
        retirement,
        owner: std::sync::atomic::AtomicUsize::new(1),
        state: Mutex::new(ChanState {
            head: 0,
            tail: 0,
            count: 1,
            closed: false,
            senders: 1,
            waiting_senders: 0,
            waiting_receivers: 0,
        }),
        scan: channel_scan::QueueRange::populated(0, 1),
        not_empty: Condvar::new(),
        not_full: Condvar::new(),
        slots: RwLock::new(Some(native_memory::NativeSlots::new(
            vec![std::sync::atomic::AtomicU64::new(message)].into_boxed_slice(),
        ))),
        cap: 1,
        array_type: 0,
        message_type: 1,
    })
}

fn drain_native(heap: &Heap, state: &ThreadState) {
    unsafe { state.enter_blocked(); }
    heap.native_retirement.drain();
    unsafe { state.exit_blocked(heap); }
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

fn cycle<H: ObjHeader, P: PtrPolicy>(mode: usize, channel_count: usize) {
    let owner_info = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
    let message_info = TypeInfo::for_header(H::SIZE)
        .with_type_id(1)
        .with_fields(1)
        .with_raw_bytes(8);
    let heap =
        Heap::new_generational::<H>(1024, 8192, vec![owner_info.clone(), message_info.clone()]);
    heap.set_gc_every_alloc(matches!(mode, 2 | 4));
    let (state, _) = heap.register_thread();
    let allocate = |info: &TypeInfo| unsafe {
        let p = if matches!(mode, 2 | 4) {
            heap.alloc_nursery_obj::<H>(info, 0)
        } else {
            heap.alloc_obj::<H>(info, 0)
        };
        assert!(!p.is_null());
        p
    };
    let owner = allocate(&owner_info);
    let message = allocate(&message_info);
    unsafe {
        owner.add(H::SIZE).cast::<u64>().write(17);
        message
            .add(H::SIZE)
            .cast::<u64>()
            .write(P::encode_ptr(owner));
        message.add(H::SIZE + 8).cast::<u64>().write(42);
    }
    let mut controls = Vec::new();
    for _ in 0..channel_count {
        let q = queue(P::encode_ptr(message), &heap.native_retirement);
        let weak = Arc::downgrade(&q);
        let handle = heap
            .channels
            .lock()
            .unwrap()
            .register(q, &heap.native_memory);
        heap.channels.lock().unwrap().bind(
            handle,
            ChannelOwner {
                address: owner as usize,
                type_id: 0,
            },
        );
        controls.push((handle, weak));
    }
    let root = unsafe { heap.globals.add(P::encode_ptr(owner)) };
    if mode == 4 {
        unsafe {
            heap.mutator_triggered_gc::<P>(&state);
        }
    }
    for _ in 0..3 {
        collect::<P>(&heap, &state, mode);
        for &(handle, ref weak) in &controls {
            let saved = heap.channels.lock().unwrap().records[&(handle as usize)]
                .0
                .owner
                .load(Ordering::Acquire);
            assert_eq!(
                saved,
                P::try_decode_ptr(heap.globals.get(root)).unwrap() as usize
            );
            let live = weak.upgrade().expect("live channel reclaimed");
            let msg = P::try_decode_ptr(
                live.slots.read().unwrap().as_ref().unwrap()[0].load(Ordering::SeqCst),
            )
            .unwrap();
            let old = heap.from_base() as usize;
            assert!(
                (old..old + heap.from_used()).contains(&(msg as usize)) || heap.is_nursery(msg)
            );
            unsafe {
                assert_eq!(msg.add(H::SIZE + 8).cast::<u64>().read(), 42);
                assert_eq!(
                    msg.add(H::SIZE).cast::<u64>().read(),
                    heap.globals.get(root)
                );
            }
        }
    }
    unsafe {
        heap.globals.set(root, 0);
    }
    collect::<P>(&heap, &state, mode);
    if matches!(mode, 2 | 4) {
        assert!(
            controls.iter().all(|(_, weak)| weak.upgrade().is_some()),
            "minor decided old-owner death"
        );
        unsafe {
            heap.mutator_triggered_gc::<P>(&state);
        }
    }
    drain_native(&heap, &state);
    assert!(
        controls.iter().all(|(_, weak)| weak.upgrade().is_none()),
        "queued self-cycle retained channel"
    );
    assert_eq!(heap.from_used(), 0);
    unsafe {
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_channel_conditional_roots_follow_all_copy_paths() {
    if !child("collector_channel_conditional_roots_follow_all_copy_paths") {
        return;
    }
    for mode in 0..5 {
        cycle::<Full, IdentityPtrPolicy>(mode, 1);
        cycle::<Full, IdentityPtrPolicy>(mode, 33);
        cycle::<Compact, IdentityPtrPolicy>(mode, 1);
        cycle::<Compact, IdentityPtrPolicy>(mode, 33);
        cycle::<Full, Tagged>(mode, 1);
        cycle::<Full, Tagged>(mode, 33);
        cycle::<Compact, Tagged>(mode, 1);
        cycle::<Compact, Tagged>(mode, 33);
    }
}

fn chain<H: ObjHeader, P: PtrPolicy>(mode: usize) {
    let owner_info = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
    let message_info = TypeInfo::for_header(H::SIZE).with_type_id(1).with_fields(1);
    let heap = Heap::new::<H>(1 << 20, vec![owner_info.clone(), message_info.clone()]);
    let (state, _) = heap.register_thread();
    let n = 6000;
    let mut owners = Vec::new();
    for i in 0..n {
        let owner = unsafe { heap.alloc_obj::<H>(&owner_info, 0) };
        assert!(!owner.is_null());
        unsafe {
            owner.add(H::SIZE).cast::<usize>().write(i);
        }
        owners.push(owner);
    }
    let mut weak = Vec::new();
    for i in 0..n {
        let message = unsafe { heap.alloc_obj::<H>(&message_info, 0) };
        assert!(!message.is_null());
        unsafe {
            message
                .add(H::SIZE)
                .cast::<u64>()
                .write(P::encode_ptr(owners[(i + 1) % n]));
        }
        let q = queue(P::encode_ptr(message), &heap.native_retirement);
        weak.push(Arc::downgrade(&q));
        let handle = heap.channels.lock().unwrap().register(q, &heap.native_memory);
        heap.channels.lock().unwrap().bind(
            handle,
            ChannelOwner {
                address: owners[i] as usize,
                type_id: 0,
            },
        );
    }
    let root = unsafe { heap.globals.add(P::encode_ptr(owners[0])) };
    collect::<P>(&heap, &state, mode);
    assert!(weak.iter().all(|q| q.upgrade().is_some()));
    assert_eq!(
        heap.from_used(),
        n * (owner_info.allocation_size(0) + message_info.allocation_size(0))
    );
    unsafe {
        heap.globals.set(root, 0);
    }
    collect::<P>(&heap, &state, mode);
    drain_native(&heap, &state);
    assert!(weak.iter().all(|q| q.upgrade().is_none()));
    assert_eq!(heap.from_used(), 0);
    unsafe {
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_channel_worklist_traces_long_chains_and_reclaims_cycles() {
    if !child("collector_channel_worklist_traces_long_chains_and_reclaims_cycles") {
        return;
    }
    for mode in [1, 3] {
        chain::<Full, IdentityPtrPolicy>(mode);
        chain::<Compact, Tagged>(mode);
    }
}

#[test]
fn collector_channel_dead_cross_generation_cycle_does_not_force_promotion_oom() {
    if !child("collector_channel_dead_cross_generation_cycle_does_not_force_promotion_oom") {
        return;
    }
    let owner_info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let message_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_fields(1)
        .with_raw_bytes(8);
    let heap = Heap::new_generational::<Full>(
        1024,
        message_info.allocation_size(0),
        vec![owner_info.clone(), message_info.clone()],
    );
    let (state, _) = heap.register_thread();
    let owner = unsafe { heap.alloc_nursery_obj::<Full>(&owner_info, 0) };
    let message = unsafe { heap.alloc_obj::<Full>(&message_info, 0) };
    assert!(!owner.is_null() && !message.is_null());
    unsafe {
        owner.add(Full::SIZE).cast::<u64>().write(17);
        message.add(Full::SIZE).cast::<u64>().write(owner as u64);
    }
    heap.mark_card_dirty(unsafe { message.add(Full::SIZE) });
    let q = queue(message as u64, &heap.native_retirement);
    let weak = Arc::downgrade(&q);
    let handle = heap.channels.lock().unwrap().register(q, &heap.native_memory);
    heap.channels.lock().unwrap().bind(
        handle,
        ChannelOwner {
            address: owner as usize,
            type_id: 0,
        },
    );
    assert_eq!(heap.from_remaining(), 0);
    unsafe {
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state);
    }
    drain_native(&heap, &state);
    assert!(weak.upgrade().is_none());
    assert_eq!(heap.from_used(), 0);
    assert_eq!(heap.nursery_used(), 0);
    unsafe {
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_channel_promotion_preflight_counts_conditional_queue_survivors() {
    if !child("collector_channel_promotion_preflight_counts_conditional_queue_survivors") {
        return;
    }
    let owner_info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let message_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_fields(2)
        .with_raw_bytes(8);
    let data_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(2)
        .with_raw_bytes(8);
    let dead_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(3)
        .with_raw_bytes(32);
    // 72 old bytes occupy 96 bytes. Owner alone fits the remaining 24;
    // its conditional message needs a major, which must retain the old data
    // reachable only through that nursery message's second field.
    let heap = Heap::new_generational::<Full>(
        1024,
        96,
        vec![owner_info.clone(), message_info.clone(), data_info.clone(), dead_info.clone()],
    );
    let (state, _) = heap.register_thread();
    let data = unsafe { heap.alloc_obj::<Full>(&data_info, 0) };
    assert!(!unsafe { heap.alloc_obj::<Full>(&dead_info, 0) }.is_null());
    let owner = unsafe { heap.alloc_nursery_obj::<Full>(&owner_info, 0) };
    let message = unsafe { heap.alloc_nursery_obj::<Full>(&message_info, 0) };
    assert!(!owner.is_null() && !message.is_null() && !data.is_null());
    unsafe {
        owner.add(Full::SIZE).cast::<u64>().write(17);
        data.add(Full::SIZE).cast::<u64>().write(99);
        message.add(Full::SIZE).cast::<u64>().write(owner as u64);
        message.add(Full::SIZE + 8).cast::<u64>().write(data as u64);
        message.add(Full::SIZE + 16).cast::<u64>().write(42);
        heap.globals.add(owner as u64);
    }
    let q = queue(message as u64, &heap.native_retirement);
    let weak = Arc::downgrade(&q);
    let handle = heap.channels.lock().unwrap().register(q, &heap.native_memory);
    heap.channels.lock().unwrap().bind(
        handle,
        ChannelOwner {
            address: owner as usize,
            type_id: 0,
        },
    );
    assert_eq!(heap.from_remaining(), 24);
    unsafe {
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state);
    }
    assert!(heap.collections() >= 1);
    assert_eq!(heap.nursery_used(), 0);
    assert_eq!(heap.from_used(), 88);
    let q = weak.upgrade().expect("live conditional queue lost");
    let message = q.slots.read().unwrap().as_ref().unwrap()[0].load(Ordering::SeqCst) as *mut u8;
    assert_eq!(
        unsafe { message.add(Full::SIZE + 16).cast::<u64>().read() },
        42
    );
    let data = unsafe { message.add(Full::SIZE + 8).cast::<*mut u8>().read() };
    let base = heap.from_base() as usize;
    assert!(
        (base..base + heap.from_used()).contains(&(data as usize)),
        "conditional old data was lost"
    );
    assert_eq!(unsafe { data.add(Full::SIZE).cast::<u64>().read() }, 99);
    unsafe {
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_channel_snapshot_uses_owner_edges_not_queue_roots() {
    if !child("collector_channel_snapshot_uses_owner_edges_not_queue_roots") {
        return;
    }
    let owner_info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let message_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_fields(1);
    let heap = Heap::new::<Full>(4096, vec![owner_info.clone(), message_info.clone()]);
    let (state, _) = heap.register_thread();
    let owner = unsafe { heap.alloc_obj::<Full>(&owner_info, 0) };
    let message = unsafe { heap.alloc_obj::<Full>(&message_info, 0) };
    unsafe {
        owner.add(Full::SIZE).cast::<u64>().write(17);
        message.add(Full::SIZE).cast::<u64>().write(owner as u64);
    }
    let handle = heap
        .channels
        .lock()
        .unwrap()
        .register(queue(message as u64, &heap.native_retirement), &heap.native_memory);
    heap.channels.lock().unwrap().bind(
        handle,
        ChannelOwner {
            address: owner as usize,
            type_id: 0,
        },
    );
    let root = unsafe { heap.globals.add(owner as u64) };
    let live = unsafe { crate::gc::dump::dump_heap_json(&heap) };
    assert_eq!(live.matches("\"reachable\": true").count(), 2, "{live}");
    unsafe {
        heap.globals.set(root, 0);
    }
    let dead = unsafe { crate::gc::dump::dump_heap_json(&heap) };
    assert_eq!(dead.matches("\"reachable\": true").count(), 0, "{dead}");
    collect::<IdentityPtrPolicy>(&heap, &state, 1);
    unsafe {
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_channel_blocked_operations_pin_their_only_managed_owner() {
    if !child("collector_channel_blocked_operations_pin_their_only_managed_owner") {
        return;
    }
    for sending in [false, true] {
        let (ready, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let array = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
            let owner_info = TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_raw_bytes(8);
            let message_info = TypeInfo::for_header(Full::SIZE)
                .with_type_id(2)
                .with_raw_bytes(8);
            let mut context = RuntimeContext::new(8192, vec![array, owner_info, message_info]);
            let heap = context.heap().clone();
            let thread = context.thread_ptr();
            let execute = || {
            let state = context.thread.registration.dyna.clone();
            let roots = unsafe { state.scratch_scope() };
            let witness = unsafe { ai_gc_alloc_varlen(thread, 0, 0, 0) };
            unsafe {
                roots.push(witness);
            }
            let control;
            let pending;
            {
                let setup = unsafe { state.scratch_scope() };
                let owner = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
                unsafe {
                    owner.add(Full::SIZE).cast::<u64>().write(17);
                    setup.push(owner);
                }
                control = unsafe { ai_chan_new(thread, 1, witness, 2) };
                unsafe {
                    ai_chan_bind_owner(thread, control, owner);
                }
                if sending {
                    let first = unsafe { ai_gc_alloc_fixed(thread, 2, 0) };
                    unsafe {
                        first.add(Full::SIZE).cast::<u64>().write(1);
                        setup.push(first);
                    }
                    assert_eq!(unsafe { ai_chan_send(thread, witness, control, first) }, 0);
                    pending = unsafe { ai_gc_alloc_fixed(thread, 2, 0) };
                    unsafe {
                        pending.add(Full::SIZE).cast::<u64>().write(2);
                        setup.push(pending);
                    }
                } else {
                    pending = std::ptr::null_mut();
                }
            }
            // Retain native storage externally so a missing pin can be observed
            // as premature registry removal, without freeing a live condvar.
            let native = heap.channels.lock().unwrap().records[&(control as usize)]
                .0
                .clone();
            ready
                .send((heap.clone(), native, control as usize))
                .unwrap();
            let result = if sending {
                unsafe { ai_chan_send(thread, witness, control, pending) }
            } else {
                unsafe { ai_chan_recv(thread, witness, control) }.is_null() as i64
            };
            assert_eq!(result, 1);
            drop(roots);
            };
            unsafe { with_current_thread(thread, execute); }
            drop(context);
        });
        let (heap, native, key) = receive.recv().unwrap();
        loop {
            let state = native.state.lock().unwrap();
            let waiting = if sending {
                state.waiting_senders
            } else {
                state.waiting_receivers
            };
            drop(state);
            if waiting == 1 {
                break;
            }
            std::thread::yield_now();
        }
        unsafe {
            heap.stw_collect::<IdentityPtrPolicy>();
        }
        let retained = heap.channels.lock().unwrap().records.contains_key(&key);
        {
            let mut state = native.state.lock().unwrap();
            state.closed = true;
            native.not_empty.notify_all();
            native.not_full.notify_all();
        }
        worker.join().unwrap();
        assert!(retained, "blocked operation lost its only managed owner");
        assert!(heap.channels.lock().unwrap().is_empty());
    }
}
