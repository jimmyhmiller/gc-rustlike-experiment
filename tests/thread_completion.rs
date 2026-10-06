//! Completion ownership and closure return inference, with subprocess deadlines.
use gcrust::codegen::jit_run_i64_gc;
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;
use std::process::Command;
mod support;

const REPEAT: &str = r#"
struct Payload { n: i64, text: String }
fn start() -> Thread<Payload> {
    let original = Thread::spawn(|| Payload { n: 42, text: str_concat("moved", "λ") });
    let surviving_alias = original;
    surviving_alias
}
fn main() -> i64 {
    let t = start();
    let alias = t;
    let mut p = t.join();
    // Force collections after completion and before observing the alias.
    let mut i = 0;
    while i < 100 { let text = str_concat("pressure", "λ"); i = i + str_len(text) / 10; }
    if !str_eq(alias.join().text, "movedλ") { return 1; }
    // Reference results retain identity on every observation.
    p.n = 81;
    if alias.join().n != 81 { return 2; }
    let scalar = thread_spawn(|| 17);
    if thread_join(scalar) != 17 || thread_join(scalar) != 17 { return 3; }
    0
}
"#;

const CONCURRENT: &str = r#"
#[value] enum Outcome { Text(String), Number(i64) }
fn measure(value: Outcome) -> i64 {
    match value { Outcome::Text(text) => str_len(text), Outcome::Number(n) => n }
}
fn main() -> i64 {
    let gate = AtomicI64::new(0);
    let target = Thread::spawn(|| {
        while gate.load() == 0 { Thread::sleep(1); }
        Outcome::Text(str_concat("result", "λ"))
    });
    let entered = AtomicI64::new(0);
    let a = Thread::spawn(|| { entered.fetch_add(1); measure(target.join()) });
    let b = Thread::spawn(|| { entered.fetch_add(1); measure(target.join()) });
    while entered.load() != 2 { Thread::sleep(1); }
    gate.store(1);
    if a.join() != 8 || b.join() != 8 || measure(target.join()) != 8 { return 1; }
    if a.join() != 8 || b.join() != 8 { return 2; }
    0
}
"#;

const ABANDONED: &str = r#"
fn main() -> i64 {
    // Runtime teardown must wait for this child AND the grandchild it starts.
    let ignored = Thread::spawn(|| {
        Thread::sleep(10);
        let nested = Thread::spawn(|| {
            Thread::sleep(10);
            let mut i = 0;
            while i < 40 { let s = str_concat("retire", "λ"); i = i + 1; }
            println("grandchild completed");
            42
        });
        let mut i = 0;
        while i < 40 { let s = str_concat("retire", "λ"); i = i + 1; }
        println("child completed");
        0
    });
    0
}
"#;

const INFERENCE: &str = r#"
struct Payload { n: i64, text: String }
impl Sync for Payload {}
fn main() -> i64 {
    let initial = Payload { n: 11, text: str_concat("start", "λ") };
    let child = Thread::spawn(|| {
        if initial.n != 11 || !str_eq(initial.text, "startλ") { return 81; }
        let mut p = initial;
        p.n = 12;
        p.text = str_concat("join", "λ");
        0
    });
    if child.join() != 0 || initial.n != 12 || !str_eq(initial.text, "joinλ") { return 1; }
    let both = Thread::spawn(|| { if true { return 17; } else { return 18; } });
    if both.join() != 17 { return 2; }
    // Nested closure inference must restore the enclosing return constraint.
    let nested = Thread::spawn(|| {
        if false { return 3; }
        let f = || { if true { return "nested"; } "other" };
        str_len(f())
    });
    if nested.join() != 6 { return 3; }
    0
}
"#;

fn source(name: &str) -> &'static str {
    match name {
        "repeat" => REPEAT,
        "concurrent" => CONCURRENT,
        "abandoned" => ABANDONED,
        "inference" => INFERENCE,
        _ => panic!("unknown fixture"),
    }
}

#[test]
fn jit_worker() {
    let Ok(name) = std::env::var("GCR_COMPLETION_FIXTURE") else {
        return;
    };
    let (module, _) = parse_with_prelude(source(&name)).unwrap();
    let resolved = resolve_module(module).unwrap();
    let program = lower_program(&resolved.globals).unwrap();
    assert_eq!(jit_run_i64_gc(&program, true).unwrap(), 0);
}

fn verify(name: &str) {
    let mut jit = Command::new(std::env::current_exe().unwrap());
    jit.args(["--exact", "jit_worker", "--nocapture"])
        .env("GCR_COMPLETION_FIXTURE", name)
        .env("GCR_GC_VERIFY", "1");
    let result = support::run_with_timeout(&mut jit, 30);
    assert!(
        result.status.success(),
        "JIT {name}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    if name == "abandoned" {
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(
            stdout.contains("child completed") && stdout.contains("grandchild completed"),
            "{stdout}"
        );
    }
    let binary = std::env::temp_dir().join(format!("gcr-completion-{name}-{}", std::process::id()));
    let input = binary.with_extension("gcr");
    std::fs::write(&input, source(name)).unwrap();
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .arg("build")
        .arg(&input)
        .arg("-o")
        .arg(&binary)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let result = support::run_with_timeout(&mut build, 60);
    assert!(
        result.status.success(),
        "AOT {name}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut run = Command::new(&binary);
    run.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1");
    let result = support::run_with_timeout(&mut run, 30);
    assert_eq!(
        result.status.code(),
        Some(0),
        "AOT {name}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    if name == "abandoned" {
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(
            stdout.contains("child completed") && stdout.contains("grandchild completed"),
            "{stdout}"
        );
    }
    std::fs::remove_file(input).unwrap();
    std::fs::remove_file(binary).unwrap();
}

#[test]
fn repeat_join_preserves_results_and_identity() {
    verify("repeat");
}
#[test]
fn concurrent_join_preserves_value_references() {
    verify("concurrent");
}
#[test]
fn abandoned_children_are_drained_before_code_unloads() {
    verify("abandoned");
}
#[test]
fn inferred_spawn_closure_accepts_early_return() {
    verify("inference");
}

#[test]
fn inferred_returns_reject_incompatible_types() {
    for source in [
        "fn main() -> i64 { let f = || { if true { return 1; } false }; 0 }",
        "fn main() -> i64 { let f = || { if true { return 1; } return false; }; 0 }",
        "fn main() -> i64 { let f = || { if true { return; } 1 }; 0 }",
        "fn main() -> i64 { let f = || { if true { return 1; } return; }; 0 }",
    ] {
        let (module, _) = parse_with_prelude(source).unwrap();
        let resolved = resolve_module(module).unwrap();
        let error = lower_program(&resolved.globals).unwrap_err();
        assert!(error.msg.contains("expected"), "{}", error.msg);
    }
}
