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
    let original = Thread::spawn(|| Payload { n: 42, text: ("moved" + "λ") });
    let surviving_alias = original;
    surviving_alias
}
fn main() -> i64 {
    let t = start();
    let alias = t;
    let mut p = t.join();
    // Force collections after completion and before observing the alias.
    let mut i = 0;
    while i < 100 { let text = "pressure" + "λ"; i = i + text.len() / 10; }
    if !(alias.join().text == "movedλ") { return 1; }
    // Reference results retain identity on every observation.
    p.n = 81;
    if alias.join().n != 81 { return 2; }
    let scalar = (unsafe { thread_spawn(|| 17) });
    if (unsafe { thread_join(scalar) }) != 17 || (unsafe { thread_join(scalar) }) != 17 { return 3; }
    let mut replaced = Thread::spawn(|| 41);
    if replaced.join() != 41 { return 4; }
    let replacement_alias = replaced;
    replaced.result = ThreadResult { v: Option::Some(99) };
    i = 0;
    while i < 100 { let text = "pressure" + "λ"; i = i + text.len() / 10; }
    if replacement_alias.join() != 99 { return 5; }
    0
}
"#;

const ATOMIC_OWNERS: &str = include_str!("fixtures/atomic_ownership.gcr");

const CONCURRENT: &str = r#"
#[value] enum Outcome { Text(String), Number(i64) }
fn measure(value: Outcome) -> i64 {
    match value { Outcome::Text(text) => text.len(), Outcome::Number(n) => n }
}
fn main() -> i64 {
    let gate = AtomicI64::new(0);
    let target = Thread::spawn(|| {
        while gate.load() == 0 { Thread::sleep(1); }
        Outcome::Text("result" + "λ")
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
            while i < 40 { let s = "retire" + "λ"; i = i + 1; }
            println("grandchild completed");
            42
        });
        let mut i = 0;
        while i < 40 { let s = "retire" + "λ"; i = i + 1; }
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
    let initial = Payload { n: 11, text: ("start" + "λ") };
    let child = Thread::spawn(|| {
        if initial.n != 11 || !(initial.text == "startλ") { return 81; }
        let mut p = initial;
        p.n = 12;
        p.text = "join" + "λ";
        0
    });
    if child.join() != 0 || initial.n != 12 || !(initial.text == "joinλ") { return 1; }
    let both = Thread::spawn(|| { if true { return 17; } else { return 18; } });
    if both.join() != 17 { return 2; }
    // Nested closure inference must restore the enclosing return constraint.
    let nested = Thread::spawn(|| {
        if false { return 3; }
        let f = || { if true { return "nested"; } "other" };
        f().len()
    });
    if nested.join() != 6 { return 3; }
    0
}
"#;

