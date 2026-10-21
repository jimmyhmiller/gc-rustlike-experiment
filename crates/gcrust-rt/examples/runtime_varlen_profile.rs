//! Reproducible native variable-length allocation profile. Prints JSONL samples;
//! correctness/count checks remain enabled. No process/RSS budget is implied.
use gcrust_rt::{
    gc::{Full, ObjHeader, TypeInfo, VarLenKind},
    runtime::{RuntimeContext, ai_gc_alloc_varlen},
};
use std::{hint::black_box, time::Instant};

fn main() {
    let samples: usize = std::env::args()
        .nth(1)
        .map(|n| n.parse().unwrap())
        .unwrap_or(7);
    assert!((1..=100).contains(&samples));
    for (case, kind, length, interior) in [
        ("bytes0", VarLenKind::Bytes, 0, 0),
        ("bytes64", VarLenKind::Bytes, 64, 0),
        ("bytes1024", VarLenKind::Bytes, 1024, 0),
        ("values16", VarLenKind::Values, 16, 0),
        ("flattened64", VarLenKind::Bytes, 8, 64),
        ("large8200", VarLenKind::Bytes, 8200, 0),
    ] {
        let mut info = TypeInfo::for_header(Full::SIZE)
            .with_raw_bytes(interior * 8)
            .with_owned_interior_ptrs((0..interior).map(|i| (Full::SIZE as u16) + i * 8).collect());
        info.varlen = kind;
        let size = info.checked_allocation_size(length).unwrap();
        let count = (32 * 1024 * 1024 / size).clamp(10_000, 300_000);
        for sample in 0..samples {
            let mut runtime =
                RuntimeContext::new_generational(256 * 1024, 8 * 1024 * 1024, vec![info.clone()]);
            let before = runtime.heap().collections();
            let mut checksum = 0u64;
            let started = Instant::now();
            runtime.enter(|thread| {
                for _ in 0..count {
                    let pointer = unsafe {
                        ai_gc_alloc_varlen(
                            black_box(thread),
                            black_box(0),
                            black_box(length as u64),
                            0,
                        )
                    };
                    assert!(!pointer.is_null());
                    let actual =
                        unsafe { pointer.add(info.varlen_count_offset()).cast::<u64>().read() };
                    assert_eq!(actual, length as u64);
                    if length != 0 {
                        assert_eq!(
                            unsafe { pointer.add(info.varlen_element_offset(0)).read() },
                            0
                        );
                    }
                    checksum += black_box(actual);
                    black_box(pointer);
                }
            });
            let elapsed = started.elapsed().as_nanos();
            assert_eq!(checksum, (count * length) as u64);
            println!(
                "{{\"case\":\"{case}\",\"sample\":{sample},\"objects\":{count},\"object_bytes\":{size},\"elapsed_ns\":{elapsed},\"checksum\":{checksum},\"major\":{},\"minor\":{}}}",
                runtime.heap().collections() - before,
                runtime.heap().minor_collections()
            );
        }
    }
}
