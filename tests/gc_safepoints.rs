//! Allocation-free mutators must participate in generational collection.
use std::process::Command;
mod support;

const SOURCE: &str = r#"
fn pressure() -> i64 {
    let mut i = 0;
    let mut bytes = 0;
    while i < 100000 {
        bytes = bytes + ("allocation" + "pressure").len();
        i = i + 1;
    }
    bytes
}
fn warm_old() -> i64 {
    // Keep ~800 KB alive through a minor, then release it on return. Later
    // promotion genuinely needs a major to reclaim these tenured bytes.
    let mut old: Array<i64> = Array::new(100000);
    old[99999] = 31;
    let bytes = pressure();
    bytes + old[99999]
}
fn main() -> i64 {
    let entered = AtomicI64::new(0);
    let stop = AtomicI64::new(0);
    let spinner = Thread::spawn(|| {
        entered.store(1);
        while stop.load() == 0 {}
        17
    });
    while entered.load() == 0 { Thread::yield_now(); }
    let warmed = warm_old();
    // ~320 KB of live nursery objects exceed the remaining old-space
    // headroom. Dead allocation churn alone must not force a major.
    let mut live: Array<i64> = Array::new(40000);
    live[39999] = 23;
    let bytes = pressure();
    stop.store(1);
    if spinner.join() != 17 || bytes != 1800000 || warmed != 1800031 || live[39999] != 23 { return 1; }
    0
}
"#;

#[test]
fn nursery_collection_requests_polls_from_allocation_free_jit_and_aot_mutators() {
    let binary = std::env::temp_dir().join(format!("gcr-gc-safepoints-{}", std::process::id()));
    let source = binary.with_extension("gcr");
    std::fs::write(&source, SOURCE).unwrap();
    let mut jit = Command::new(env!("CARGO_BIN_EXE_gcr"));
    jit.arg("run")
        .arg(&source)
        .env("GCR_TENURED_MB", "1")
        .env("GCR_NURSERY_MB", "1")
        .env("GCR_GC_STATS", "1")
        .env("GCR_GC_VERIFY", "1");
    verify(&mut jit);
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let output = support::run_with_timeout(&mut build, 60);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut native = Command::new(&binary);
    native
        .env("GCR_TENURED_MB", "1")
        .env("GCR_NURSERY_MB", "1")
        .env("GCR_GC_STATS", "1")
        .env("GCR_GC_VERIFY", "1");
    verify(&mut native);
    std::fs::remove_file(source).unwrap();
    std::fs::remove_file(binary).unwrap();
}

fn verify(command: &mut Command) {
    let output = support::run_with_timeout(command, 10);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let minor = stderr
        .lines()
        .find(|line| line.contains("minor +"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|count| count.parse::<usize>().ok())
        .expect("minor count");
    assert!(
        minor > 0,
        "fixture did not exercise nursery collection: {stderr}"
    );
    let major = stderr
        .lines()
        .find(|line| line.contains("minor +"))
        .and_then(|line| line.split_whitespace().nth(4))
        .and_then(|count| count.parse::<usize>().ok())
        .expect("major count");
    assert!(
        major > 0,
        "fixture did not exercise major-before-minor collection: {stderr}"
    );
}