fn source(name: &str) -> &'static str {
    match name {
        "repeat" => REPEAT,
        "atomic_owners" => ATOMIC_OWNERS,
        "channel_owners" => include_str!("fixtures/channel_ownership.gcr"),
        "orphan_threads" => include_str!("fixtures/orphan_threads.gcr"),
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
    assert_eq!(unsafe { jit_run_i64_gc(&program, true) }.unwrap(), 0);
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

#[test]
fn safe_thread_spawns_establish_managed_completion_ownership() {
    for (source, binds) in [
        (REPEAT, true),
        ("fn main() -> i64 { let handle = unsafe { thread_spawn(|| 42) }; unsafe { thread_join(handle) } }", false),
    ] {
        let (module, _) = parse_with_prelude(source).unwrap();
        let resolved = resolve_module(module).unwrap();
        let program = lower_program(&resolved.globals).unwrap();
        // Inspect the typed owner as well as the emitted call. A replaceable
        // ThreadResult field cannot anchor the lifetime of the native handle.
        fn check_owners(node: &serde_json::Value, program: &gcrust::core::CoreProgram) {
            if let Some(call) = node.get("RuntimeCall") {
                if call["func"] == "ai_thread_bind_owner" {
                    let layout = call["args"][1]["repr"]["Ref"].as_u64().unwrap() as usize;
                    let name = &program.layouts[layout].name;
                    assert!(name.starts_with("Thread<"), "unstable completion owner: {name}");
                }
            }
            match node {
                serde_json::Value::Object(fields) => {
                    for child in fields.values() { check_owners(child, program); }
                }
                serde_json::Value::Array(children) => {
                    for child in children { check_owners(child, program); }
                }
                _ => {}
            }
        }
        check_owners(&serde_json::to_value(&program).unwrap(), &program);
        for optimized in [false, true] {
            let ir = gcrust::codegen::emit_llvm_ir(&program, optimized).unwrap();
            assert_eq!(ir.contains("call void @ai_thread_bind_owner("), binds,
                "safe wrappers must bind; legacy raw spawn must preserve execution ownership");
        }
    }
}

#[test]
fn atomic_aliases_survive_moving_gc_and_concurrent_updates() {
    verify("atomic_owners");
}

#[test]
fn safe_atomics_bind_their_stable_wrapper_identity() {
    fn visit(node: &serde_json::Value, program: &gcrust::core::CoreProgram, count: &mut usize) {
        if let Some(call) = node.get("RuntimeCall") {
            if call["func"] == "ai_atomic_i64_bind_owner" {
                let layout = call["args"][1]["repr"]["Ref"].as_u64().unwrap() as usize;
                assert_eq!(program.layouts[layout].name, "AtomicI64");
                *count += 1;
            }
        }
        match node {
            serde_json::Value::Object(fields) => { for child in fields.values() { visit(child, program, count); } }
            serde_json::Value::Array(children) => { for child in children { visit(child, program, count); } }
            _ => {}
        }
    }
    for (source, bound) in [
        (ATOMIC_OWNERS, true),
        ("fn main() -> i64 { let cell = unsafe { atomic_i64_new(42) }; unsafe { atomic_i64_load(cell) } }", false),
    ] {
        let (module, _) = parse_with_prelude(source).unwrap();
        let resolved = resolve_module(module).unwrap();
        let program = lower_program(&resolved.globals).unwrap();
        let mut count = 0;
        visit(&serde_json::to_value(&program).unwrap(), &program, &mut count);
        assert_eq!(count > 0, bound);
        for optimized in [false, true] {
            let ir = gcrust::codegen::emit_llvm_ir(&program, optimized).unwrap();
            assert_eq!(ir.contains("call void @ai_atomic_i64_bind_owner("), bound);
        }
    }
}

#[test]
fn channel_cycles_aliases_and_blocked_operations_survive_gc() {
    verify("channel_owners");
}

#[test]
fn safe_channels_bind_the_wrapper_and_raw_queues_remain_unbound() {
    fn visit(node: &serde_json::Value, program: &gcrust::core::CoreProgram, count: &mut usize) {
        if let Some(call) = node.get("RuntimeCall") {
            if call["func"] == "ai_chan_bind_owner" {
                let layout = call["args"][1]["repr"]["Ref"].as_u64().unwrap() as usize;
                assert!(program.layouts[layout].name.starts_with("Channel<"));
                *count += 1;
            }
        }
        match node {
            serde_json::Value::Object(fields) => { for child in fields.values() { visit(child, program, count); } }
            serde_json::Value::Array(children) => { for child in children { visit(child, program, count); } }
            _ => {}
        }
    }
    for (source, binds) in [
        (include_str!("fixtures/channel_ownership.gcr"), true),
        ("struct Message { n: i64 } fn main() -> i64 { let witness: Array<Message> = Array::new(0); let ctrl = unsafe { chan_new(1, witness) }; unsafe { chan_close(ctrl); } 0 }", false),
    ] {
        let (module, _) = parse_with_prelude(source).unwrap();
        let resolved = resolve_module(module).unwrap();
        let program = lower_program(&resolved.globals).unwrap();
        let mut count = 0;
        visit(&serde_json::to_value(&program).unwrap(), &program, &mut count);
        assert_eq!(count > 0, binds);
        for optimized in [false, true] {
            let ir = gcrust::codegen::emit_llvm_ir(&program, optimized).unwrap();
            assert_eq!(ir.contains("call void @ai_chan_bind_owner("), binds);
        }
    }
}

#[test]
fn unobserved_thread_results_and_native_retirement_survive_gc() {
    verify("orphan_threads");
}
