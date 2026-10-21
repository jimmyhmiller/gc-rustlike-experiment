//! Separate stopped-world telemetry, complete GC call, and native teardown.
//! Both comparison libraries use this exact source; construction is untimed.
use gcrust_rt::{
    gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
    runtime::{RuntimeContext, ai_atomic_i64_bind_owner, ai_atomic_i64_new, ai_gc_alloc_fixed},
};
use std::time::Instant;
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let dead: usize = args[0].parse().unwrap();
    let raw: usize = args[1].parse().unwrap();
    let rounds: usize = args[2].parse().unwrap();
    assert!((1..=32768).contains(&dead) && raw <= 4096 && (1..=32).contains(&rounds));
    let mut pauses = Vec::new();
    let mut calls = Vec::new();
    let mut totals = Vec::new();
    for round in 0..=rounds {
        let mut runtime = RuntimeContext::new(
            32 << 20,
            vec![TypeInfo::for_header(Full::SIZE).with_raw_bytes(8)],
        );
        let heap = runtime.heap().clone();
        let (start, call) = runtime.enter(|thread| unsafe {
            for _ in 0..raw {
                ai_atomic_i64_new(thread, 0);
            }
            for _ in 0..dead {
                let owner = ai_gc_alloc_fixed(thread, 0, 0);
                let cell = ai_atomic_i64_new(thread, 0);
                ai_atomic_i64_bind_owner(thread, cell, owner);
            }
            let start = Instant::now();
            heap.mutator_triggered_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
            (start, start.elapsed().as_nanos())
        });
        drop(runtime);
        let total = start.elapsed().as_nanos();
        assert_eq!(heap.collections(), 1);
        assert_eq!(heap.from_used(), 0);
        let events = heap.gc_events();
        assert_eq!(events.len(), 1);
        if round != 0 {
            pauses.push(events[0].pause_ns);
            calls.push(call);
            totals.push(total);
        }
    }
    println!(
        "{{\"dead\":{dead},\"raw\":{raw},\"pause_ns\":{pauses:?},\"call_ns\":{calls:?},\"total_ns\":{totals:?}}}"
    );
}
