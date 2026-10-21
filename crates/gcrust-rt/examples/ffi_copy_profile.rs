//! Same-binary legacy/versioned FFI copy comparison. Each process checks all
//! copied bytes against an independent pattern before reporting elapsed time.
use gcrust_rt::{
    gc::{Full, ObjHeader, TypeInfo},
    runtime::{self, RuntimeContext, Thread},
};
use std::{hint::black_box, time::Instant};
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(args.len(), 4, "mode operation bytes bits");
    let versioned = match args[0].as_str() {
        "old" => false,
        "new" => true,
        _ => panic!("mode"),
    };
    let operation = args[1].as_str();
    let bytes: usize = args[2].parse().unwrap();
    let bits: i64 = args[3].parse().unwrap();
    assert!([1, 8, 16, 32, 64].contains(&bits));
    assert!(bytes <= 1 << 20 && bytes % (bits as usize / 8).max(1) == 0);
    let budget = if operation == "string" {
        512 << 20
    } else if operation == "in" {
        64 << 20
    } else {
        32 << 20
    };
    let rounds = (budget / bytes.max(1)).clamp(32, 1_000_000);
    let mut rt = RuntimeContext::new(
        8 << 20,
        vec![TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0)],
    );
    let before = rt.heap().collections();
    rt.enter(|thread| unsafe {
        let roots = (&*(*thread).dyna_thread).scratch_scope();
        let owner = roots.push(runtime::ai_gc_alloc_varlen(thread, 0, bytes as u64, 0));
        let input: Vec<u8> = (0..bytes).map(|i| ((i * 17 + 3) % 127) as u8).collect();
        let expected: Vec<u8> = input.iter().map(|&b| if bits == 1 { b & 1 } else { b }).collect();
        runtime::ai_buf_copy_out(thread, roots.get(owner), input.as_ptr(), bytes as i64, bits);
        let mut native = input.clone();
        native.push(0xa5);
        type Scalar = unsafe extern "C" fn(*mut Thread, *const u8, *mut u8, i64, i64);
        type Output = unsafe extern "C" fn(*mut Thread, *mut u8, *const u8, i64, i64);
        let scalar: Scalar = if versioned { runtime::ai_buf_copy_in_v2 } else { runtime::ai_buf_copy_in };
        let output: Output = if versioned { runtime::ai_buf_copy_out_v2 } else { runtime::ai_buf_copy_out };
        let string = if versioned { runtime::ai_str_copy_to_buf_v2 } else { runtime::ai_str_copy_to_buf };
        let begin = Instant::now();
        for _ in 0..rounds {
            match operation {
                "in" => black_box(scalar)(black_box(thread), black_box(roots.get(owner)), black_box(native.as_mut_ptr()), bytes as i64, bits),
                "out" => black_box(output)(black_box(thread), black_box(roots.get(owner)), black_box(native.as_ptr()), bytes as i64, bits),
                "string" => { assert_eq!(bits, 8); black_box(string)(black_box(thread), black_box(roots.get(owner)), black_box(native.as_mut_ptr())); }
                _ => panic!("operation"),
            }
        }
        let nanos = begin.elapsed().as_nanos();
        if operation == "out" {
            runtime::ai_buf_copy_in(thread, roots.get(owner), native.as_mut_ptr(), bytes as i64, bits);
        }
        assert_eq!(&native[..bytes], expected);
        assert_eq!(native[bytes], if operation == "string" { 0 } else { 0xa5 });
        println!("{{\"mode\":\"{}\",\"operation\":\"{}\",\"bytes\":{},\"bits\":{},\"rounds\":{},\"nanos\":{},\"checksum\":{}}}", args[0], operation, bytes, bits, rounds, nanos, native[..bytes].iter().map(|&b| b as u64).sum::<u64>());
    });
    assert_eq!(
        rt.heap().collections(),
        before,
        "unrequested collection in throughput profile"
    );
}
