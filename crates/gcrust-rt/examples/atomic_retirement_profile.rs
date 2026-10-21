//! Dead managed atomic owners: collection pause and collection plus teardown.
//! Native cell preparation and payload validation happen before the timer.
use gcrust_rt::{
    gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
    runtime::{
        RuntimeContext, ai_atomic_i64_bind_owner, ai_atomic_i64_load, ai_atomic_i64_new,
        ai_gc_alloc_fixed,
    },
};
use std::time::Instant;
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let count: usize = args[0].parse().unwrap();
    let rounds: usize = args.get(1).map(|v| v.parse().unwrap()).unwrap_or(8);
    assert!((1..=32768).contains(&count) && (1..=64).contains(&rounds));
    let mut collection_ns = Vec::with_capacity(rounds);
    let mut collection_and_teardown_ns = Vec::with_capacity(rounds);
    for round in 0..=rounds {
        let mut context = RuntimeContext::new(
            32 << 20,
            vec![TypeInfo::for_header(Full::SIZE).with_raw_bytes(8)],
        );
        let heap = context.heap().clone(); // Keep arena destruction outside timing.
        let mut started = None;
        let mut pause = 0;
        context.enter(|thread| unsafe {
            for index in 0..count {
                let cell = ai_atomic_i64_new(thread, index as i64);
                let owner = ai_gc_alloc_fixed(thread, 0, 0);
                ai_atomic_i64_bind_owner(thread, cell, owner);
                assert_eq!(ai_atomic_i64_load(thread, cell), index as i64);
            }
            let before = heap.collections();
            // Managed owners intentionally have no roots. No raw cell is used
            // again after this safepoint.
            let start = Instant::now();
            heap.mutator_triggered_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
            pause = start.elapsed().as_nanos();
            started = Some(start);
            assert_eq!(heap.collections() - before, 1);
            assert_eq!(heap.from_used(), 0);
        });
        drop(context); // Includes waiting for deferred native destruction.
        let total = started.unwrap().elapsed().as_nanos();
        if round != 0 {
            collection_ns.push(pause);
            collection_and_teardown_ns.push(total);
        }
    }
    println!(
        "{{\"cells\":{count},\"rounds\":{rounds},\"collection_ns\":{collection_ns:?},\"collection_and_teardown_ns\":{collection_and_teardown_ns:?}}}"
    );
}
