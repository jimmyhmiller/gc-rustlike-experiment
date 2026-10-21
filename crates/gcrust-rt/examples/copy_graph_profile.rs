//! Cyclic or tree-shaped graph: one root, checked after every moving collection.
use gcrust_rt::gc::{Full, Heap, IdentityPtrPolicy, ObjHeader, RootSet, TypeInfo};
use std::time::Instant;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let count: usize = args[0].parse().unwrap();
    let rounds: usize = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(16);
    let shape = args.get(2).map(String::as_str).unwrap_or("cycle");
    assert!(matches!(shape, "cycle" | "tree"));
    assert!((1..=262144).contains(&count) && (1..=64).contains(&rounds));
    let info = TypeInfo::for_header(Full::SIZE)
        .with_fields(2)
        .with_raw_bytes(8);
    let heap = Heap::new::<Full>(32 << 20, vec![info.clone()]);
    let nodes: Vec<_> = (0..count)
        .map(|_| unsafe { heap.alloc_obj::<Full>(&info, 0) })
        .collect();
    for (index, &node) in nodes.iter().enumerate() {
        assert!(!node.is_null());
        let children = if shape == "tree" {
            [2 * index + 1, 2 * index + 2]
        } else {
            [(index + 1) % count, (index + 17) % count]
        };
        unsafe {
            for (field, child) in children.into_iter().enumerate() {
                node.add(info.value_field_offset(field as u16))
                    .cast::<u64>()
                    .write(if child < count {
                        nodes[child] as u64
                    } else {
                        0
                    });
            }
            node.add(info.raw_data_offset())
                .cast::<u64>()
                .write(index as u64);
        }
    }
    let mut roots = RootSet::new();
    roots.add(nodes[0] as u64);
    drop(nodes);
    let before = heap.collections();
    let mut samples = Vec::with_capacity(rounds);
    for round in 0..=rounds {
        let start = Instant::now();
        unsafe { heap.collect::<IdentityPtrPolicy>(&[&roots]) };
        if round != 0 {
            samples.push(start.elapsed().as_nanos());
        }
        assert_eq!(heap.from_used(), count * info.allocation_size(0));
        let first = roots.get(0) as *const u8;
        if shape == "tree" {
            let mut pending = vec![(first, 0)];
            let mut visited = 0;
            while let Some((node, index)) = pending.pop() {
                assert!(visited < count && index < count);
                visited += 1;
                unsafe {
                    assert_eq!(
                        node.add(info.raw_data_offset()).cast::<u64>().read(),
                        index as u64
                    );
                    for field in 0..2 {
                        let child = node
                            .add(info.value_field_offset(field))
                            .cast::<u64>()
                            .read();
                        let child_index = index * 2 + field as usize + 1;
                        if child_index < count {
                            assert_ne!(child, 0);
                            pending.push((child as *const u8, child_index));
                        } else {
                            assert_eq!(child, 0);
                        }
                    }
                }
            }
            assert_eq!(visited, count);
            continue;
        }
        let mut node = first;
        for index in 0..count {
            unsafe {
                assert_eq!(
                    node.add(info.raw_data_offset()).cast::<u64>().read(),
                    index as u64
                );
                let skip = node.add(info.value_field_offset(1)).cast::<u64>().read() as *const u8;
                assert_eq!(
                    skip.add(info.raw_data_offset()).cast::<u64>().read(),
                    ((index + 17) % count) as u64
                );
                node = node.add(info.value_field_offset(0)).cast::<u64>().read() as *const u8;
            }
        }
        assert_eq!(node, first);
    }
    assert_eq!(heap.collections() - before, rounds + 1);
    println!(
        "{{\"nodes\":{count},\"shape\":\"{shape}\",\"rounds\":{rounds},\"samples_ns\":{samples:?}}}"
    );
}
