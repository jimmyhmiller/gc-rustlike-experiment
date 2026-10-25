#[allow(dead_code)]
mod support;
use std::process::Command;

#[test]
fn repeated_engine_destruction_keeps_runtime_ownership_valid() {
    let tokens = gcrust::lexer::lex("fn main() -> i64 { 42 }").unwrap();
    let module = gcrust::parser::parse_module(&tokens).unwrap();
    let resolved = gcrust::resolve::resolve_module(module).unwrap();
    let program = gcrust::lower::lower_program(&resolved.globals).unwrap();
    for _ in 0..32 {
        assert_eq!(unsafe { gcrust::codegen::jit_run_i64(&program) }.unwrap(), 42);
    }
}

#[test]
fn literals_and_empty_arrays_keep_canonical_identity_across_functions() {
    let source = r#"
        #[inline(never)] fn a() -> String { "shared" }
        #[inline(never)] fn b() -> String { "shared" }
        #[inline(never)] fn x() -> Array<i64> { array_new(0) }
        #[inline(never)] fn y() -> Array<i64> { array_new(0) }
        fn main() -> i64 {
            if std::ptr::eq(a(), b()) && std::ptr::eq(x(), y()) { 42 } else { 0 }
        }
    "#;
    let (module, _) = gcrust::compile::parse_with_prelude(source).unwrap();
    let resolved = gcrust::resolve::resolve_module(module).unwrap();
    let program = gcrust::lower::lower_program(&resolved.globals).unwrap();
    assert_eq!(unsafe { gcrust::codegen::jit_run_i64(&program) }.unwrap(), 42);
}

#[test]
fn cross_function_writes_do_not_keep_stale_heap_memory_facts() {
    let path = std::env::temp_dir().join(format!("gcr-clobber-{}.gcr", std::process::id()));
    std::fs::write(&path, r#"
        struct Buffer { values: Array<i64> }
        struct Holder { buffer: Buffer }
        #[inline(never)] fn replace(mut holder: Holder) {
            holder.buffer.values = array_new(32);
        }
        fn main() -> i64 {
            let mut holder = Holder { buffer: Buffer { values: array_new(8) } };
            replace(holder);
            holder.buffer.values[14] = 42;
            holder.buffer.values[14]
        }
    "#).unwrap();
    {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
        command.arg("run").arg(&path).arg("--jit");
        let output = command.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn eager_machine_compilation_finishes_before_user_entry() {
    let path = std::env::temp_dir().join(format!("gcr-eager-{}.gcr", std::process::id()));
    std::fs::write(&path, r#"
        #[inline(never)] fn alternate(n: i64) -> i64 { n * 2 }
        fn main() -> i64 {
            let args: Vec<String> = std::env::args().collect();
            stderr_line("USER_ENTRY");
            if args.len() == 1 { 42 } else { alternate(3) }
        }
    "#).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
    command.arg("run").arg(&path).arg("--jit").env("GCR_COMPILE_TIMINGS", "1");
    let output = support::run_with_timeout(&mut command, 60);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let compiled = stderr.find("gcr timing: JIT machine compilation").expect("machine compilation timing");
    let entry = stderr.find("USER_ENTRY").expect("entry marker");
    assert!(compiled < entry, "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn unresolved_eager_units_fail_before_user_entry() {
    let path = std::env::temp_dir().join(format!("gcr-eager-error-{}.gcr", std::process::id()));
    let symbol = format!("gcr_missing_eager_target_{}", std::process::id());
    std::fs::write(&path, format!(r#"
        extern "C" {{ fn {symbol}() -> i64; }}
        #[inline(never)] fn alternate() -> i64 {{ unsafe {{ {symbol}() }} }}
        fn main() -> i64 {{
            let args: Vec<String> = std::env::args().collect();
            if args.len() > 100 {{ return alternate(); }}
            print_str("USER_ENTRY");
            42
        }}
    "#)).unwrap();
    let output = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr"))
        .arg("run").arg(&path).arg("--jit"), 30);
    std::fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{}", String::from_utf8_lossy(&output.stdout));
    assert!(String::from_utf8_lossy(&output.stderr).contains(&symbol), "{}", String::from_utf8_lossy(&output.stderr));
}
