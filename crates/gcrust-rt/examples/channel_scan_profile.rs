//! Quiescent collection and send/receive costs for a fixed occupied queue.
use gcrust_rt::{
    gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
    runtime::{self, RuntimeContext},
};
use std::{hint::black_box, time::Instant};
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let capacity: usize = args[0].parse().unwrap();
    let occupied: usize = args[1].parse().unwrap();
    assert!(capacity > occupied && capacity <= 131072);
    let types = vec![
        TypeInfo::for_header(Full::SIZE).with_varlen_values(0),
        TypeInfo::for_header(Full::SIZE)
            .with_type_id(1)
            .with_raw_bytes(8),
    ];
    let mut rt = RuntimeContext::new(16 << 20, types);
    let heap = rt.heap().clone();
    rt.enter(|thread| unsafe {
        let dyna = &*(*thread).dyna_thread;
        let roots = dyna.scratch_scope();
        let witness = roots.push(runtime::ai_gc_alloc_varlen(thread, 0, 0, 0));
        let queue = runtime::ai_chan_new(thread, capacity as i64, roots.get(witness), 1);
        for i in 0..occupied {
            let value = runtime::ai_gc_alloc_fixed(thread, 1, 0);
            value.add(Full::SIZE).cast::<u64>().write(i as u64);
            assert_eq!(runtime::ai_chan_send(thread, roots.get(witness), queue, value), 0);
        }
        // Repeated full collections keep exactly the same live queue graph.
        heap.mutator_triggered_gc::<IdentityPtrPolicy>(dyna);
        let rounds = 128;
        let start = Instant::now();
        for _ in 0..rounds { heap.mutator_triggered_gc::<IdentityPtrPolicy>(dyna); }
        let collection_nanos = start.elapsed().as_nanos();
        for i in 0..occupied {
            let value = runtime::ai_chan_recv(thread, roots.get(witness), queue);
            assert!(!value.is_null());
            assert_eq!(value.add(Full::SIZE).cast::<u64>().read(), i as u64);
        }
        let value = roots.push(runtime::ai_gc_alloc_fixed(thread, 1, 0));
        roots.get(value).add(Full::SIZE).cast::<u64>().write(42);
        let operations = 20000;
        let start = Instant::now();
        for _ in 0..operations {
            assert_eq!(black_box(runtime::ai_chan_send)(thread, roots.get(witness), queue, roots.get(value)), 0);
            assert_eq!(black_box(runtime::ai_chan_recv)(thread, roots.get(witness), queue), roots.get(value));
        }
        let operation_nanos = start.elapsed().as_nanos();
        runtime::ai_chan_close(thread, queue);
        assert_eq!(heap.native_buffer_bytes(), 0);
        println!("{{\"capacity\":{capacity},\"occupied\":{occupied},\"collections\":{rounds},\"collection_nanos\":{collection_nanos},\"operations\":{operations},\"operation_nanos\":{operation_nanos}}}");
    });
}
