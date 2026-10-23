//! C interoperability tests: scalars, value structs, native buffers, callbacks,
//! and rejection of representations that cannot cross the boundary. JIT and AOT
//! cases call real libc/libm symbols; aggregate tests compare against C helpers.
//! Native transitions publish roots and park the mutator. See `docs/ffi.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

use gcrust::codegen::{build_executable, jit_run_i64};
use gcrust::lexer::lex;
use gcrust::lower::lower_program;
use gcrust::parser::parse_module;
use gcrust::resolve::resolve_module;

fn jit(src: &str) -> i64 {
    let module = parse_module(&lex(src).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    unsafe { jit_run_i64(&prog) }.expect("jit_run_i64 failed")
}

/// Lower `src` and return the lowering error message, if any (None = compiled).
fn lower_err(src: &str) -> Option<String> {
    let module = parse_module(&lex(src).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    match lower_program(&resolved.globals) {
        Ok(_) => None,
        Err(e) => Some(e.msg),
    }
}

#[test]
fn jit_calls_libm_scalar_externs() {
    // sqrt(144.0)=12, pow(2,10)=1024, abs(-30)=30  ->  1066
    let src = r#"
        extern "C" fn sqrt(x: f64) -> f64;
        extern "C" fn pow(base: f64, exp: f64) -> f64;
        extern "C" fn abs(x: i32) -> i32;
        fn main() -> i64 {
            let r = (unsafe { sqrt(144.0) });
            let p = (unsafe { pow(2.0, 10.0) });
            let a = (unsafe { abs((0 - 30) as i32) });
            (r as i64) + (p as i64) + (a as i64)
        }
    "#;
    assert_eq!(jit(src), 1066);
}

#[test]
fn extern_rejects_non_scalar_param() {
    let src = r#"
        extern "C" fn strlen(s: String) -> i64;
        fn main() -> i64 { (unsafe { strlen("hi") }) }
    "#;
    let err = lower_err(src).expect("expected a blittable-only error");
    assert!(
        err.contains("only scalar types") && err.contains("FFI boundary"),
        "unexpected error: {err}"
    );
}

#[test]
fn jit_passes_value_struct_by_pointer_out_param() {
    // gettimeofday fills our value struct in place through the pointer. tv_sec
    // ends up far past 2023, proving C wrote the caller's stack-local struct.
    let src = r#"
        #[value] struct TimeVal { tv_sec: i64, tv_usec: i64 }
        extern "C" fn gettimeofday(mut tv: TimeVal, tz: i64) -> i32;
        fn main() -> i64 {
            let mut tv = TimeVal { tv_sec: 0, tv_usec: 0 };
            let rc = (unsafe { gettimeofday(tv, 0) });
            if rc != 0 { return 0 - 1; }
            if tv.tv_sec > 1700000000 { 1 } else { 0 }
        }
    "#;
    assert_eq!(jit(src), 1);
}

#[test]
fn extern_accepts_value_struct_of_scalars() {
    // A value struct of scalar fields is blittable and may cross by pointer.
    let src = r#"
        #[value] struct Pair { a: i64, b: i64 }
        extern "C" fn takes_pair(p: Pair) -> i64;
        fn main() -> i64 { 0 }
    "#;
    assert!(lower_err(src).is_none(), "#[value] struct of scalars should be accepted");
}

#[test]
fn extern_rejects_heap_struct_param() {
    // Inspects heap layout: every struct a heap object as written.
    gcrust::compile::set_optimize(false);
    // A non-value (heap `Ref`) struct must NOT cross — its address would move
    // under GC. Only stack-resident value structs are allowed.
    let src = r#"
        struct Heapy { a: i64 }
        extern "C" fn bad(h: Heapy) -> i64;
        fn main() -> i64 { let h = Heapy { a: 1 }; (unsafe { bad(h) }) }
    "#;
    let err = lower_err(src).expect("expected a blittable-only error");
    assert!(
        err.contains("FFI boundary") && err.contains("heap"),
        "unexpected error: {err}"
    );
}

#[test]
fn jit_passes_string_bytes_via_as_c_bytes() {
    // as_c_bytes copies the String's UTF-8 (+NUL) to a native buffer and passes a
    // RawPtr. strlen("hello, ffi")=10, atoi("32")=32 -> 42.
    let src = r#"
        extern "C" fn strlen(s: RawPtr) -> i64;
        extern "C" fn atoi(s: RawPtr) -> i32;
        fn main() -> i64 {
            let n = (unsafe { strlen(as_c_bytes("hello, ffi")) });
            let v = (unsafe { atoi(as_c_bytes("32")) });
            n + (v as i64)
        }
    "#;
    assert_eq!(jit(src), 42);
}

#[test]
fn jit_array_copy_out_buffer() {
    // memset fills a mut Array<u8> in place; the copy-out write-back makes the
    // result visible. memcmp on two equal arrays returns 0. -> 7+7+0 = 14.
    let src = r#"
        extern "C" fn memset(mut buf: RawPtr, c: i32, n: i64) -> RawPtr;
        extern "C" fn memcmp(a: RawPtr, b: RawPtr, n: i64) -> i32;
        fn main() -> i64 {
            let mut buf: Array<u8> = Array::new(5);
            (unsafe { memset(as_c_bytes(buf), 7, 5) });
            let sum = (buf[0] as i64) + (buf[4] as i64);
            let mut a: Array<u8> = Array::new(2);
            a[0] = 9; a[1] = 9;
            let mut b: Array<u8> = Array::new(2);
            b[0] = 9; b[1] = 9;
            sum + ((unsafe { memcmp(as_c_bytes(a), as_c_bytes(b), 2) }) as i64)
        }
    "#;
    assert_eq!(jit(src), 14);
}

#[test]
fn jit_c_callback_into_gcrust() {
    // libc qsort calls a gc-rust comparator back through a synthesized C-ABI
    // trampoline. Sorts [30,10,50,20,40] -> [10,20,30,40,50]; 10*100+50 = 1050.
    let src = r#"
        extern "C" fn qsort(mut base: RawPtr, n: i64, size: i64, compar: extern fn(RawPtr, RawPtr) -> i32);
        fn cmp(a: RawPtr, b: RawPtr) -> i32 {
            let x = (unsafe { ptr_read_i64(a) }); let y = (unsafe { ptr_read_i64(b) });
            if x < y { (0 - 1) as i32 } else if x > y { 1i32 } else { 0i32 }
        }
        fn main() -> i64 {
            let mut arr: Array<i64> = Array::new(5);
            arr[0] = 30; arr[1] = 10; arr[2] = 50;
            arr[3] = 20; arr[4] = 40;
            (unsafe { qsort(as_c_bytes(arr), 5, 8, cmp) });
            arr[0] * 100 + arr[4]
        }
    "#;
    assert_eq!(jit(src), 1050);
}

#[test]
fn callback_signature_mismatch_rejected() {
    // cmp returns i64 but the callback type expects i32 -> compile error.
    let src = r#"
        extern "C" fn takes_cb(f: extern fn(i64) -> i32);
        fn cb(x: i64) -> i64 { x }
        fn main() -> i64 { (unsafe { takes_cb(cb) }); 0 }
    "#;
    assert!(lower_err(src).is_some(), "expected a callback signature error");
}

#[test]
fn as_c_bytes_rejects_array_of_non_scalars() {
    let src = r#"
        extern "C" fn f(p: RawPtr) -> i64;
        fn main() -> i64 {
            let mut a: Array<String> = Array::new(1);
            (unsafe { f(as_c_bytes(a)) })
        }
    "#;
    let err = lower_err(src).expect("expected a non-scalar-array error");
    assert!(err.contains("array of scalars"), "unexpected error: {err}");
}

#[test]
fn as_c_bytes_rejected_outside_extern_arg() {
    let src = r#"
        fn main() -> i64 { let _p = as_c_bytes("x"); 0 }
    "#;
    let err = lower_err(src).expect("expected a misuse error");
    assert!(
        err.contains("direct argument") && err.contains("extern"),
        "unexpected error: {err}"
    );
}

#[test]
fn raw_string_to_rawptr_param_rejected() {
    // Passing a String directly to a RawPtr param (without as_c_bytes) must fail
    // — there is no way to hand raw heap memory to C.
    let src = r#"
        extern "C" fn g(s: RawPtr) -> i64;
        fn main() -> i64 { (unsafe { g("x") }) }
    "#;
    let err = lower_err(src).expect("expected a type mismatch");
    assert!(err.contains("RawPtr") && err.contains("String"), "unexpected error: {err}");
}

#[test]
fn extern_rejects_non_scalar_return() {
    // The extern is only checked when reached, so main must call it.
    let src = r#"
        extern "C" fn make_str() -> String;
        fn main() -> i64 { let _s = (unsafe { make_str() }); 0 }
    "#;
    let err = lower_err(src).expect("expected a blittable-only error");
    assert!(
        err.contains("only scalar types") && err.contains("FFI boundary"),
        "unexpected error: {err}"
    );
}

// --- AOT: the system linker resolves the C symbols against libc/libm ---

mod support;

fn ensure_runtime_lib() -> PathBuf {
    support::runtime_staticlib()
}

fn run_exit_code(bin: &Path) -> i32 {
    let status = Command::new(bin).status().expect("failed to run AOT binary");
    status.code().expect("AOT binary terminated by signal")
}

#[test]
fn aot_calls_libm_scalar_externs() {
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let mut out = std::env::temp_dir();
    out.push(format!("gcrust_ffi_test_{}", std::process::id()));

    let src = include_str!("../examples/ffi.gcr");
    let module = parse_module(&lex(src).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    build_executable(&prog, &out, &[]).expect("build_executable failed");

    // 1066 & 0xFF = 42
    assert_eq!(run_exit_code(&out), 1066 & 0xFF);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn aot_passes_value_struct_by_pointer() {
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let mut out = std::env::temp_dir();
    out.push(format!("gcrust_ffi_struct_test_{}", std::process::id()));

    let src = include_str!("../examples/ffi_struct.gcr");
    let module = parse_module(&lex(src).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    build_executable(&prog, &out, &[]).expect("build_executable failed");

    // Returns 1 once C has written tv_sec (current Unix time > 2023).
    assert_eq!(run_exit_code(&out), 1);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn aot_passes_value_struct_by_value() {
    // Pass value structs BY VALUE per the C ABI (AAPCS64), with NO shim: a 4-byte
    // `Color` coerced into a GPR, an HFA `Vector2 {f32,f32}` into SIMD registers,
    // both directions (args + returns). Links a small C helper compiled by `cc`,
    // so the system ABI is the oracle.
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let dir = std::env::temp_dir().join(format!("gcrust_ffi_byval_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let cpath = dir.join("abi.c");
    std::fs::write(
        &cpath,
        "#include <stdint.h>\n\
         typedef struct { uint8_t r,g,b,a; } Color;\n\
         typedef struct { float x,y; } Vector2;\n\
         int color_sum(Color c){ return c.r+c.g+c.b+c.a; }\n\
         int vec_sum(Vector2 v){ return (int)(v.x+v.y); }\n\
         Vector2 mk_vec(float x,float y){ Vector2 v; v.x=x; v.y=y; return v; }\n\
         Color mk_color(int r,int g,int b){ Color c; c.r=r;c.g=g;c.b=b;c.a=255; return c; }\n",
    )
    .unwrap();
    let obj = dir.join("abi.o");
    let ok = Command::new("cc")
        .args(["-c"])
        .arg(&cpath)
        .arg("-o")
        .arg(&obj)
        .status()
        .expect("run cc")
        .success();
    assert!(ok, "compiling the C ABI helper failed");

    let src = "#[value] struct Color { r: u8, g: u8, b: u8, a: u8 }\n#[value] struct Vector2 { x: f32, y: f32 }\nextern \"C\" fn color_sum(c: Color) -> i32;\nextern \"C\" fn vec_sum(v: Vector2) -> i32;\nextern \"C\" fn mk_vec(x: f32, y: f32) -> Vector2;\nextern \"C\" fn mk_color(r: i32, g: i32, b: i32) -> Color;\nfn main() -> i64 {\nlet s1 = (unsafe { color_sum(Color { r: 10, g: 20, b: 30, a: 255 }) });\nlet s2 = (unsafe { vec_sum(Vector2 { x: 3.0, y: 4.0 }) });\nlet v = (unsafe { mk_vec(1.5, 2.5) });\nlet s3 = (v.x + v.y) as i32;\nlet c = (unsafe { mk_color(1, 2, 3) });\nlet s4 = (c.r as i32) + (c.g as i32) + (c.b as i32) + (c.a as i32);\n(s1 as i64) + (s2 as i64) + (s3 as i64) + (s4 as i64)\n}\n";
    let module = parse_module(&lex(src).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    let out = dir.join("byval");
    build_executable(&prog, &out, &[obj.to_string_lossy().into_owned()])
        .expect("build_executable failed");

    // 315 (color_sum) + 7 (vec_sum) + 4 (mk_vec) + 261 (mk_color) = 587; &0xFF = 75.
    assert_eq!(run_exit_code(&out), 587 & 0xFF);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn aot_passes_string_bytes() {
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let mut out = std::env::temp_dir();
    out.push(format!("gcrust_ffi_bytes_test_{}", std::process::id()));

    let src = include_str!("../examples/ffi_bytes.gcr");
    let module = parse_module(&lex(src).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    build_executable(&prog, &out, &[]).expect("build_executable failed");

    // strlen("hello, ffi")=10 + atoi("32")=32 -> 42
    assert_eq!(run_exit_code(&out), 42);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn aot_array_copy_out_buffer() {
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let mut out = std::env::temp_dir();
    out.push(format!("gcrust_ffi_buffer_test_{}", std::process::id()));

    let src = include_str!("../examples/ffi_buffer.gcr");
    let module = parse_module(&lex(src).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    build_executable(&prog, &out, &[]).expect("build_executable failed");

    // memset copy-out (7+7) + memcmp equal (0) -> 14
    assert_eq!(run_exit_code(&out), 14);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn aot_c_callback_into_gcrust() {
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let mut out = std::env::temp_dir();
    out.push(format!("gcrust_ffi_callback_test_{}", std::process::id()));

    let src = include_str!("../examples/ffi_callback.gcr");
    let module = parse_module(&lex(src).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    build_executable(&prog, &out, &[]).expect("build_executable failed");

    // 1050 & 0xFF = 26
    assert_eq!(run_exit_code(&out), 1050 & 0xFF);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn large_foreign_buffers_do_not_consume_worker_stack() {
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let out = std::env::temp_dir().join(format!("gcrust_ffi_large_buffers_{}", std::process::id()));
    let src = r#"
        extern "C" fn strlen(text: RawPtr) -> i64;
        extern "C" fn memset(mut buf: RawPtr, c: i32, n: i64) -> RawPtr;
        fn main() -> i64 {
            let worker = Thread::spawn(|| {
                let mut text = "a";
                for _ in 0..22 { text = text + text; }
                let mut bytes: Array<u8> = Array::new(4194304);
                let empty: Array<u64> = Array::new(0);
                for round in 0..8 {
                    if unsafe { strlen(as_c_bytes(text)) } != 4194304 { return 1; }
                    unsafe { memset(as_c_bytes(bytes), (round + 1) as i32, 4194304); }
                    if bytes[0] != ((round + 1) as u8) { return 2; }
                    if bytes[4194303] != ((round + 1) as u8) { return 3; }
                    unsafe { memset(as_c_bytes(empty), 0, 0); }
                }
                0
            });
            worker.join()
        }
    "#;
    let (module, _) = gcrust::compile::parse_with_prelude(src).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    build_executable(&prog, &out, &[]).unwrap();
    for workers in ["1", "4"] {
        let mut child = Command::new(&out)
            .env("RUST_MIN_STACK", "1048576")
            .env("GCR_NURSERY_MB", "1")
            .env("GCR_TENURED_MB", "64")
            .env("GCR_GC_WORKERS", workers)
            .env("GCR_GC_VERIFY", "1")
            .spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "large foreign buffers failed: {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("large foreign buffer regression timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    std::fs::remove_file(out).unwrap();
}

#[test]
fn native_foreign_storage_is_aligned_initialized_and_handles_empty_buffers() {
    use gcrust::runtime::{ai_ffi_buffer_alloc_v1, ai_ffi_buffer_free_v1};
    for bytes in [0, 1, 7, 8, 9, 32768, 32769, 4194304] {
        let buffer = ai_ffi_buffer_alloc_v1(bytes);
        assert!(!buffer.is_null());
        assert_eq!(buffer as usize % 8, 0);
        unsafe {
            let contents = std::slice::from_raw_parts_mut(buffer, bytes.max(1) as usize);
            assert!(contents.iter().all(|&b| b == 0));
            contents.fill(0xa5);
            ai_ffi_buffer_free_v1(buffer, bytes);
        }
    }
}

#[test]
fn nested_foreign_arguments_keep_outer_buffers_alive() {
    assert_eq!(jit(r#"
        extern "C" fn strlen(text: RawPtr) -> i64;
        extern "C" fn memcmp(a: RawPtr, b: RawPtr, n: i64) -> i32;
        fn main() -> i64 {
            let mut result = 0;
            for _ in 0..1000 {
                if unsafe {
                    memcmp(as_c_bytes("abc"), as_c_bytes("abd"),
                           strlen(as_c_bytes("abc")))
                } < 0i32 { result = result + 1; }
            }
            result
        }
    "#), 1000);
}

#[test]
fn generated_large_copies_preserve_roots_during_concurrent_collection() {
    let source = include_str!("fixtures/ffi_copy.gcr");
    let (module, _) = gcrust::compile::parse_with_prelude(source).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    assert_eq!(unsafe { gcrust::codegen::jit_run_i64_gc(&prog, true) }.unwrap(), 0);
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let binary = std::env::temp_dir().join(format!("gcr-ffi-copy-{}", std::process::id()));
    build_executable(&prog, &binary, &[]).unwrap();
    for workers in ["1", "4"] {
        for stress in ["0", "1"] {
            let output = support::run_with_timeout(Command::new(&binary)
                .env("GCR_GC_WORKERS", workers).env("GCR_GC_STRESS", stress)
                .env("GCR_NURSERY_MB", "1").env("GCR_TENURED_MB", "16")
                .env("GCR_GC_VERIFY", "1").env("GCRUST_POISON", "1"), 60);
            assert!(output.status.success(), "{output:?}");
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
        }
    }
    std::fs::remove_file(binary).unwrap();
}

#[test]
fn native_buffer_pressure_reloads_allocation_sources_and_releases_accounting() {
    let source = include_str!("fixtures/ffi_buffers.gcr");
    let (module, _) = gcrust::compile::parse_with_prelude(source).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    assert_eq!(unsafe { gcrust::codegen::jit_run_i64_gc(&prog, true) }.unwrap(), 0);
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let dir = std::env::temp_dir().join(format!("gcr-ffi-buffers-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("app");
    build_executable(&prog, &binary, &[]).unwrap();
    for workers in ["1", "4"] {
        let metrics = dir.join(format!("metrics-{workers}.json"));
        let output = support::run_with_timeout(Command::new(&binary)
            .env("GCR_GC_WORKERS", workers).env("GCR_NURSERY_MB", "1")
            .env("GCR_TENURED_MB", "16").env("GCR_GC_VERIFY", "1")
            .env("GCRUST_POISON", "1").env("GCR_METRICS_FILE", &metrics), 60);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
        let stats: serde_json::Value = serde_json::from_slice(&std::fs::read(metrics).unwrap()).unwrap();
        assert_eq!(stats["native_buffer_bytes"], 0);
        assert!(stats["native_buffer_peak_bytes"].as_u64().unwrap() >= 2 * 3670016);
        assert!(stats["native_pressure_collections"].as_u64().unwrap() >= 2);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn foreign_temporaries_release_on_control_flow_exits_and_preserve_return_roots() {
    let (module, _) = gcrust::compile::parse_with_prelude(include_str!("fixtures/ffi_buffer_cleanup.gcr")).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    assert_eq!(unsafe { gcrust::codegen::jit_run_i64_gc(&prog, true) }.unwrap(), 0);
    let lib = ensure_runtime_lib();
    unsafe { std::env::set_var("GCRUST_RUNTIME_LIB", &lib); }
    let dir = std::env::temp_dir().join(format!("gcr-ffi-buffer-cleanup-{}",std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for debug in [false,true] {
        let binary = dir.join(if debug {"debug"} else {"native"});
        // CLI exercises both optimized output and full debug roots/layouts.
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ffi_buffer_cleanup.gcr");
        let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
        command.arg("build").arg(source).arg("-o").arg(&binary).env("GCRUST_RUNTIME_LIB",&lib);
        if debug {command.arg("--debug");}
        let built=support::run_with_timeout(&mut command,60);assert!(built.status.success(),"{built:?}");
        for workers in ["1","4"] {
            let metrics=dir.join(format!("metrics-{debug}-{workers}.json"));
            let output=support::run_with_timeout(Command::new(&binary).env("GCR_GC_WORKERS",workers)
                .env("GCR_NURSERY_MB","1").env("GCR_TENURED_MB","16")
                .env("GCR_GC_STRESS","1").env("GCR_GC_VERIFY","1").env("GCRUST_POISON","1")
                .env("GCR_METRICS_FILE",&metrics),60);
            assert!(output.status.success(),"{output:?}");assert_eq!(String::from_utf8_lossy(&output.stdout).trim(),"42");
            let stats:serde_json::Value=serde_json::from_slice(&std::fs::read(metrics).unwrap()).unwrap();
            assert_eq!(stats["native_buffer_bytes"],0,"temporary native storage leaked");
            assert!(stats["native_buffer_peak_bytes"].as_u64().unwrap()>0);
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn aot_value_struct_abi_matches_c_shapes_and_register_pressure() {
    let directory = std::env::temp_dir().join(format!("gcr-abi-matrix-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    // C is the independent ABI oracle, in both argument and return directions.
    let shapes: &[(&str, &[(&str, &str, &str, &str)])] = &[
        (
            "Signed",
            &[
                ("a", "int8_t", "i8", "-7"),
                ("b", "int16_t", "i16", "-300"),
                ("c", "int32_t", "i32", "-70000"),
            ],
        ),
        (
            "Flag",
            &[
                ("flag", "_Bool", "bool", "true"),
                ("code", "uint16_t", "u16", "17"),
            ],
        ),
        (
            "Bytes",
            &[
                ("a", "uint8_t", "u8", "1"),
                ("b", "uint8_t", "u8", "2"),
                ("c", "uint8_t", "u8", "3"),
            ],
        ),
        (
            "F3",
            &[
                ("a", "float", "f32", "1.5"),
                ("b", "float", "f32", "2.5"),
                ("c", "float", "f32", "3.5"),
            ],
        ),
        (
            "F4",
            &[
                ("a", "float", "f32", "1.5"),
                ("b", "float", "f32", "2.5"),
                ("c", "float", "f32", "3.5"),
                ("d", "float", "f32", "4.5"),
            ],
        ),
        (
            "D2",
            &[("a", "double", "f64", "1.5"), ("b", "double", "f64", "2.5")],
        ),
        (
            "IF",
            &[("i", "int32_t", "i32", "17"), ("f", "float", "f32", "2.5")],
        ),
        (
            "FI",
            &[("f", "float", "f32", "2.5"), ("i", "int32_t", "i32", "17")],
        ),
        (
            "DF",
            &[("d", "double", "f64", "1.5"), ("f", "float", "f32", "2.5")],
        ),
        (
            "FD",
            &[("f", "float", "f32", "2.5"), ("d", "double", "f64", "1.5")],
        ),
        (
            "Mixed",
            &[("i", "int64_t", "i64", "17"), ("d", "double", "f64", "2.5")],
        ),
        (
            "Reverse",
            &[("d", "double", "f64", "2.5"), ("i", "int64_t", "i64", "17")],
        ),
        (
            "Pair",
            &[("a", "int64_t", "i64", "17"), ("b", "int64_t", "i64", "23")],
        ),
    ];
    let mut c = String::from("#include <stdint.h>\n#include <stdbool.h>\n");
    let mut source = String::new();
    let mut checks = String::new();
    for (index, (name, fields)) in shapes.iter().enumerate() {
        let c_fields = fields
            .iter()
            .map(|(n, t, _, _)| format!("{t} {n};"))
            .collect::<String>();
        let g_fields = fields
            .iter()
            .map(|(n, _, t, _)| format!("{n}: {t}"))
            .collect::<Vec<_>>()
            .join(",");
        let literals = fields
            .iter()
            .map(|(_, _, _, v)| *v)
            .collect::<Vec<_>>()
            .join(",");
        let g_literal = fields
            .iter()
            .map(|(n, _, t, v)| {
                let literal = if *t == "f32" {
                    format!("{v}f32")
                } else {
                    v.to_string()
                };
                format!("{n}: {literal}")
            })
            .collect::<Vec<_>>()
            .join(",");
        let c_check = fields
            .iter()
            .map(|(n, _, _, v)| format!("v.{n}=={v}"))
            .collect::<Vec<_>>()
            .join("&&");
        let g_check = fields
            .iter()
            .map(|(n, _, t, v)| {
                let literal = if *t == "f32" {
                    format!("{v}f32")
                } else {
                    v.to_string()
                };
                format!("v.{n} != {literal}")
            })
            .collect::<Vec<_>>()
            .join(" || ");
        c.push_str(&format!("typedef struct {{ {c_fields} }} {name};\nint check_{name}({name} v){{return {c_check};}}\n{name} make_{name}(void){{{name} v={{{literals}}};return v;}}\n"));
        source.push_str(&format!("#[value] struct {name} {{ {g_fields} }}\nextern \"C\" fn check_{name}(v:{name})->i32;\nextern \"C\" fn make_{name}()->{name};\n"));
        checks.push_str(&format!("if (unsafe {{ check_{name}({name}{{{g_literal}}}) }}) != 1 {{ return {}; }} let v=unsafe {{make_{name}()}}; if {g_check} {{return {};}}\n",10+index,100+index));
    }
    c.push_str(r#"
        typedef struct { int64_t a,b,c; } Big;
        typedef struct { F3 floats; int32_t code; } Nested;
        int nested(Nested v){ return v.floats.a==1.5f && v.floats.b==2.5f && v.floats.c==3.5f && v.code==17; }
        Nested make_nested(void){Nested v={{1.5f,2.5f,3.5f},17};return v;}
        int gp6(int64_t a,int64_t b,int64_t c,int64_t d,int64_t e,int64_t f,Mixed v,double tail){return a==1&&b==2&&c==3&&d==4&&e==5&&f==6&&v.i==17&&v.d==2.5&&tail==9.5;}
        int gp5(int64_t a,int64_t b,int64_t c,int64_t d,int64_t e,Pair v,int64_t tail){return a==1&&b==2&&c==3&&d==4&&e==5&&v.a==17&&v.b==23&&tail==9;}
        int sse8(double a,double b,double c,double d,double e,double f,double g,double h,Mixed v,int64_t tail){return a==1&&b==2&&c==3&&d==4&&e==5&&f==6&&g==7&&h==8&&v.i==17&&v.d==2.5&&tail==9;}
        int sse7(double a,double b,double c,double d,double e,double f,double g,D2 v,double tail){return a==1&&b==2&&c==3&&d==4&&e==5&&f==6&&g==7&&v.a==1.5&&v.b==2.5&&tail==9.5;}
        int64_t bump(Big *v){v->a=9;return 9;}
        int snapshot(Big v,int64_t tail){return v.a==1&&v.b==2&&v.c==3&&tail==9;}
        int private_copy(Big v){((volatile Big*)&v)->a=99;return v.a==99&&v.b==2&&v.c==3;}
    "#);
    source.push_str(
        r#"
        #[value] struct Big {a:i64,b:i64,c:i64}
        #[value] struct Nested {floats:F3,code:i32}
        extern "C" fn nested(v:Nested)->i32;
        extern "C" fn make_nested()->Nested;
        extern "C" fn gp6(a:i64,b:i64,c:i64,d:i64,e:i64,f:i64,v:Mixed,tail:f64)->i32;
        extern "C" fn gp5(a:i64,b:i64,c:i64,d:i64,e:i64,v:Pair,tail:i64)->i32;
        extern "C" fn sse8(a:f64,b:f64,c:f64,d:f64,e:f64,f:f64,g:f64,h:f64,v:Mixed,tail:i64)->i32;
        extern "C" fn sse7(a:f64,b:f64,c:f64,d:f64,e:f64,f:f64,g:f64,v:D2,tail:f64)->i32;
        extern "C" fn bump(mut v:Big)->i64;
        extern "C" fn snapshot(v:Big,tail:i64)->i32;
        extern "C" fn private_copy(v:Big)->i32;
    "#,
    );
    checks.push_str(r#"
        if (unsafe {nested(Nested{floats:F3{a:1.5f32,b:2.5f32,c:3.5f32},code:17})})!=1 {return 201;}
        let n=unsafe {make_nested()}; if n.floats.a!=1.5f32 || n.floats.b!=2.5f32 || n.floats.c!=3.5f32 || n.code!=17 {return 202;}
        if (unsafe {gp6(1,2,3,4,5,6,Mixed{i:17,d:2.5},9.5)})!=1 {return 203;}
        if (unsafe {gp5(1,2,3,4,5,Pair{a:17,b:23},9)})!=1 {return 204;}
        if (unsafe {sse8(1.0,2.0,3.0,4.0,5.0,6.0,7.0,8.0,Mixed{i:17,d:2.5},9)})!=1 {return 205;}
        if (unsafe {sse7(1.0,2.0,3.0,4.0,5.0,6.0,7.0,D2{a:1.5,b:2.5},9.5)})!=1 {return 206;}
        let mut big=Big{a:1,b:2,c:3};
        if (unsafe {snapshot(big,unsafe {bump(big)})})!=1 {return 207;}
        if big.a!=9 || (unsafe {private_copy(big)})!=1 || big.a!=9 {return 208;}
    "#);
    source.push_str(&format!("fn main()->i64 {{ {checks} 0 }}"));
    let c_path = directory.join("oracle.c");
    let object = directory.join("oracle.o");
    std::fs::write(&c_path, c).unwrap();
    assert!(
        Command::new("cc")
            .arg("-O2")
            .arg("-c")
            .arg(&c_path)
            .arg("-o")
            .arg(&object)
            .status()
            .unwrap()
            .success()
    );
    let module = parse_module(&lex(&source).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    let program = lower_program(&resolved.globals).unwrap();
    let binary = directory.join("matrix");
    build_executable(&program, &binary, &[object.to_string_lossy().into_owned()]).unwrap();
    assert_eq!(
        run_exit_code(&binary),
        0,
        "C ABI matrix failed; retained fixture at {}",
        directory.display()
    );
    std::fs::remove_dir_all(directory).unwrap();
}
