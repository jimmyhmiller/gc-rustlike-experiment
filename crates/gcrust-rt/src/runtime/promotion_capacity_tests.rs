use super::*;
use crate::gc::ObjHeader;

#[test]
fn collector_dead_nursery_does_not_force_major_at_full_tenured_capacity() {
    let buffer = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
    let leaf = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    let mut runtime = RuntimeContext::new_generational(1024, 4096, vec![buffer, leaf]);
    let heap = runtime.heap().clone();
    let thread = runtime.thread_ptr();
    let execute = || {
        let old = unsafe { ai_gc_alloc_varlen(thread, 0, 4056, 0) };
        let root = unsafe { heap.globals.add(old as u64) };
        let dead = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
        assert!(!dead.is_null());
        assert_eq!(heap.from_remaining(), 16);
        assert!(heap.nursery_used() > 16);
        unsafe {
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
        }
        assert_eq!(heap.collections(), 0, "garbage occupancy forced a major");
        assert_eq!(heap.minor_collections(), 1);
        assert_eq!(heap.nursery_used(), 0);
        assert_eq!(heap.globals.get(root), old as u64);
        assert_eq!(heap.from_used(), 4080);
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}

#[test]
fn collector_precise_promotion_capacity_follows_remembered_cycles_and_deduplicates() {
    let owner = TypeInfo::for_header(Full::SIZE)
        .with_fields(1)
        .with_raw_bytes(4008);
    let node = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_fields(1)
        .with_raw_bytes(8);
    let mut runtime = RuntimeContext::new_generational(1024, 4096, vec![owner, node]);
    let heap = runtime.heap().clone();
    let thread = runtime.thread_ptr();
    let execute = || {
        let old = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
        let owner_root = unsafe { heap.globals.add(old as u64) };
        let first = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
        let temporary = unsafe { heap.globals.add(first as u64) };
        let second = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
        let first = heap.globals.get(temporary) as *mut u8;
        unsafe {
            first.add(Full::SIZE).cast::<*mut u8>().write(second);
            first.add(Full::SIZE + 8).cast::<u64>().write(17);
            second.add(Full::SIZE).cast::<*mut u8>().write(first);
            second.add(Full::SIZE + 8).cast::<u64>().write(23);
            slots::ai_slot_ref_store(thread, old, Full::SIZE, first);
            heap.globals.set(temporary, 0);
        }
        for _ in 0..3 {
            assert!(!unsafe { ai_gc_alloc_fixed(thread, 1, 0) }.is_null());
        }
        assert_eq!(heap.from_remaining(), 64);
        assert!(heap.nursery_used() > 64);
        unsafe {
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
        }
        assert_eq!(heap.collections(), 0);
        assert_eq!(heap.minor_collections(), 1);
        assert_eq!(heap.from_used(), 4096);
        let old = heap.globals.get(owner_root) as *mut u8;
        let first = unsafe { slots::ai_slot_bits64_load(thread, old, Full::SIZE) } as *mut u8;
        let second = unsafe { slots::ai_slot_bits64_load(thread, first, Full::SIZE) } as *mut u8;
        assert_eq!(
            unsafe { slots::ai_slot_bits64_load(thread, second, Full::SIZE) },
            first as u64
        );
        assert_eq!(
            unsafe { slots::ai_slot_bits64_load(thread, first, Full::SIZE + 8) },
            17
        );
        assert_eq!(
            unsafe { slots::ai_slot_bits64_load(thread, second, Full::SIZE + 8) },
            23
        );
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}

#[test]
fn collector_promotion_preflight_handles_tagged_cycles_and_required_major() {
    use crate::gc::{Compact, PtrPolicy};
    struct Tagged;
    impl PtrPolicy for Tagged {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            (bits & 7 == 5).then_some((bits & !7) as *mut u8)
        }
        fn encode_ptr(pointer: *mut u8) -> u64 {
            pointer as u64 | 5
        }
    }
    fn check<H: ObjHeader>() {
        let node = TypeInfo::for_header(H::SIZE)
            .with_type_id(1)
            .with_fields(1)
            .with_raw_bytes(8);
        let free = 2 * node.allocation_size(0);
        let old_extent = 4096 - free;
        let buffer = TypeInfo::for_header(H::SIZE).with_varlen_bytes(0);
        for live in [2, 3] {
            let heap = Heap::new_generational::<H>(1024, 4096, vec![buffer.clone(), node.clone()]);
            let (state, _) = heap.register_thread();
            let garbage = unsafe { heap.alloc_obj::<H>(&buffer, old_extent - H::SIZE - 8) };
            assert!(!garbage.is_null());
            let nodes: Vec<_> = (0..live)
                .map(|_| unsafe { heap.alloc_nursery_obj::<H>(&node, 0) })
                .collect();
            assert!(nodes.iter().all(|node| !node.is_null()));
            for (index, &object) in nodes.iter().enumerate() {
                unsafe {
                    object
                        .add(H::SIZE)
                        .cast::<u64>()
                        .write(Tagged::encode_ptr(nodes[(index + 1) % live]));
                    object.add(H::SIZE + 8).cast::<u64>().write(index as u64);
                }
            }
            for _ in 0..3 {
                assert!(!unsafe { heap.alloc_nursery_obj::<H>(&node, 0) }.is_null());
            }
            let root = unsafe { heap.globals.add(Tagged::encode_ptr(nodes[0])) };
            let alias = unsafe { heap.globals.add(Tagged::encode_ptr(nodes[0])) };
            assert_eq!(heap.from_remaining(), free);
            assert!(heap.nursery_used() > free);
            unsafe {
                heap.mutator_triggered_minor_gc::<Tagged>(&state);
            }
            assert_eq!(heap.collections(), usize::from(live == 3));
            assert_eq!(heap.minor_collections(), 1);
            assert_eq!(
                heap.from_used(),
                if live == 2 {
                    4096
                } else {
                    live * node.allocation_size(0)
                }
            );
            assert_eq!(heap.globals.get(root), heap.globals.get(alias));
            let start = Tagged::try_decode_ptr(heap.globals.get(root)).unwrap();
            let mut current = start;
            for index in 0..live {
                assert!(heap.is_tenured(current));
                assert_eq!(
                    unsafe { current.add(H::SIZE + 8).cast::<u64>().read() },
                    index as u64
                );
                let bits = unsafe { current.add(H::SIZE).cast::<u64>().read() };
                assert_eq!(bits & 7, 5);
                current = Tagged::try_decode_ptr(bits).unwrap();
            }
            assert_eq!(current, start);
            unsafe {
                heap.safe_deregister_thread(&state);
            }
        }
    }
    check::<Compact>();
    check::<Full>();
}

#[test]
fn collector_promotion_fallback_ignores_dead_nursery_old_back_edges() {
    const CHILD: &str = "GCR_PROMOTION_BACK_EDGE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::promotion_capacity_tests::collector_promotion_fallback_ignores_dead_nursery_old_back_edges", "--nocapture"])
            .env(CHILD, "1").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!(
                    "promotion back-edge collection hung: {:?}",
                    child.wait_with_output().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    use crate::gc::{Compact, PtrPolicy};
    struct Tagged;
    impl PtrPolicy for Tagged {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            (bits & 7 == 5).then_some((bits & !7) as *mut u8)
        }
        fn encode_ptr(pointer: *mut u8) -> u64 {
            pointer as u64 | 5
        }
    }
    fn check<H: ObjHeader, P: PtrPolicy>() {
        let buffer = TypeInfo::for_header(H::SIZE).with_varlen_bytes(0);
        let node = TypeInfo::for_header(H::SIZE)
            .with_type_id(1)
            .with_fields(1)
            .with_raw_bytes(8);
        let size = node.allocation_size(0);
        let heap = Heap::new_generational::<H>(1024, 4096, vec![buffer.clone(), node.clone()]);
        let (state, _) = heap.register_thread();
        let garbage = unsafe { heap.alloc_obj::<H>(&buffer, 4096 - 3 * size - H::SIZE - 8) };
        let old = unsafe { heap.alloc_obj::<H>(&node, 0) };
        let dead_wrapper = unsafe { heap.alloc_nursery_obj::<H>(&node, 0) };
        let young: Vec<_> = (0..3)
            .map(|_| unsafe { heap.alloc_nursery_obj::<H>(&node, 0) })
            .collect();
        assert!(!garbage.is_null() && !old.is_null() && !dead_wrapper.is_null());
        assert!(young.iter().all(|object| !object.is_null()));
        unsafe {
            dead_wrapper
                .add(H::SIZE)
                .cast::<u64>()
                .write(P::encode_ptr(garbage));
            old.add(H::SIZE)
                .cast::<u64>()
                .write(P::encode_ptr(young[0]));
            old.add(H::SIZE + 8).cast::<u64>().write(0);
            for (index, &object) in young.iter().enumerate() {
                let next = if index == 2 { old } else { young[index + 1] };
                object.add(H::SIZE).cast::<u64>().write(P::encode_ptr(next));
                object
                    .add(H::SIZE + 8)
                    .cast::<u64>()
                    .write(index as u64 + 1);
            }
        }
        heap.mark_card_dirty(old);
        let root = unsafe { heap.globals.add(P::encode_ptr(old)) };
        assert_eq!(heap.from_remaining(), 2 * size);
        // Three reachable young nodes require reclamation. The unrooted young
        // wrapper must not keep the large old buffer alive during that major.
        unsafe {
            heap.mutator_triggered_minor_gc::<P>(&state);
        }
        assert_eq!(heap.collections(), 1);
        assert_eq!(heap.minor_collections(), 1);
        assert_eq!(heap.nursery_used(), 0);
        assert_eq!(heap.from_used(), 4 * size);
        let events = heap.gc_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind.as_str(), "major");
        assert_eq!(events[0].before_bytes, (4096 - 2 * size) as u64);
        assert_eq!(events[0].after_bytes, size as u64);
        assert_eq!(events[1].kind.as_str(), "minor");
        assert_eq!(events[1].before_bytes, (4 * size) as u64);
        assert_eq!(events[1].after_bytes, 0);
        assert_eq!(events[1].promoted_bytes, (3 * size) as u64);
        let start = P::try_decode_ptr(heap.globals.get(root)).unwrap();
        let mut current = start;
        for id in 0..4 {
            assert!(heap.is_tenured(current));
            assert_eq!(unsafe { current.add(H::SIZE + 8).cast::<u64>().read() }, id);
            current =
                P::try_decode_ptr(unsafe { current.add(H::SIZE).cast::<u64>().read() }).unwrap();
        }
        assert_eq!(current, start);
        unsafe {
            heap.safe_deregister_thread(&state);
        }
    }
    check::<Compact, IdentityPtrPolicy>();
    check::<Full, IdentityPtrPolicy>();
    check::<Compact, Tagged>();
    check::<Full, Tagged>();
}
