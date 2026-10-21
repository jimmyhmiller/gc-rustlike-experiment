//! Native allocation/release profile with full zero-initialization and ledger checks.
use gcrust_rt::{
    gc::{Full, ObjHeader, TypeInfo},
    runtime::{self, RuntimeContext},
};
use std::{hint::black_box, time::Instant};
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(args.len(), 2);
    let old = match args[0].as_str() {
        "old" => true,
        "new" => false,
        _ => panic!("mode"),
    };
    let bytes: i64 = args[1].parse().unwrap();
    assert!((0..=1048576).contains(&bytes));
    let rounds = ((32 << 20) / (bytes.max(1) as usize)).clamp(64, 300000);
    let mut rt = RuntimeContext::new(
        2 << 20,
        vec![TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0)],
    );
    let heap = rt.heap().clone();
    rt.enter(|thread| unsafe {
        // Validate complete initialized payload outside the timed loop.
        let token = if old {
            runtime::ai_ffi_buffer_alloc_v1(bytes)
        } else {
            runtime::ai_ffi_buffer_alloc_v2(thread, bytes)
        };
        let p = if old {
            token
        } else {
            runtime::ai_ffi_buffer_data_v2(token)
        };
        assert!(
            std::slice::from_raw_parts(p, bytes.max(1) as usize)
                .iter()
                .all(|&b| b == 0)
        );
        if old {
            runtime::ai_ffi_buffer_free_v1(p, bytes)
        } else {
            runtime::ai_ffi_buffer_free_v2(thread, token)
        };
        let begin = Instant::now();
        for _ in 0..rounds {
            let token = if old {
                black_box(runtime::ai_ffi_buffer_alloc_v1)(black_box(bytes))
            } else {
                black_box(runtime::ai_ffi_buffer_alloc_v2)(black_box(thread), black_box(bytes))
            };
            let p = if old {
                token
            } else {
                runtime::ai_ffi_buffer_data_v2(token)
            };
            assert_eq!(*p, 0);
            assert_eq!(*p.add(bytes.max(1) as usize - 1), 0);
            *p = 0xa5;
            *p.add(bytes.max(1) as usize - 1) = 0xa5;
            if old {
                black_box(runtime::ai_ffi_buffer_free_v1)(p, bytes)
            } else {
                black_box(runtime::ai_ffi_buffer_free_v2)(thread, token)
            };
        }
        let nanos = begin.elapsed().as_nanos();
        assert_eq!(heap.native_buffer_bytes(), 0);
        assert_eq!(heap.native_pressure_collections(), 0);
        println!(
            "{{\"mode\":\"{}\",\"bytes\":{},\"rounds\":{},\"nanos\":{}}}",
            args[0], bytes, rounds, nanos
        );
    });
}
