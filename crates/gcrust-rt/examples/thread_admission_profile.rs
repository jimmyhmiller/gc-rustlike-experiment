//! Actual runtime spawn/bind admission; native joins validate results afterward.
use gcrust_rt::{
    gc::{Full, ObjHeader, TypeInfo},
    runtime::{
        RuntimeContext, Thread, ai_gc_alloc_fixed, ai_thread_bind_owner, ai_thread_join,
        ai_thread_spawn,
    },
};
use std::time::Instant;
unsafe extern "C" fn child(_thread: *mut Thread, env: *mut u8) -> u64 {
    unsafe { env.add(Full::SIZE).cast::<u64>().read() }
}
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let count: usize = args[0].parse().unwrap();
    let rounds: usize = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(8);
    assert!((1..=256).contains(&count) && (1..=16).contains(&rounds));
    let mut samples = Vec::with_capacity(rounds);
    for round in 0..=rounds {
        let mut context = RuntimeContext::new(
            1 << 20,
            vec![TypeInfo::for_header(Full::SIZE).with_raw_bytes(8)],
        );
        context.enter(|thread| unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let env = roots.push(ai_gc_alloc_fixed(thread, 0, 0));
            roots.get(env).add(Full::SIZE).cast::<u64>().write(42);
            let mut handles = Vec::with_capacity(count);
            let start = Instant::now();
            for _ in 0..count {
                let handle = ai_thread_spawn(thread, roots.get(env), child as *const u8);
                ai_thread_bind_owner(thread, handle, roots.get(env));
                handles.push(handle);
            }
            if round != 0 {
                samples.push(start.elapsed().as_nanos());
            }
            for &handle in &handles {
                assert_eq!(ai_thread_join(thread, handle), 42);
                assert_eq!(ai_thread_join(thread, handle), 42);
            }
            assert_eq!(roots.get(env).add(Full::SIZE).cast::<u64>().read(), 42);
        });
    }
    println!("{{\"threads\":{count},\"rounds\":{rounds},\"samples_ns\":{samples:?}}}");
}
