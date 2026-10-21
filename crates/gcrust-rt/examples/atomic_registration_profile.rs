//! Fresh native atomic-cell registration through the actual runtime ABI.
use gcrust_rt::{
    gc::{Full, ObjHeader, TypeInfo},
    runtime::{RuntimeContext, ai_atomic_i64_load, ai_atomic_i64_new},
};
use std::time::Instant;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let count: usize = args[0].parse().unwrap();
    let rounds: usize = args.get(1).map(|v| v.parse().unwrap()).unwrap_or(16);
    assert!((1..=32768).contains(&count) && (1..=64).contains(&rounds));
    let mut samples = Vec::with_capacity(rounds);
    for round in 0..=rounds {
        let mut context = RuntimeContext::new(
            1 << 20,
            vec![TypeInfo::for_header(Full::SIZE).with_raw_bytes(8)],
        );
        context.enter(|thread| unsafe {
            let mut cells = Vec::with_capacity(count);
            let start = Instant::now();
            for index in 0..count {
                cells.push(ai_atomic_i64_new(thread, index as i64));
            }
            if round != 0 {
                samples.push(start.elapsed().as_nanos());
            }
            for (index, &cell) in cells.iter().enumerate() {
                assert_eq!(ai_atomic_i64_load(thread, cell), index as i64);
            }
        });
    }
    println!("{{\"cells\":{count},\"rounds\":{rounds},\"samples_ns\":{samples:?}}}");
}
