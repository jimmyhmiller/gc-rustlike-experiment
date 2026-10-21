//! Many distinct writable root slots aliasing a small live object.
use gcrust_rt::gc::{Full, Heap, IdentityPtrPolicy, ObjHeader, TypeInfo};
use std::time::Instant;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let count: usize = args[0].parse().unwrap();
    let rounds: usize = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(16);
    assert!((1..=1048576).contains(&count) && (1..=64).contains(&rounds));
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new_generational::<Full>(1 << 20, 8 << 20, vec![info.clone()]);
    let (thread, _) = heap.register_thread();
    for _ in 0..count {
        unsafe { heap.globals.add(0) };
    }
    let before = heap.minor_collections();
    let mut samples = Vec::with_capacity(rounds);
    for round in 0..=rounds {
        let object = unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) };
        assert!(!object.is_null());
        unsafe {
            object
                .add(info.raw_data_offset())
                .cast::<u64>()
                .write(round as u64)
        };
        for index in 0..count {
            unsafe { heap.globals.set(index, object as u64) };
        }
        let old_used = heap.from_used();
        let start = Instant::now();
        unsafe { heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&thread) };
        if round != 0 {
            samples.push(start.elapsed().as_nanos());
        }
        assert_eq!(heap.nursery_used(), 0);
        assert_eq!(heap.from_used(), old_used + info.allocation_size(0));
        let relocated = heap.globals.get(0);
        assert_ne!(relocated, object as u64);
        for index in 0..count {
            assert_eq!(heap.globals.get(index), relocated);
        }
        assert_eq!(
            unsafe {
                (relocated as *const u8)
                    .add(info.raw_data_offset())
                    .cast::<u64>()
                    .read()
            },
            round as u64
        );
    }
    assert_eq!(heap.minor_collections() - before, rounds + 1);
    println!("{{\"roots\":{count},\"rounds\":{rounds},\"samples_ns\":{samples:?}}}");
}
