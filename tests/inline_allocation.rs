//! Compiled and runtime allocation share one cursor and publish initialized
//! extents correctly across nursery resets, worker allocation, and GC stress.
use std::process::Command;
mod support;

#[test]
fn inline_and_runtime_allocations_preserve_roots_and_exact_counters() {
    let dir = std::env::temp_dir().join(format!("gcr-inline-allocation-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/inline_allocation.gcr");
    let binary = dir.join("inline-allocation");
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let compiled = support::run_with_timeout(&mut build, 60);
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let mut allocation_counts = Vec::new();
    for jit in [false, true] {
        for stress in [false, true] {
            for workers in [1, 4] {
                let metrics = dir.join(format!("metrics-{jit}-{stress}-{workers}.json"));
                let mut command = if jit {
                    let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                    command.arg("run").arg(&source).arg("--jit");
                    command
                } else {
                    Command::new(&binary)
                };
                command
                    .env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                    .env("GCR_GC_VERIFY", "1")
                    .env("GCR_NURSERY_MB", "1")
                    .env("GCR_TENURED_MB", "16")
                    .env("GCR_GC_WORKERS", workers.to_string())
                    .env("GCR_METRICS_FILE", &metrics);
                let output = support::run_with_timeout(&mut command, 60);
                assert!(
                    output.status.success(),
                    "jit={jit}, stress={stress}, workers={workers}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    String::from_utf8_lossy(&output.stdout).trim(),
                    if jit { "277276\n0" } else { "277276" }
                );
                let data: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(metrics).unwrap()).unwrap();
                if !stress {
                    assert!(
                        data["gc_minor"].as_u64().unwrap() > 0,
                        "must exercise nursery reset"
                    );
                }
                allocation_counts.push((
                    data["alloc_objects"].as_u64().unwrap(),
                    data["alloc_bytes"].as_u64().unwrap(),
                ));
            }
        }
    }
    // Mode and scheduling affect when collection runs, never allocated bytes.
    // 400 Item allocations and 386 arrays must all be counted.
    assert!(allocation_counts[0].0 >= 786);
    assert!(
        allocation_counts
            .iter()
            .all(|count| count == &allocation_counts[0]),
        "{allocation_counts:?}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn collecting_foreign_callback_refreshes_caller_roots_and_copy_out() {
    let dir = std::env::temp_dir().join(format!("gcr-root-mirror-callback-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/root_mirror_callback.gcr");
    for mode in ["jit", "native", "debug"] {
        let binary = dir.join(mode);
        if mode != "jit" {
            let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
            build
                .arg("build")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
            if mode == "debug" {
                build.arg("--debug");
            }
            let output = support::run_with_timeout(&mut build, 60);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        for stress in [false, true] {
            let metrics = dir.join(format!("metrics-{mode}-{stress}.json"));
            let mut command = if mode == "jit" {
                let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                command.arg("run").arg(&source).arg("--jit");
                command
            } else {
                Command::new(&binary)
            };
            command
                .env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                .env("GCR_GC_VERIFY", "1")
                .env("GCR_NURSERY_MB", "1")
                .env("GCR_TENURED_MB", "16")
                .env("GCR_GC_WORKERS", "4")
                .env("GCR_METRICS_FILE", &metrics);
            let output = support::run_with_timeout(&mut command, 60);
            assert!(
                output.status.success(),
                "{mode} stress={stress}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                if mode == "jit" { "1050\n0" } else { "1050" }
            );
            let data: serde_json::Value =
                serde_json::from_slice(&std::fs::read(metrics).unwrap()).unwrap();
            assert!(
                data["gc_minor"].as_u64().unwrap() + data["gc_major"].as_u64().unwrap() > 0,
                "callback must actually collect"
            );
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}
