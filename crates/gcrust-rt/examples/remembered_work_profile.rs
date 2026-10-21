//! Minor collections with little young data and a wide dirty old graph.
use gcrust_rt::gc::{Full, Heap, IdentityPtrPolicy, ObjHeader, TypeInfo};
use std::time::Instant;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let count: usize = args[0].parse().unwrap();
    let fields: u16 = args[1].parse().unwrap();
    let rounds: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(16);
    assert!(
        (1..=65536).contains(&count) && (1..=128).contains(&fields) && (1..=64).contains(&rounds)
    );
    let node = TypeInfo::for_header(Full::SIZE)
        .with_fields(fields)
        .with_raw_bytes(8);
    let leaf = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    let heap = Heap::new_generational::<Full>(1 << 20, 128 << 20, vec![node.clone(), leaf.clone()]);
    let (thread, _) = heap.register_thread();
    let mut nodes = Vec::with_capacity(count);
    for index in 0..count {
        let object = unsafe { heap.alloc_obj::<Full>(&node, 0) };
        assert!(!object.is_null());
        for field in 0..fields {
            unsafe {
                object
                    .add(node.value_field_offset(field))
                    .cast::<u64>()
                    .write(0)
            };
        }
        unsafe {
            object
                .add(node.raw_data_offset())
                .cast::<u64>()
                .write(index as u64)
        };
        unsafe { heap.globals.add(object as u64) };
        nodes.push(object);
    }
    let before = heap.minor_collections();
    let mut samples = Vec::new();
    for round in 0..=rounds {
        let child = unsafe { heap.alloc_nursery_obj::<Full>(&leaf, 0) };
        assert!(!child.is_null());
        unsafe {
            child
                .add(leaf.raw_data_offset())
                .cast::<u64>()
                .write(round as u64)
        };
        for &object in &nodes {
            let slot = unsafe { object.add(node.value_field_offset(0)) };
            unsafe { slot.cast::<u64>().write(child as u64) };
            heap.mark_card_dirty(slot);
        }
        let start = Instant::now();
        unsafe { heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&thread) };
        if round != 0 {
            samples.push(start.elapsed().as_nanos());
        }
        assert_eq!(heap.nursery_used(), 0);
        for (index, &object) in nodes.iter().enumerate() {
            let child =
                unsafe { object.add(node.value_field_offset(0)).cast::<u64>().read() } as *const u8;
            assert!(!child.is_null());
            assert!(!heap.is_nursery(child));
            assert_eq!(
                unsafe { child.add(leaf.raw_data_offset()).cast::<u64>().read() },
                round as u64
            );
            for field in 1..fields {
                assert_eq!(
                    unsafe {
                        object
                            .add(node.value_field_offset(field))
                            .cast::<u64>()
                            .read()
                    },
                    0
                );
            }
            assert_eq!(
                unsafe { object.add(node.raw_data_offset()).cast::<u64>().read() },
                index as u64
            );
        }
    }
    assert_eq!(heap.minor_collections() - before, rounds + 1);
    println!(
        "{{\"objects\":{count},\"fields\":{fields},\"rounds\":{rounds},\"samples_ns\":{samples:?}}}"
    );
}
