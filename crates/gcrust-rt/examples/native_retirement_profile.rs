//! Report first-use and steady construction/empty-close costs separately.
use gcrust_rt::{
    gc::{Full, ObjHeader, TypeInfo},
    runtime::{self, RuntimeContext},
};
use std::{hint::black_box, time::Instant};
fn main() {
    let capacity: usize = std::env::args().nth(1).unwrap().parse().unwrap();
    assert!((1..=131072).contains(&capacity));
    let mut rt = RuntimeContext::new(
        2 << 20,
        vec![TypeInfo::for_header(Full::SIZE).with_varlen_values(0)],
    );
    let heap = rt.heap().clone();
    rt.enter(|thread| unsafe {
        let roots=(&*(*thread).dyna_thread).scratch_scope();
        let witness=roots.push(runtime::ai_gc_alloc_varlen(thread,0,0,0));
        let start=Instant::now();
        let queue=black_box(runtime::ai_chan_new)(thread,capacity as i64,roots.get(witness),0);
        black_box(runtime::ai_chan_close)(thread,queue);
        let first_nanos=start.elapsed().as_nanos();
        let rounds=((256 << 20) / (capacity * 8)).clamp(256, 20000);
        let start=Instant::now();
        for _ in 0..rounds {
            let queue=black_box(runtime::ai_chan_new)(thread,capacity as i64,roots.get(witness),0);
            black_box(runtime::ai_chan_close)(thread,queue);
        }
        let nanos=start.elapsed().as_nanos();
        assert_eq!(heap.native_buffer_bytes(),0);
        assert_eq!(heap.native_buffer_peak_bytes(),capacity*8);
        assert_eq!(heap.native_pressure_collections(),0);
        println!("{{\"capacity\":{capacity},\"rounds\":{rounds},\"first_nanos\":{first_nanos},\"nanos\":{nanos}}}");
    });
}
