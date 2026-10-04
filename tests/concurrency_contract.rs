//! Synchronized publication regressions, not certification of arbitrary races.
use gcrust::codegen::jit_run_i64_gc;
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;
mod support;

const SOURCE: &str = r#"
struct Payload { n: i64, text: String }
// Transitional bridge: all conflicting payload accesses are synchronized.
impl Sync for Payload {}
fn main() -> i64 {
    let initial = Payload { n: 11, text: str_concat("start", "λ") };
    let child = Thread::spawn(|| {
        if initial.n != 11 || !str_eq(initial.text, "startλ") { 81 } else {
            let mut p = initial;
            p.n = 12;
            p.text = str_concat("join", "λ");
            0
        }
    });
    if child.join() != 0 { return 81; }
    if initial.n != 12 || !str_eq(initial.text, "joinλ") { return 82; }
    let queue: Channel<Payload> = Channel::new(1);
    let producer = Thread::spawn(|| {
        let p = Payload { n: 13, text: str_concat("channel", "λ") };
        queue.send(p);
        0
    });
    let received = queue.recv();
    if received.n != 13 || !str_eq(received.text, "channelλ") { return 83; }
    producer.join();
    let published = Payload { n: 0, text: "" };
    let ready = AtomicI64::new(0);
    let writer = Thread::spawn(|| {
        let mut p = published;
        p.n = 14;
        p.text = str_concat("atomic", "λ");
        ready.store(1);
        0
    });
    while ready.load() == 0 { Thread::sleep(1); }
    // Sleep avoids busy spinning; ready.load(), not sleep, publishes the data.
    if published.n != 14 || !str_eq(published.text, "atomicλ") { return 84; }
    writer.join();
    0
}
"#;

#[test]
fn synchronized_publication_jit_and_aot_with_moving_gc() {
    let (module, _) = parse_with_prelude(SOURCE).unwrap();
    let resolved = resolve_module(module).unwrap();
    let program = lower_program(&resolved.globals).unwrap();
    assert_eq!(jit_run_i64_gc(&program, true).unwrap(), 0);
    let binary = std::env::temp_dir().join(format!("gcr-contract-{}", std::process::id()));
    let source = binary.with_extension("gcr");
    std::fs::write(&source, SOURCE).unwrap();
    let mut build = std::process::Command::new(env!("CARGO_BIN_EXE_gcr"));
    build.arg("build").arg(&source).arg("-o").arg(&binary)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let result = support::run_with_timeout(&mut build, 60);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let mut run = std::process::Command::new(&binary);
    run.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1");
    let result = support::run_with_timeout(&mut run, 30);
    std::fs::remove_file(source).unwrap();
    std::fs::remove_file(binary).unwrap();
    assert_eq!(result.status.code(), Some(0), "{}", String::from_utf8_lossy(&result.stderr));
}
