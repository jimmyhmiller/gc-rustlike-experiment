//! Correctness-checked concurrent-cycle timings without competing mutators.
use gcrust_rt::gc::{Full, Heap, IdentityPtrPolicy, ObjHeader, TypeInfo};
use std::time::Instant;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let count: usize = args.first().expect("reference count").parse().unwrap();
    let rounds: usize = args.get(1).expect("measured rounds").parse().unwrap();
    let shape = args.get(2).expect("shared or distinct");
    assert!((1..=262144).contains(&count) && (1..=128).contains(&rounds));
    assert!(matches!(shape.as_str(), "shared" | "distinct"));
    let array = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
    let leaf = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    let heap = Heap::new::<Full>(32 << 20, vec![array.clone(), leaf.clone()]);
    let owner = unsafe { heap.alloc_obj::<Full>(&array, count) };
    assert!(!owner.is_null());
    let mut shared = std::ptr::null_mut();
    for index in 0..count {
        let object = if shape == "shared" && index != 0 {
            shared
        } else {
            let object = unsafe { heap.alloc_obj::<Full>(&leaf, 0) };
            assert!(!object.is_null());
            unsafe {
                object.add(Full::SIZE).cast::<u64>().write(index as u64);
            }
            shared = object;
            object
        };
        unsafe {
            owner
                .add(array.varlen_count_offset() + 8 + index * 8)
                .cast::<u64>()
                .write(object as u64);
        }
    }
    let root = unsafe { heap.globals.add(owner as u64) };
    let live = array.allocation_size(count)
        + leaf.allocation_size(0) * if shape == "shared" { 1 } else { count };
    let mut samples = Vec::with_capacity(rounds);
    for round in 0..rounds + 2 {
        let start = Instant::now();
        unsafe {
            heap.concurrent_collect::<IdentityPtrPolicy>();
        }
        let elapsed = start.elapsed().as_nanos();
        if round >= 2 {
            samples.push(elapsed);
        }
        assert_eq!(heap.from_used(), live);
        let owner = heap.globals.get(root) as *const u8;
        let mut first = std::ptr::null();
        for index in 0..count {
            let object = unsafe {
                owner
                    .add(array.varlen_count_offset() + 8 + index * 8)
                    .cast::<*const u8>()
                    .read()
            };
            assert!(heap.from_space_contains(object));
            let expected = if shape == "shared" { 0 } else { index as u64 };
            assert_eq!(
                unsafe { object.add(Full::SIZE).cast::<u64>().read() },
                expected
            );
            if index == 0 {
                first = object;
            }
            if shape == "shared" {
                assert_eq!(object, first);
            }
        }
    }
    assert_eq!(heap.collections(), rounds + 2);
    println!(
        "{{\"references\":{count},\"shape\":\"{shape}\",\"rounds\":{rounds},\"live_bytes\":{live},\"samples_ns\":{samples:?}}}"
    );
}
