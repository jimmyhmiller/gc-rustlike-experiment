use super::*;
use crate::gc::ObjHeader;

#[test]
fn collector_oversized_runtime_objects_skip_nursery_and_keep_remembered_edges() {
    let fixed = TypeInfo::for_header(Full::SIZE)
        .with_fields(1)
        .with_raw_bytes(2048);
    let bytes = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_varlen_bytes(0);
    let leaf = TypeInfo::for_header(Full::SIZE)
        .with_type_id(2)
        .with_raw_bytes(8);
    let values = TypeInfo::for_header(Full::SIZE)
        .with_type_id(3)
        .with_varlen_values(0);
    let mut runtime =
        RuntimeContext::new_generational(1024, 16384, vec![fixed.clone(), bytes.clone(), leaf.clone(), values.clone()]);
    let heap = runtime.heap().clone();
    let thread = runtime.thread_ptr();
    let execute = || {
        let young = unsafe { ai_gc_alloc_fixed(thread, 2, 0) };
        unsafe {
            young.add(Full::SIZE).cast::<u64>().write(42);
        }
        let young_root = unsafe { heap.globals.add(young as u64) };
        let array_young = unsafe { ai_gc_alloc_fixed(thread, 2, 0) };
        unsafe {
            array_young.add(Full::SIZE).cast::<u64>().write(84);
        }
        let array_young_root = unsafe { heap.globals.add(array_young as u64) };
        let nursery_used = heap.nursery_used();
        let raw = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
        let young = heap.globals.get(young_root) as *mut u8;
        // Model generated constructor initialization: plain pointer stores, no
        // per-field barrier, before a safepoint or exposure to another mutator.
        unsafe {
            raw.add(Full::SIZE).cast::<*mut u8>().write(young);
        }
        let raw_root = unsafe { heap.globals.add(raw as u64) };
        unsafe {
            raw.add(fixed.raw_data_offset() + 2047).write(0x51);
        }
        let text = unsafe { ai_gc_alloc_varlen(thread, 1, 2048, 0) };
        let text_root = unsafe { heap.globals.add(text as u64) };
        unsafe {
            text.add(bytes.varlen_element_offset(2047)).write(0x73);
        }
        let array = unsafe { ai_gc_alloc_varlen(thread, 3, 256, 0) };
        let array_root = unsafe { heap.globals.add(array as u64) };
        assert_eq!(heap.minor_collections(), 0);
        assert_eq!(heap.collections(), 0);
        assert_eq!(heap.nursery_used(), nursery_used);
        assert_eq!(heap.globals.get(young_root), young as u64);
        for object in [raw, text, array] {
            assert!(heap.is_tenured(object));
        }
        assert!(heap.is_nursery(young));
        // A later-card old-to-young edge must remain visible even though the large
        // owner bypassed the nursery allocation path entirely.
        unsafe {
            slots::ai_slot_ref_store(
                thread,
                array,
                values.varlen_element_offset(255),
                array_young,
            );
            heap.globals.set(young_root, 0);
            heap.globals.set(array_young_root, 0);
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
        }
        let raw = heap.globals.get(raw_root) as *mut u8;
        let raw_child = unsafe { slots::ai_slot_bits64_load(thread, raw, Full::SIZE) } as *mut u8;
        assert!(heap.is_tenured(raw_child));
        assert_eq!(
            unsafe { slots::ai_slot_bits64_load(thread, raw_child, Full::SIZE) },
            42
        );
        let array = heap.globals.get(array_root) as *mut u8;
        let promoted =
            unsafe { slots::ai_slot_bits64_load(thread, array, values.varlen_element_offset(255)) }
                as *mut u8;
        assert!(heap.is_tenured(promoted));
        assert_eq!(
            unsafe { slots::ai_slot_bits64_load(thread, promoted, Full::SIZE) },
            84
        );
        unsafe {
            heap.mutator_triggered_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
        }
        let raw = heap.globals.get(raw_root) as *const u8;
        let text = heap.globals.get(text_root) as *const u8;
        let array = heap.globals.get(array_root) as *const u8;
        assert_eq!(
            unsafe { raw.add(fixed.raw_data_offset() + 2047).read() },
            0x51
        );
        assert_eq!(
            unsafe { text.add(bytes.varlen_element_offset(2047)).read() },
            0x73
        );
        assert_eq!(
            unsafe { text.add(bytes.varlen_count_offset()).cast::<u64>().read() },
            2048
        );
        assert_eq!(
            unsafe { array.add(values.varlen_count_offset()).cast::<u64>().read() },
            256
        );
        assert_eq!(
            heap.from_used(),
            fixed.allocation_size(0)
                + bytes.allocation_size(2048)
                + values.allocation_size(256)
                + 2 * leaf.allocation_size(0)
        );
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}

#[test]
fn collector_oversized_runtime_allocation_collects_only_when_tenured_is_full() {
    let bytes = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
    let mut runtime = RuntimeContext::new_generational(1024, 4096, vec![bytes.clone()]);
    let heap = runtime.heap().clone();
    let thread = runtime.thread_ptr();
    let execute = || {
        for value in [17u8, 23] {
            let object = unsafe { ai_gc_alloc_varlen(thread, 0, 2048, 0) };
            unsafe {
                object.add(bytes.varlen_element_offset(2047)).write(value);
            }
            assert!(heap.is_tenured(object));
            assert_eq!(
                unsafe { object.add(bytes.varlen_count_offset()).cast::<u64>().read() },
                2048
            );
            assert_eq!(heap.from_used(), bytes.allocation_size(2048));
        }
        assert_eq!(heap.collections(), 1);
        assert_eq!(heap.minor_collections(), 0);
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}

#[test]
fn collector_oversized_runtime_allocation_preserves_explicit_stress_collection() {
    let bytes = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
    let leaf = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    let mut runtime = RuntimeContext::new_generational(1024, 8192, vec![bytes, leaf]);
    let heap = runtime.heap().clone();
    let thread = runtime.thread_ptr();
    let execute = || {
        let young = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
        let root = unsafe { heap.globals.add(young as u64) };
        heap.set_gc_every_alloc(true);
        let object = unsafe { ai_gc_alloc_varlen(thread, 0, 2048, 0) };
        assert!(heap.is_tenured(object));
        assert!(heap.is_tenured(heap.globals.get(root) as *const u8));
        assert_eq!(heap.minor_collections(), 1);
        assert_eq!(heap.collections(), 0);
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}

#[test]
fn collector_oversized_runtime_reclaims_dead_nursery_back_edges_before_major() {
    const CHILD: &str = "GCR_LARGE_NURSERY_BACK_EDGE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::large_allocation_tests::collector_oversized_runtime_reclaims_dead_nursery_back_edges_before_major", "--nocapture"])
            .env(CHILD, "1").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!(
                    "large allocation back-edge cleanup hung: {:?}",
                    child.wait_with_output().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    let bytes = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
    let wrapper = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_fields(1);
    let mut runtime = RuntimeContext::new_generational(1024, 4096, vec![bytes.clone(), wrapper]);
    let heap = runtime.heap().clone();
    let thread = runtime.thread_ptr();
    let execute = || {
        let first = unsafe { ai_gc_alloc_varlen(thread, 0, 2048, 0) };
        let root = unsafe { heap.globals.add(first as u64) };
        let dead_wrapper = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
        let first = heap.globals.get(root) as *mut u8;
        unsafe {
            dead_wrapper.add(Full::SIZE).cast::<*mut u8>().write(first);
            heap.globals.set(root, 0);
        }
        // The wrapper is not rooted. A major alone conservatively scans nursery
        // contents and would retain the first buffer, preventing the second from
        // fitting. Exhaustion must scavenge the nursery before tenured reclamation.
        let second = unsafe { ai_gc_alloc_varlen(thread, 0, 2048, 0) };
        assert!(heap.is_tenured(second));
        assert_eq!(heap.minor_collections(), 1);
        assert_eq!(heap.collections(), 1);
        assert_eq!(heap.nursery_used(), 0);
        assert_eq!(heap.from_used(), bytes.allocation_size(2048));
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}

#[test]
fn collector_runtime_nursery_boundary_uses_rounded_full_object_extent() {
    let bytes = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
    // Header/count overhead and padding are part of placement, not just the
    // requested payload. The largest fitting payload here is exactly 1000.
    for (length, extent, tenured) in [
        (0, 24, false),
        (999, 1024, false),
        (1000, 1024, false),
        (1001, 1032, true),
    ] {
        let mut runtime = RuntimeContext::new_generational(1024, 4096, vec![bytes.clone()]);
        let heap = runtime.heap().clone();
        let thread = runtime.thread_ptr();
        let execute = || {
            let object = unsafe { ai_gc_alloc_varlen(thread, 0, length, 0) };
            assert_eq!(bytes.allocation_size(length as usize), extent);
            assert_eq!(heap.is_tenured(object), tenured, "payload length {length}");
            assert_eq!(heap.is_nursery(object), !tenured);
            assert_eq!(
                unsafe { object.add(bytes.varlen_count_offset()).cast::<u64>().read() },
                length
            );
            if length > 0 {
                assert_eq!(
                    unsafe {
                        object
                            .add(bytes.varlen_element_offset(length as usize - 1))
                            .read()
                    },
                    0
                );
            }
            assert_eq!(heap.minor_collections(), 0);
            assert_eq!(heap.collections(), 0);
        };
        unsafe { crate::runtime::with_current_thread(thread, execute) }
    }
}

#[test]
fn collector_tenured_constructor_preserves_embedded_reference_without_leading_slots() {
    let owner = TypeInfo::for_header(Full::SIZE)
        .with_raw_bytes(2048)
        .with_interior_ptrs(&[2048]);
    let leaf = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    let mut runtime = RuntimeContext::new_generational(1024, 8192, vec![owner.clone(), leaf.clone()]);
    let heap = runtime.heap().clone();
    let thread = runtime.thread_ptr();
    let execute = || {
        let child = unsafe { ai_gc_alloc_fixed(thread, 1, 0) };
        unsafe {
            child.add(Full::SIZE).cast::<u64>().write(73);
        }
        let child_root = unsafe { heap.globals.add(child as u64) };
        let object = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
        let child = heap.globals.get(child_root) as *mut u8;
        // A flattened value capture can have no leading pointer slots at all.
        // Its late raw-region pointer still requires initial owner remembering.
        unsafe {
            object.add(2048).cast::<*mut u8>().write(child);
        }
        let owner_root = unsafe { heap.globals.add(object as u64) };
        assert_eq!(heap.minor_collections(), 0);
        assert_eq!(heap.collections(), 0);
        assert!(heap.is_nursery(child));
        assert!(heap.is_tenured(object));
        unsafe {
            heap.globals.set(child_root, 0);
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
        }
        for _ in 0..3 {
            let object = heap.globals.get(owner_root) as *mut u8;
            let child = unsafe { slots::ai_slot_bits64_load(thread, object, 2048) } as *mut u8;
            assert!(heap.is_tenured(child));
            assert_eq!(
                unsafe { slots::ai_slot_bits64_load(thread, child, Full::SIZE) },
                73
            );
            assert_eq!(
                heap.from_used(),
                owner.allocation_size(0) + leaf.allocation_size(0)
            );
            unsafe {
                heap.mutator_triggered_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
            }
        }
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}
