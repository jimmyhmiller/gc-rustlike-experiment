//! Recursive polling must keep allocation-free spinners responsive and their
//! live references correct across another mutator's moving collections.
use std::process::Command;
mod support;

#[test]
fn recursive_scalar_array_spinners_participate_in_moving_collections() {
    let dir = std::env::temp_dir().join(format!("gcr-recursive-poll-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recursive_poll.gcr");
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
                    if mode == "jit" {
                        "1999084\n0"
                    } else {
                        "1999084"
                    }
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
    assert!(counts[0].0 >= 2003);
    assert!(counts.iter().all(|count| *count == counts[0]), "{counts:?}");
    std::fs::remove_dir_all(dir).unwrap();
}
