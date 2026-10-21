//! Cross custom header geometry with pointer encodings and every slot shape.
use super::custom_header_tests::OffsetHeader;
use super::*;
use crate::gc::IdentityPtrPolicy;

struct LowTag;
impl PtrPolicy for LowTag {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        (bits & 7 == 3).then_some((bits & !7) as *mut u8)
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64 | 3
    }
}
struct HighTag;
impl PtrPolicy for HighTag {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        (bits >> 48 == 0x7ffc).then_some((bits & 0x0000_ffff_ffff_ffff) as *mut u8)
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64 | 0x7ffc_0000_0000_0000
    }
}

const NODES: usize = 256;
const ELEMENTS: usize = 6;

fn graph<H: ObjHeader, P: PtrPolicy>(mode: u8) {
    let info = TypeInfo::for_header(H::SIZE)
        .with_type_id(1)
        .with_fields(2)
        .with_raw_bytes(16)
        .with_varlen_values(2)
        .with_owned_interior_ptrs(vec![(H::SIZE + 16) as u16]);
    let table = vec![info.clone().with_type_id(0), info.clone()];
    let heap = if mode == 1 {
        Heap::new_generational::<H>(1 << 18, 1 << 18, table)
    } else {
        Heap::new::<H>(1 << 18, table)
    };
    let interior = info.raw_data_offset();
    let payload = interior + 8;
    let elements = info.varlen_count_offset() + 8;
    let mut slots = vec![
        info.value_field_offset(0),
        info.value_field_offset(1),
        interior,
    ];
    slots.extend((0..ELEMENTS).map(|i| elements + i * 8));
    let null = P::encode_ptr(std::ptr::null_mut());
    let allocate = |index: usize| unsafe {
        let object = if mode == 1 {
            heap.alloc_nursery_obj::<H>(&info, ELEMENTS)
        } else {
            heap.alloc_obj::<H>(&info, ELEMENTS)
        };
        assert!(!object.is_null());
        for &offset in &slots {
            object.add(offset).cast::<u64>().write(null);
        }
        object.add(payload).cast::<u64>().write(index as u64);
        object
    };
    let objects: Vec<_> = (0..NODES).map(allocate).collect();
    // slot 0 is the complete cycle; every other shape includes shared aliases
    // and encoded nulls. Expected destinations depend on stable node IDs.
    let destination = |index: usize, slot: usize| -> Option<usize> {
        match slot {
            0 => Some((index + 1) % NODES),
            1 => Some(0),
            2 => Some((index + 17) % NODES),
            3 | 6 => None,
            _ => Some((index + slot * 7) % NODES),
        }
    };
    for (index, &object) in objects.iter().enumerate() {
        for (slot, &offset) in slots.iter().enumerate() {
            let target = destination(index, slot).map_or(std::ptr::null_mut(), |i| objects[i]);
            unsafe {
                object
                    .add(offset)
                    .cast::<u64>()
                    .write(P::encode_ptr(target));
            }
        }
    }
    let root = unsafe { heap.globals.add(P::encode_ptr(objects[0])) };
    let (state, _) = heap.register_thread();
    let expected_header = H::new(1);
    let expected_header =
        unsafe { std::slice::from_raw_parts((&expected_header as *const H).cast::<u8>(), H::SIZE) };
    for pass in 0..3 {
        let old = P::try_decode_ptr(heap.globals.get(root)).unwrap();
        if mode == 1 && pass == 1 {
            let young = allocate(NODES);
            for slot in [1, 2, slots.len() - 1] {
                unsafe {
                    old.add(slots[slot])
                        .cast::<u64>()
                        .write(P::encode_ptr(young));
                }
                heap.mark_card_dirty(unsafe { old.add(slots[slot]) });
            }
            unsafe {
                heap.mutator_triggered_minor_gc::<P>(&state);
            }
            assert_eq!(P::try_decode_ptr(heap.globals.get(root)), Some(old));
            let moved =
                P::try_decode_ptr(unsafe { old.add(slots[1]).cast::<u64>().read() }).unwrap();
            assert_ne!(moved, young);
            assert!(heap.is_tenured(moved));
            for slot in [1, 2, slots.len() - 1] {
                assert_eq!(
                    unsafe { old.add(slots[slot]).cast::<u64>().read() },
                    P::encode_ptr(moved)
                );
            }
        }
        unsafe {
            match (mode, pass) {
                (1, 0) => heap.mutator_triggered_minor_gc::<P>(&state),
                (2, _) => {
                    state.enter_blocked();
                    heap.concurrent_collect::<P>();
                    state.exit_blocked(&heap);
                }
                _ => heap.mutator_triggered_gc::<P>(&state),
            }
        }
        let first = P::try_decode_ptr(heap.globals.get(root)).unwrap();
        assert_ne!(first, old);
        assert_eq!(
            unsafe { crate::gc::barrier::read_barrier_atomic(old, heap.forwarding_offset()) },
            first
        );
        let mut nodes = Vec::with_capacity(NODES);
        let mut node = first;
        for index in 0..NODES {
            assert_eq!(
                unsafe { node.add(payload).cast::<u64>().read() },
                index as u64
            );
            nodes.push(node);
            node = P::try_decode_ptr(unsafe { node.add(slots[0]).cast::<u64>().read() }).unwrap();
        }
        assert_eq!(node, first);
        let young = if mode == 1 && pass >= 1 {
            Some(P::try_decode_ptr(unsafe { first.add(slots[1]).cast::<u64>().read() }).unwrap())
        } else {
            None
        };
        for (index, &object) in nodes.iter().enumerate() {
            assert_eq!(unsafe { read_type_id(object, H::TYPE_ID_OFFSET) }, 1);
            assert_eq!(
                unsafe { std::slice::from_raw_parts(object, H::SIZE) },
                expected_header
            );
            assert_eq!(unsafe { read_varlen_count(object, &info) }, ELEMENTS);
            for (slot, &offset) in slots.iter().enumerate() {
                let target =
                    if index == 0 && [1, 2, slots.len() - 1].contains(&slot) && young.is_some() {
                        young.unwrap()
                    } else {
                        destination(index, slot).map_or(std::ptr::null_mut(), |i| nodes[i])
                    };
                assert_eq!(
                    unsafe { object.add(offset).cast::<u64>().read() },
                    P::encode_ptr(target),
                    "pass={pass} node={index} slot={slot}"
                );
            }
        }
        if let Some(object) = young {
            assert_eq!(
                unsafe { std::slice::from_raw_parts(object, H::SIZE) },
                expected_header
            );
            assert_eq!(unsafe { read_varlen_count(object, &info) }, ELEMENTS);
            assert_eq!(
                unsafe { object.add(payload).cast::<u64>().read() },
                NODES as u64
            );
            for &offset in &slots {
                assert_eq!(unsafe { object.add(offset).cast::<u64>().read() }, null);
            }
        }
        assert_eq!(
            heap.from_used(),
            (NODES + usize::from(young.is_some())) * info.allocation_size(ELEMENTS)
        );
    }
    unsafe {
        heap.safe_deregister_thread(&state);
    }
}

