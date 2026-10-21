//! Callable expressions preserve source order and GC roots across their operands.
use std::process::Command;
mod support;

#[test]
fn callable_expressions_work_in_jit_and_native_under_collection() {
    let root = std::env::temp_dir().join(format!("gcr-callable-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("main.gcr");
    let binary = root.join("program");
    std::fs::write(&source, include_str!("fixtures/callable_expressions.gcr")).unwrap();
    let check = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr")).arg("check").arg(&source), 30);
    assert!(check.status.success(), "{check:?}");
    let build = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr"))
        .arg("build").arg(&source).arg("-o").arg(&binary)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()), 60);
    assert!(build.status.success(), "{build:?}");
    for native in [false, true] {
        for stress in ["0", "1"] {
            let mut command = if native { Command::new(&binary) } else {
                let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                command.arg("run").arg(&source).arg("--jit"); command
            };
            let out = support::run_with_timeout(command.env("GCR_GC_STRESS", stress)
                .env("GCR_GC_VERIFY", "1").env("GCR_GC_WORKERS", "4"), 30);
            if native { assert_eq!(out.status.code(), Some(42), "{out:?}"); }
            else {
                assert!(out.status.success(), "{out:?}");
                assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_callable_expressions_fail_before_native_output() {
    let root = std::env::temp_dir().join(format!("gcr-callable-errors-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("main.gcr");
    let binary = root.join("program");
    for (text, diagnostic) in [
        ("fn main()->i64{42()}", "cannot call"),
        ("fn value()->i64{42} fn main()->i64{let value=7;value()}", "cannot call"),
        ("enum C<T>{Empty,Value(T)} fn apply(Empty:i64)->i64{Empty()} fn main()->i64{apply(7)}", "cannot call"),
        ("fn main()->i64{(|x:i64|x)()}", "expects 1 args"),
        ("fn main()->i64{(|x:i64|x)(true)}", "type mismatch"),
        ("fn main()->i64{(||true)()}", "type mismatch"),
    ] {
        std::fs::write(&source, text).unwrap();
        for mode in ["check", "build", "run"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
            command.arg(mode).arg(&source);
            if mode == "build" { command.arg("-o").arg(&binary); }
            if mode == "run" { command.arg("--jit"); }
            let out = support::run_with_timeout(&mut command, 30);
            assert!(!out.status.success(), "unexpected acceptance: {text} in {mode}");
            assert!(String::from_utf8_lossy(&out.stderr).contains(diagnostic), "{text} in {mode}: {out:?}");
            assert!(!binary.exists(), "failed check created native output");
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
