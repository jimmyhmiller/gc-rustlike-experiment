//! Channel construction/close profile. Includes registry insertion, excludes
//! managed wrappers and final execution teardown. Uses empty queues; backing counters are checked.
use gcrust_rt::{
    gc::{Full, ObjHeader, TypeInfo},
    runtime::{self, RuntimeContext},
};
use std::{hint::black_box, time::Instant};
fn main() {
    let cap: usize = std::env::args().nth(1).unwrap().parse().unwrap();
    assert!((1..=131072).contains(&cap));
    let rounds = ((32 << 20) / (cap * 8)).clamp(32, 20000);
    let mut rt = RuntimeContext::new(
        2 << 20,
        vec![TypeInfo::for_header(Full::SIZE).with_varlen_values(0)],
    );
    let heap = rt.heap().clone();
    rt.enter(|thread| unsafe {
        let roots = (&*(*thread).dyna_thread).scratch_scope();
        let witness = roots.push(runtime::ai_gc_alloc_varlen(thread, 0, 0, 0));
        let start = Instant::now();
        for _ in 0..rounds {
            let queue = black_box(runtime::ai_chan_new)(
                black_box(thread),
                black_box(cap as i64),
                black_box(roots.get(witness)),
                0,
            );
            black_box(runtime::ai_chan_close)(thread, queue);
        }
        let nanos = start.elapsed().as_nanos();
        assert_eq!(heap.native_buffer_bytes(), 0);
        assert_eq!(heap.native_buffer_peak_bytes(), cap * 8);
        assert_eq!(heap.native_pressure_collections(), 0);
        println!(
            "{{\"capacity\":{},\"rounds\":{},\"nanos\":{},\"peak_bytes\":{}}}",
            cap,
            rounds,
            nanos,
            heap.native_buffer_peak_bytes()
        );
    });
}