fn layouts<P: PtrPolicy>(mode: u8) {
    graph::<OffsetHeader<4, 2, 0>, P>(mode);
    graph::<OffsetHeader<8, 10, 0>, P>(mode);
    graph::<OffsetHeader<8, 2, 8>, P>(mode);
}

#[test]
fn collector_custom_header_policy_graph_matrix() {
    const CASE: &str = "GCR_CUSTOM_HEADER_POLICY_CASE";
    if let Some(case) = std::env::var_os(CASE) {
        let case = case.into_string().unwrap();
        let (mode, policy) = case.split_once(':').unwrap();
        let mode = mode.parse().unwrap();
        match policy {
            "identity" => layouts::<IdentityPtrPolicy>(mode),
            "low" => layouts::<LowTag>(mode),
            "high" => layouts::<HighTag>(mode),
            _ => panic!("unknown policy"),
        }
        return;
    }
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    for mode in 0..3 {
        for policy in ["identity", "low", "high"] {
            for workers in [1, 2, 4, 8] {
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "gc::heap::custom_header_policy_tests::collector_custom_header_policy_graph_matrix"])
                    .env(CASE, format!("{mode}:{policy}"))
                    .env("GCR_GC_WORKERS", workers.to_string())
                    .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
                let deadline = Instant::now() + Duration::from_secs(15);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        panic!("custom header policy matrix timed out: mode={mode} policy={policy} workers={workers}: {:?}", child.wait_with_output().unwrap());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "mode={mode} policy={policy} workers={workers}: {output:?}"
                );
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("test gc::heap::custom_header_policy_tests::collector_custom_header_policy_graph_matrix ... ok"),
                    "custom header matrix child did not execute its regression: {output:?}"
                );
                println!("custom header policy: mode={mode} policy={policy} workers={workers} layouts=3 collections={}", if mode == 1 {12} else {9});
            }
        }
    }
}
