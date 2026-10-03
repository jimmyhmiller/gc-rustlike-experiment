//! End-to-end AOT tests: `gcr build` must produce a standalone native binary
//! that runs against the statically-linked GC runtime and exits with the
//! program's `i64` result truncated to the process exit code (`& 0xFF`).
//!
//! We drive the public `gcrust::codegen::build_executable` path directly (the
//! same one `gcr build` uses), link, run the binary, and assert its exit code.

use std::path::{Path, PathBuf};
use std::process::Command;

use gcrust::codegen::build_executable;
use gcrust::lexer::lex;
use gcrust::lower::lower_program;
use gcrust::parser::parse_module;
use gcrust::resolve::resolve_module;

/// Build the profile-matched fixture runtime in its isolated target directory.
mod support;

fn ensure_runtime_lib() -> PathBuf {
    support::runtime_staticlib()
}

/// Compile `src` to a native executable at `out`, returning nothing on success.
fn build(src: &str, out: &Path, runtime_lib: &Path) {
    // Safety: tests in this file run serially w.r.t. this var because each sets
    // it to the same path before building.
    unsafe {
        std::env::set_var("GCRUST_RUNTIME_LIB", runtime_lib);
    }
    let module = parse_module(&lex(src).unwrap()).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    build_executable(&prog, out, &[]).expect("build_executable failed");
}

/// Run an executable and return its process exit code.
fn run_exit_code(bin: &Path) -> i32 {
    let status = Command::new(bin)
        .status()
        .expect("failed to run AOT binary");
    status.code().expect("AOT binary terminated by signal")
}

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("gcrust_aot_test_{}_{}", std::process::id(), name));
    p
}

#[test]
fn aot_fib_exit_code() {
    let lib = ensure_runtime_lib();
    let out = tmp("fib");
    build(include_str!("../examples/fib.gcr"), &out, &lib);
    // fib(32) = 2178309; the process exit code is the low byte.
    assert_eq!(run_exit_code(&out), 2178309 & 0xFF);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn aot_heap_struct_and_enum() {
    let lib = ensure_runtime_lib();
    let out = tmp("shapes");
    // Struct + enum-with-payload + match + GC heap allocation.
    build(include_str!("../examples/shapes.gcr"), &out, &lib);
    // 3*3*3 + 4*5 = 47.
    assert_eq!(run_exit_code(&out), 47);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn aot_binary_trees_gc_under_load() {
    let lib = ensure_runtime_lib();
    let out = tmp("binary_trees");
    build(include_str!("../examples/binary_trees.gcr"), &out, &lib);
    // checksum 5242840; low byte = 216. Proves the GC runs under load in the
    // AOT-linked binary without crashing.
    assert_eq!(run_exit_code(&out), 5242840 & 0xFF);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn aot_arithmetic_edges_match_jit_contract() {
    let lib = ensure_runtime_lib();
    let out = tmp("arithmetic");
    build(
        r#"
        fn check(min: i64, neg: i64, count: i64) -> i64 {
            if min / neg == min && min % neg == 0 && 1 << count == 1 && 1 << neg == min { 42 } else { 0 }
        }
        fn main() -> i64 { check(0 - 9223372036854775807 - 1, 0 - 1, 64) }
    "#,
        &out,
        &lib,
    );
    assert_eq!(run_exit_code(&out), 42);
    let _ = std::fs::remove_file(&out);
}
