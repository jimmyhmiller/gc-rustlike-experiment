//! Live root enrollment with repeated source registration and moving payloads.
use gcrust_rt::gc::{Full, Heap, IdentityPtrPolicy, ObjHeader, RootSet, RootSource, TypeInfo};
use std::time::Instant;

struct Repeated<'a> {
    roots: &'a RootSet,
    repeats: usize,
}
// The backing RootSet is stable and quiescent throughout each collection.
unsafe impl RootSource for Repeated<'_> {
    unsafe fn scan_roots(&self, visitor: &mut dyn FnMut(*mut u64)) {
        for _ in 0..self.repeats {
            unsafe { self.roots.scan_roots(visitor) };
        }
    }
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let count: usize = args[0].parse().unwrap();
    let repeats: usize = args[1].parse().unwrap();
    assert!(count <= 16384 && repeats > 0 && repeats <= 64);
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new::<Full>(4 << 20, vec![info.clone()]);
    let mut roots = RootSet::new();
    for index in 0..count {
        let object = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        unsafe {
            object
                .add(info.raw_data_offset())
                .cast::<u64>()
                .write(index as u64)
        };
        roots.add(object as u64);
    }
    let source = Repeated {
        roots: &roots,
        repeats,
    };
    unsafe { heap.collect::<IdentityPtrPolicy>(&[&source]) };
    let before = heap.collections();
    let rounds: usize = args
        .get(2)
        .map(|value| value.parse().unwrap())
        .unwrap_or(64);
    assert!((1..=4096).contains(&rounds));
    let start = Instant::now();
    for _ in 0..rounds {
        unsafe { heap.collect::<IdentityPtrPolicy>(&[&source]) };
        assert_eq!(heap.from_used(), count * info.allocation_size(0));
        for index in 0..count {
            let object = roots.get(index) as *const u8;
            assert_eq!(
                unsafe { object.add(info.raw_data_offset()).cast::<u64>().read() },
                index as u64
            );
        }
    }
    let nanos = start.elapsed().as_nanos();
    assert_eq!(heap.collections() - before, rounds);
    println!("{{\"roots\":{count},\"repeats\":{repeats},\"rounds\":{rounds},\"nanos\":{nanos}}}");
}
