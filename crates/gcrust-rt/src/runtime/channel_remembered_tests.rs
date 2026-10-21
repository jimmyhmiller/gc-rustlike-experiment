//! Remembered native queue references and their interaction with heap cards.
use super::*;
use crate::gc::{ChannelEdges, ConditionalChannels, Full, ObjHeader};

fn child(name: &str) -> bool {
    if std::env::var_os("GCR_CHANNEL_REMEMBERED_CHILD").is_some() {
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
                &format!("runtime::channel_remembered_tests::{name}"),
                "--nocapture",
            ])
            .env("GCR_CHANNEL_REMEMBERED_CHILD", "1")
            .env("GCR_GC_WORKERS", workers.to_string())
            .env("GCR_GC_VERIFY", "1")
            .env("GCRUST_POISON", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while process.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                process.kill().unwrap();
                panic!(
                    "channel remembered-set stalled: {:?}",
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
fn runtime() -> RuntimeContext {
    RuntimeContext::new_generational(
        1 << 20,
        4 << 20,
        vec![
            TypeInfo::for_header(Full::SIZE).with_varlen_values(0),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_raw_bytes(8),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(2)
                .with_fields(1)
                .with_raw_bytes(8),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(3)
                .with_raw_bytes(8),
        ],
    )
}
unsafe fn queued(channel: *const ChanCtrl, index: usize) -> *mut u8 {
    unsafe {
        (*channel).slots.read().unwrap().as_ref().unwrap()[index].load(Ordering::SeqCst) as *mut u8
    }
}
unsafe fn message(thread: *mut Thread, tag: u64) -> *mut u8 {
    let value = unsafe { ai_gc_alloc_fixed(thread, 2, 0) };
    unsafe {
        value.add(Full::SIZE).cast::<u64>().write(0);
        value.add(Full::SIZE + 8).cast::<u64>().write(tag);
    }
    value
}

#[test]
fn collector_channel_remembered_reference_clean_minor_and_heap_cards() {
    if !child("collector_channel_remembered_reference_clean_minor_and_heap_cards") {
        return;
    }
    let mut context = runtime();
    let heap = context.heap().clone();
    context.enter(|thread| unsafe {
        let state = &*(*thread).dyna_thread;
        let roots = state.scratch_scope();
        let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
        let owner = roots.push(ai_gc_alloc_fixed(thread, 1, 0));
        let channel = ai_chan_new(thread, 2, roots.get(witness), 2);
        ai_chan_bind_owner(thread, channel, roots.get(owner));
        assert!(!(*channel).needs_minor_scan());
        let fresh = message(thread, 11);
        assert_eq!(ai_chan_send(thread, roots.get(witness), channel, fresh), 0);
        assert!((*channel).needs_minor_scan());
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert!(!(*channel).needs_minor_scan());
        assert!(!heap.is_nursery(queued(channel, 0)));
        // Clean direct references do not suppress a young child stored into
        // an old queued message. The ordinary object/card barrier owns it.
        let young = ai_gc_alloc_fixed(thread, 3, 0);
        young.add(Full::SIZE).cast::<u64>().write(55);
        ai_slot_ref_store(thread, queued(channel, 0), Full::SIZE, young);
        assert!(!(*channel).needs_minor_scan());
        {
            let _pause = heap.pause_world();
            allocation_probe::start();
            let groups = ConditionalChannels::new(&heap, true, None);
            let counts = allocation_probe::finish_bytes();
            assert_eq!(counts, (0, 0, 0, 0));
            assert!(!groups.has_source_owners());
            assert_eq!(groups.initial_groups().len(), 0);
            // Full collection and read-only graph walks must retain all edges.
            let owner_address = (*channel).owner.load(Ordering::Acquire) as *mut u8;
            let major = ConditionalChannels::new(&heap, false, None);
            let index = major.source_group(owner_address, 1).unwrap();
            let mut values = Vec::new();
            major.scan(index, &mut |slot| values.push(*slot));
            assert_eq!(values, vec![queued(channel, 0) as u64]);
            values.clear();
            ChannelEdges::new(&heap).trace(owner_address, &mut |slot| values.push(*slot));
            assert_eq!(values, vec![queued(channel, 0) as u64]);
        }
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        let leaf = queued(channel, 0).add(Full::SIZE).cast::<u64>().read() as *mut u8;
        assert!(!heap.is_nursery(leaf));
        assert_eq!(leaf.add(Full::SIZE).cast::<u64>().read(), 55);
        heap.mutator_triggered_gc::<IdentityPtrPolicy>(state);
        assert_eq!(
            queued(channel, 0).add(Full::SIZE + 8).cast::<u64>().read(),
            11
        );
        let leaf = queued(channel, 0).add(Full::SIZE).cast::<u64>().read() as *mut u8;
        assert_eq!(leaf.add(Full::SIZE).cast::<u64>().read(), 55);
        let old = ai_chan_recv(thread, roots.get(witness), channel);
        assert_eq!(ai_chan_send(thread, roots.get(witness), channel, old), 0);
        assert!(
            !(*channel).needs_minor_scan(),
            "old send dirtied a clean queue"
        );
        let fresh = message(thread, 99);
        assert_eq!(ai_chan_send(thread, roots.get(witness), channel, fresh), 0);
        assert!((*channel).needs_minor_scan());
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert!(!(*channel).needs_minor_scan());
        for tag in [11, 99] {
            let value = ai_chan_recv(thread, roots.get(witness), channel);
            assert!(!heap.is_nursery(value));
            assert_eq!(value.add(Full::SIZE + 8).cast::<u64>().read(), tag);
        }
        ai_chan_close(thread, channel);
        assert_eq!(heap.native_buffer_bytes(), 0);
    });
}

#[test]
fn collector_channel_remembered_reference_unbound_then_bound() {
    if !child("collector_channel_remembered_reference_unbound_then_bound") {
        return;
    }
    let mut context = runtime();
    let heap = context.heap().clone();
    context.enter(|thread| unsafe {
        let state = &*(*thread).dyna_thread;
        let roots = state.scratch_scope();
        let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
        let channel = ai_chan_new(thread, 1, roots.get(witness), 2);
        let fresh = message(thread, 42);
        ai_chan_send(thread, roots.get(witness), channel, fresh);
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert_eq!(
            queued(channel, 0).add(Full::SIZE + 8).cast::<u64>().read(),
            42
        );
        assert!(
            !(*channel).needs_minor_scan(),
            "completed raw minor stayed dirty"
        );
        let owner = roots.push(ai_gc_alloc_fixed(thread, 1, 0));
        ai_chan_bind_owner(thread, channel, roots.get(owner));
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert!(!(*channel).needs_minor_scan());
        let value = ai_chan_recv(thread, roots.get(witness), channel);
        assert_eq!(value.add(Full::SIZE + 8).cast::<u64>().read(), 42);
        let old_message = roots.push(value);
        ai_chan_close(thread, channel);
        let clean = ai_chan_new(thread, 1, roots.get(witness), 2);
        ai_chan_send(thread, roots.get(witness), clean, roots.get(old_message));
        assert!(!(*clean).needs_minor_scan());
        let young_owner = roots.push(ai_gc_alloc_fixed(thread, 1, 0));
        assert!(heap.is_nursery(roots.get(young_owner)));
        ai_chan_bind_owner(thread, clean, roots.get(young_owner));
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert_eq!(
            (*clean).owner.load(Ordering::Acquire),
            roots.get(young_owner) as usize
        );
        assert!(!heap.is_nursery(roots.get(young_owner)));
        assert!(!(*clean).needs_minor_scan());
        let value = ai_chan_recv(thread, roots.get(witness), clean);
        assert_eq!(value.add(Full::SIZE + 8).cast::<u64>().read(), 42);
        ai_chan_close(thread, clean);

        // A clean queue must not turn its unreachable young owner into a root.
        let dead = ai_chan_new(thread, 1, roots.get(witness), 2);
        ai_chan_send(thread, roots.get(witness), dead, roots.get(old_message));
        let unrooted_owner = ai_gc_alloc_fixed(thread, 1, 0);
        ai_chan_bind_owner(thread, dead, unrooted_owner);
        assert!(!(*dead).needs_minor_scan());
        let weak = Arc::downgrade(&heap.channels.lock().unwrap().records[&(dead as usize)].0);
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert!(!heap.channels.lock().unwrap().records.contains_key(&(dead as usize)));
        blocking_region(thread, || heap.native_retirement.drain());
        assert!(weak.upgrade().is_none());
        assert_eq!(heap.native_buffer_bytes(), 0);
    });
}

#[test]
fn collector_channel_remembered_reference_major_keeps_young_mark() {
    if !child("collector_channel_remembered_reference_major_keeps_young_mark") {
        return;
    }
    let mut context = runtime();
    let heap = context.heap().clone();
    context.enter(|thread| unsafe {
        let state = &*(*thread).dyna_thread;
        let roots = state.scratch_scope();
        let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
        let owner = roots.push(ai_gc_alloc_fixed(thread, 1, 0));
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        let channel = ai_chan_new(thread, 1, roots.get(witness), 2);
        ai_chan_bind_owner(thread, channel, roots.get(owner));
        let fresh = message(thread, 77);
        ai_chan_send(thread, roots.get(witness), channel, fresh);
        blocking_region(thread, || heap.stw_collect::<IdentityPtrPolicy>());
        assert!(heap.is_nursery(queued(channel, 0)));
        assert!(
            (*channel).needs_minor_scan(),
            "major cleared a live nursery edge"
        );
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert!(!(*channel).needs_minor_scan());
        let value = ai_chan_recv(thread, roots.get(witness), channel);
        assert!(!heap.is_nursery(value));
        assert_eq!(value.add(Full::SIZE + 8).cast::<u64>().read(), 77);
        ai_chan_close(thread, channel);
    });
}

#[test]
fn collector_channel_remembered_reference_unbound_minor_and_diagnostics() {
    if !child("collector_channel_remembered_reference_unbound_minor_and_diagnostics") {
        return;
    }
    let mut context = runtime();
    let heap = context.heap().clone();
    context.enter(|thread| unsafe {
        let state = &*(*thread).dyna_thread;
        let roots = state.scratch_scope();
        let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
        let channel = ai_chan_new(thread, 2, roots.get(witness), 2);
        let fresh = message(thread, 42);
        ai_chan_send(thread, roots.get(witness), channel, fresh);
        assert!((*channel).needs_minor_scan());
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert!(!(*channel).needs_minor_scan());
        {
            let _pause = heap.pause_world();
            let mut minor_slots = 0;
            allocation_probe::start();
            heap.channels
                .lock()
                .unwrap()
                .scan_unbound_roots(&mut |_| minor_slots += 1, true);
            let counts = allocation_probe::finish_bytes();
            assert_eq!(counts, (0, 0, 0, 0));
            assert_eq!(minor_slots, 0);
            let mut full_slots = Vec::new();
            heap.channels
                .lock()
                .unwrap()
                .scan_unbound_roots(&mut |slot| full_slots.push(*slot), false);
            assert_eq!(full_slots, vec![queued(channel, 0) as u64]);
            let expected = queued(channel, 0);
            let mut visible = false;
            heap.visit_roots(&mut |value| visible |= value == expected);
            assert!(visible, "diagnostics lost a clean unconditional queue root");
        }
        let leaf = ai_gc_alloc_fixed(thread, 3, 0);
        leaf.add(Full::SIZE).cast::<u64>().write(55);
        ai_slot_ref_store(thread, queued(channel, 0), Full::SIZE, leaf);
        assert!(!(*channel).needs_minor_scan());
        super::channel_root_probe::start();
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert_eq!(super::channel_root_probe::finish(), 1,
            "minor evacuation must use the minor root enumerator");
        let leaf = queued(channel, 0).add(Full::SIZE).cast::<u64>().read() as *mut u8;
        assert!(!heap.is_nursery(leaf));
        assert_eq!(leaf.add(Full::SIZE).cast::<u64>().read(), 55);
        super::channel_root_probe::start();
        heap.mutator_triggered_gc::<IdentityPtrPolicy>(state);
        assert_eq!(super::channel_root_probe::finish(), 0,
            "major evacuation must retain complete root enumeration");
        assert_eq!(
            queued(channel, 0).add(Full::SIZE + 8).cast::<u64>().read(),
            42
        );
        let leaf = queued(channel, 0).add(Full::SIZE).cast::<u64>().read() as *mut u8;
        assert_eq!(leaf.add(Full::SIZE).cast::<u64>().read(), 55);
        assert!(!(*channel).needs_minor_scan());
        let fresh = message(thread, 99);
        ai_chan_send(thread, roots.get(witness), channel, fresh);
        assert!((*channel).needs_minor_scan());
        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
        assert!(!(*channel).needs_minor_scan());
        for tag in [42, 99] {
            let value = ai_chan_recv(thread, roots.get(witness), channel);
            assert!(!heap.is_nursery(value));
            assert_eq!(value.add(Full::SIZE + 8).cast::<u64>().read(), tag);
        }
        ai_chan_close(thread, channel);
        assert!(
            heap.channels
                .lock()
                .unwrap()
                .unbound
                .contains(&(channel as usize))
        );
        assert_eq!(heap.native_buffer_bytes(), 0);
    });
}
