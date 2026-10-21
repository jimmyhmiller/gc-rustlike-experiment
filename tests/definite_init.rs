//! Zeroed root storage is private runtime state, never an initialized language
//! value. Validate every reachable read, including compound assignments and
//! closure captures, without rejecting correctly initialized deferred bindings.
use gcrust::codegen::jit_run_i64_gc;
use gcrust::{compile::parse_with_prelude, lower::lower_program, resolve::resolve_module};
use std::process::Command;
mod support;
fn program(source: &str) -> Result<gcrust::core::CoreProgram, gcrust::lower::LowerError> {
    let (module, _) = parse_with_prelude(source).unwrap();
    let resolved = resolve_module(module).unwrap();
    lower_program(&resolved.globals)
}
#[test]
fn reads_before_assignment_are_rejected_on_every_path() {
    for source in [
        "fn main() -> i64 { let mut x: i64; x }",
        "struct S { n: i64 } fn main() -> i64 { let mut x: S; x.n }",
        "#[value] struct S { n: i64 } fn main() -> i64 { let mut x: S; x.n }",
        "fn main() -> i64 { let mut x: Array<i64>; x[0] }",
        "fn main() -> i64 { let mut x: fn() -> i64; x() }",
        "fn main() -> i64 { let mut x: Thread<i64>; x.join() }",
        "fn main() -> i64 { let mut x: i64; x += 1; x }",
        "fn maybe(b: bool) -> i64 { let mut x: i64; if b { x = 1; } x } fn main() -> i64 { maybe(false) }",
        "fn maybe(b: bool) -> i64 { let mut x: i64; while b { x = 1; } x } fn main() -> i64 { maybe(false) }",
        "fn main() -> i64 { let mut x: i64; let f = || x; x = 42; f() }",
        "fn main() -> i64 { let f = || { let mut x: i64; x }; f() }",
        "fn maybe(b: bool) -> i64 { let mut x: i64; let _b = b && { x = 1; true }; x } fn main() -> i64 { maybe(false) }",
        "fn maybe(b: bool) -> i64 { let mut x: i64; loop { if b { break; } x = 1; break; } x } fn main() -> i64 { maybe(true) }",
        "struct S { child: S } fn main() -> i64 { let mut x: S; x = S { child: x }; 0 }",
    ] {
        let error = program(source).err().expect("uninitialized read accepted");
        assert!(
            error.msg.contains("may be uninitialized"),
            "{}: {source}",
            error.msg
        );
    }
}
const SOURCE: &str = r#"
struct S { n: i64 }
fn branch(b: bool) -> i64 {
    let mut x: S;
    if b { x = S { n: 7 }; } else { x = S { n: 8 }; }
    x.n
}
fn diverging(b: bool) -> i64 {
    let mut x: i64;
    if b { return 8; } else { x = 7; }
    x
}
fn looping(b: bool) -> i64 {
    let mut x: i64;
    loop { if b { x = 7; break; } else { x = 8; break; } }
    x
}
fn variants(v: Option<i64>) -> i64 {
    let mut x: i64;
    match v { Option::Some(n) => { x = n; }, Option::None => { x = 8; } }
    x
}
fn main() -> i64 {
    let mut x: i64;
    x = branch(true);
    let f = || x;
    let g = || { let mut y: i64; y = 7; y };
    f() + g() + diverging(false) + looping(true) + variants(Option::Some(7)) + 7
}
"#;
#[test]
fn deferred_bindings_work_across_branches_loops_matches_and_closures() {
    assert_eq!(unsafe { jit_run_i64_gc(&program(SOURCE).unwrap(), true) }.unwrap(), 42);
    let root = std::env::temp_dir().join(format!("gcr-definite-init-{}", std::process::id()));
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
fn loop_control_is_typed_and_cannot_escape_a_closure() {
    for (source, message) in [
        ("fn main() -> i64 { break; 0 }", "break outside loop"),
        ("fn main() -> i64 { continue; 0 }", "continue outside loop"),
        (
            "fn main() -> i64 { loop { let f = || { break; }; f(); } }",
            "break outside loop",
        ),
        (
            "fn main() -> i64 { loop { break 42; } }",
            "break values are not supported",
        ),
        ("fn main() -> i64 { loop { break; } true }", "type mismatch"),
    ] {
        let e = program(source)
            .err()
            .expect("invalid control flow accepted");
        assert!(e.msg.contains(message), "{}: {source}", e.msg);
    }
}
