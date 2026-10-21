//! Separate dead-channel collection latency from complete native cleanup.
use gcrust_rt::{
    gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
    runtime::{self, RuntimeContext},
};
use std::time::Instant;
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let capacity: usize = args[0].parse().unwrap();
    let count: usize = args[1].parse().unwrap();
    assert!((1..=131072).contains(&capacity) && (1..=32768).contains(&count));
    let mut rt = RuntimeContext::new(
        2 << 20,
        vec![
            TypeInfo::for_header(Full::SIZE).with_varlen_values(0),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_raw_bytes(8),
        ],
    );
    let heap = rt.heap().clone();
    let (start, collection_nanos, pending_bytes) = rt.enter(|thread| unsafe {
        let dyna = &*(*thread).dyna_thread;
        let mut owners = Vec::new();
        {
            let roots = dyna.scratch_scope();
            let witness = roots.push(runtime::ai_gc_alloc_varlen(thread, 0, 0, 0));
            for _ in 0..count {
                let channel = runtime::ai_chan_new(thread, capacity as i64, roots.get(witness), 1);
                let owner = runtime::ai_gc_alloc_fixed(thread, 1, 0);
                runtime::ai_chan_bind_owner(thread, channel, owner);
                owners.push(heap.globals.add(owner as u64));
            }
            assert_eq!(heap.native_buffer_bytes(), capacity * 8 * count);
        }
        for owner in owners {
            heap.globals.set(owner, 0);
        }
        // Every owner is now dead. Population/startup are outside both timers.
        let start = Instant::now();
        heap.mutator_triggered_gc::<IdentityPtrPolicy>(dyna);
        let collection_nanos = start.elapsed().as_nanos();
        (start, collection_nanos, heap.native_buffer_bytes())
    });
    unsafe {
        rt.finish_threads();
    }
    let total_nanos = start.elapsed().as_nanos();
    assert_eq!(heap.native_buffer_bytes(), 0);
    assert_eq!(heap.native_buffer_peak_bytes(), capacity * 8 * count);
    println!(
        "{{\"capacity\":{capacity},\"count\":{count},\"collection_nanos\":{collection_nanos},\"total_nanos\":{total_nanos},\"pending_bytes\":{pending_bytes}}}"
    );
}
