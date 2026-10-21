//! Capability enforcement must happen before LLVM emission, without depending
//! on names, aliases, generic specialization or deferred execution to be safe.
use gcrust::codegen::jit_run_i64_gc;
use gcrust::{compile::parse_with_prelude, lower::lower_program, resolve::resolve_module};
use std::process::Command;
mod support;
fn error(source: &str) -> String {
    let (module, _) = parse_with_prelude(source).unwrap();
    let resolved = resolve_module(module).unwrap();
    lower_program(&resolved.globals)
        .err()
        .expect("unsafe operation accepted")
        .msg
}
fn program(source: &str) -> gcrust::core::CoreProgram {
    let (module, _) = parse_with_prelude(source).unwrap();
    let resolved = resolve_module(module).unwrap();
    lower_program(&resolved.globals).unwrap()
}
#[test]
fn privileged_intrinsics_require_a_capability() {
    for name in [
        "ptr_read_i64",
        "thread_spawn",
        "thread_join",
        "thread_bind_owner",
        "atomic_i64_new",
        "atomic_i64_bind_owner",
        "atomic_i64_load",
        "atomic_i64_store",
        "atomic_i64_fetch_add",
        "atomic_i64_cas",
        "atom_load",
        "atom_cas",
        "chan_new",
        "chan_bind_owner",
        "chan_send",
        "chan_recv",
        "chan_close",
        "chan_sender_clone",
        "chan_sender_drop",
        "chan_waiting_senders",
        "chan_waiting_receivers",
    ] {
        let source = format!("fn main() -> i64 {{ {name}(); 0 }}");
        assert!(
            error(&source).contains("requires an explicit `unsafe"),
            "{name}"
        );
    }
}
#[test]
fn foreign_aliases_and_generic_calls_require_capability() {
    for source in [
        "extern \"C\" fn abs(x: i32) -> i32; fn main() -> i64 { abs(-7 as i32) as i64 }",
        "mod ffi { pub extern \"C\" fn abs(x: i32) -> i32; } use ffi::abs; fn main() -> i64 { abs(-7 as i32) as i64 }",
        "extern \"C\" fn abs(x: i32) -> i32; fn apply<T>(x: T) -> i64 { abs(-7 as i32) as i64 } fn main() -> i64 { apply(1) }",
    ] {
        assert!(error(source).contains("requires an explicit `unsafe"));
    }
}
#[test]
fn capability_does_not_leak_into_closures_or_other_functions() {
    for source in [
        "extern \"C\" fn abs(x: i32) -> i32; fn main() -> i64 { let f = unsafe { || abs(-7 as i32) as i64 }; f() }",
        "extern \"C\" fn abs(x: i32) -> i32; fn bad() -> i64 { abs(-7 as i32) as i64 } fn main() -> i64 { unsafe { bad() } }",
        "extern \"C\" fn abs(x: i32) -> i32; fn main() -> i64 { unsafe { abs(-7 as i32); } abs(-8 as i32) as i64 }",
        "extern \"C\" fn abs(x: i32) -> i32; fn main() -> i64 { let f: fn(i32) -> i32 = unsafe { abs }; f(-7 as i32) as i64 }",
    ] {
        assert!(error(source).contains("requires an explicit `unsafe"));
    }
}
#[test]
fn transparent_native_records_cannot_bypass_the_boundary() {
    for source in [
        "fn main() -> i64 { let a = AtomicI64::new(0); let p = a.cell; 0 }",
        "fn main() -> i64 { let a = AtomicI64::new(0); let AtomicI64 { cell } = a; 0 }",
        "fn forge(p: RawPtr) -> Thread<i64> { Thread { handle: p, result: ThreadResult { v: Option::Some(1) } } } fn main() -> i64 { let a = AtomicI64::new(0); let p = unsafe { a.cell }; forge(p).join() }",
        "fn main() -> i64 { let mut t = Thread::spawn(|| 0); let a = AtomicI64::new(0); let p = unsafe { a.cell }; t.handle = p; 0 }",
        "struct Box<T> { value: T } fn wrap(p: RawPtr) -> Box<RawPtr> { Box { value: p } } fn main() -> i64 { let a = AtomicI64::new(0); let p = unsafe { a.cell }; let b = wrap(p); 0 }",
    ] {
        assert!(
            error(source).contains("requires an explicit `unsafe"),
            "{source}"
        );
    }
}
#[test]
fn ordinary_functions_with_native_names_are_safe() {
    let source = "fn thread_join(x: i64) -> i64 { x + 1 } struct A {} impl A { fn ptr_read_i64(x: i64) -> i64 { x + 1 } } fn main() -> i64 { thread_join(20) + A::ptr_read_i64(20) }";
    assert_eq!(unsafe { jit_run_i64_gc(&program(source), true) }.unwrap(), 42);
}
const SOURCE: &str = r#"
extern "C" fn abs(x: i32) -> i32;
fn safe_abs(x: i32) -> i64 { unsafe { abs(x) as i64 } }
fn main() -> i64 {
    let f = unsafe { || unsafe { abs(-7 as i32) as i64 } };
    let atom = AtomicI64::new(0);
    let channel: Channel<i64> = Channel::new(1);
    let child = Thread::spawn(|| { atom.store(safe_abs(-7 as i32)); channel.send(28).expect("send"); 0 });
    let message = channel.recv_value();
    child.join();
    channel.close();
    let mut total = 0;
    unsafe { unsafe { total = f() + atom.load() + message; } }
    total
}
"#;
#[test]
fn explicit_capabilities_and_safe_wrappers_work_in_jit_and_native() {
    assert_eq!(unsafe { jit_run_i64_gc(&program(SOURCE), true) }.unwrap(), 42);
    let root = std::env::temp_dir().join(format!("gcr-unsafe-boundary-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("main.gcr");
    let bin = root.join("program");
    std::fs::write(&path, SOURCE).unwrap();
    let output = support::run_with_timeout(
        Command::new(env!("CARGO_BIN_EXE_gcr"))
            .arg("build")
            .arg(&path)
            .arg("-o")
            .arg(&bin)
            .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()),
        60,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for stress in [false, true] {
        let output = support::run_with_timeout(
            Command::new(&bin)
                .env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                .env("GCR_GC_VERIFY", "1"),
            30,
        );
        assert_eq!(output.status.code(), Some(42), "{output:?}");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn foreign_callback_targets_keep_their_c_abi() {
    let source = r#"
        extern "C" fn strcmp(a: RawPtr, b: RawPtr) -> i32;
        extern "C" fn qsort(mut base: RawPtr, count: i64, size: i64, compare: extern fn(RawPtr, RawPtr) -> i32);
        fn main() -> i64 {
            let mut bytes: Array<u8> = Array::new(4);
            bytes[0] = 98 as u8; bytes[1] = 0 as u8;
            bytes[2] = 97 as u8; bytes[3] = 0 as u8;
            unsafe { qsort(as_c_bytes(bytes), 2, 2, strcmp); }
            if bytes[0] == 97 as u8 && bytes[2] == 98 as u8 { 42 } else { 1 }
        }
    "#;
    let p = program(source);
    let ir = gcrust::codegen::emit_llvm_ir(&p, false).unwrap();
    assert!(!ir.contains("__cb_strcmp"));
    assert_eq!(unsafe { jit_run_i64_gc(&p, true) }.unwrap(), 42);
    let root = std::env::temp_dir().join(format!("gcr-native-callback-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("main.gcr");
    let bin = root.join("program");
    std::fs::write(&path, source).unwrap();
    let output = support::run_with_timeout(
        Command::new(env!("CARGO_BIN_EXE_gcr"))
            .arg("build")
            .arg(&path)
            .arg("-o")
            .arg(&bin)
            .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()),
        60,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = support::run_with_timeout(
        Command::new(&bin)
            .env("GCR_GC_STRESS", "1")
            .env("GCR_GC_VERIFY", "1"),
        30,
    );
    assert_eq!(output.status.code(), Some(42), "{output:?}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn aggregate_callbacks_are_diagnosed_instead_of_using_a_wrong_abi() {
    let source = r#"
        #[value] struct Pair { a: i64, b: i64 }
        extern "C" fn apply(callback: extern fn(Pair) -> i64) -> i64;
        fn callback(p: Pair) -> i64 { p.a + p.b }
        fn main() -> i64 { unsafe { apply(callback) } }
    "#;
    assert!(error(source).contains("aggregate callback ABI is not implemented"));
}

#[test]
fn completion_owner_binding_requires_a_managed_reference_and_raw_handle() {
    for (source, expected) in [
        ("fn main() -> i64 { unsafe { thread_bind_owner(0, ThreadResult { v: Option::Some(1) }); } 0 }", "handle must be RawPtr"),
        ("fn main() -> i64 { let a = AtomicI64::new(0); unsafe { thread_bind_owner(a.cell, 1); } 0 }", "owner must be a managed reference"),
        ("fn main() -> i64 { let a = AtomicI64::new(0); unsafe { thread_bind_owner(a.cell); } 0 }", "takes a handle and managed owner"),
    ] {
        assert!(error(source).contains(expected), "{source}");
    }
}

#[test]
fn atomic_owner_binding_rejects_invalid_arguments() {
    for (source, expected) in [
        ("fn main() -> i64 { unsafe { atomic_i64_bind_owner(1, AtomicI64::new(0)); } 0 }", "handle must be RawPtr"),
        ("fn main() -> i64 { let a = AtomicI64::new(0); unsafe { atomic_i64_bind_owner(a.cell, 42); } 0 }", "owner must be a managed reference"),
        ("fn main() -> i64 { unsafe { atomic_i64_bind_owner(); } 0 }", "takes a handle and managed owner"),
    ] { assert!(error(source).contains(expected), "{source}"); }
}

#[test]
fn channel_owner_binding_rejects_invalid_arguments() {
    for (source, expected) in [
        ("fn main() -> i64 { unsafe { chan_bind_owner(1, AtomicI64::new(0)); } 0 }", "handle must be RawPtr"),
        ("fn main() -> i64 { let c: Channel<i64> = Channel::new(1); unsafe { chan_bind_owner(c.ctrl, 42); } 0 }", "owner must be a managed reference"),
        ("fn main() -> i64 { unsafe { chan_bind_owner(); } 0 }", "takes a handle and managed owner"),
    ] { assert!(error(source).contains(expected), "{source}"); }
}
