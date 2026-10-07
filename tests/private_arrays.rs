//! Escape proofs and entry-only roots preserve aliases, dynamic metadata and
//! relocation under an allocating sibling mutator. Debug mode stays generic.
use std::process::Command;
mod support;

#[test]
fn private_aliases_and_entry_roots_survive_parallel_moving_gc() {
    let dir = std::env::temp_dir().join(format!("gcr-private-arrays-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/private_arrays.gcr");
    let mut counts = Vec::new();
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
            let result = support::run_with_timeout(&mut build, 60);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        for stress in [false, true] {
            for workers in [1, 4] {
                let metrics = dir.join(format!("{mode}-{stress}-{workers}.json"));
                let mut run = if mode == "jit" {
                    let mut run = Command::new(env!("CARGO_BIN_EXE_gcr"));
                    run.arg("run").arg(&source).arg("--jit");
                    run
                } else {
                    Command::new(&binary)
                };
                run.env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                    .env("GCR_GC_VERIFY", "1")
                    .env("GCR_GC_WORKERS", workers.to_string())
                    .env("GCR_NURSERY_MB", "1")
                    .env("GCR_TENURED_MB", "16")
                    .env("GCR_METRICS_FILE", &metrics);
                let result = support::run_with_timeout(&mut run, 60);
                assert!(
                    result.status.success(),
                    "{mode}/{stress}/{workers}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                assert_eq!(
                    String::from_utf8_lossy(&result.stdout).trim(),
                    if mode == "jit" { "7538\n0" } else { "7538" }
                );
                let data: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(metrics).unwrap()).unwrap();
                assert!(
                    data["gc_minor"].as_u64().unwrap() + data["gc_major"].as_u64().unwrap() > 0
                );
                counts.push((
                    data["alloc_objects"].as_u64().unwrap(),
                    data["alloc_bytes"].as_u64().unwrap(),
                ));
            }
        }
    }
    assert!(counts[0].0 >= 802);
    assert!(counts.iter().all(|count| *count == counts[0]), "{counts:?}");
    std::fs::remove_dir_all(dir).unwrap();
}
