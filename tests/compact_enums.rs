//! Physical enum shapes preserve nominal reflection and moving-GC payloads.
use std::process::Command;
mod support;

#[test]
fn compact_enum_shapes_survive_parallel_moving_gc() {
    let dir = std::env::temp_dir().join(format!("gcr-compact-enums-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compact_enums.gcr");
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
                let snapshots = dir.join(format!("snapshots-{mode}-{stress}-{workers}"));
                std::fs::create_dir_all(&snapshots).unwrap();
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
                    .env("GCR_METRICS_FILE", &metrics)
                    .env("GCR_HEAP_SNAPSHOT_DIR", &snapshots);
                let result = support::run_with_timeout(&mut run, 60);
                assert!(
                    result.status.success(),
                    "{mode}/{stress}/{workers}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                assert_eq!(
                    String::from_utf8_lossy(&result.stdout).trim(),
                    if mode == "jit" { "51716\n0" } else { "51716" }
                );
                let snapshot: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(snapshots.join("snapshot-0000.json")).unwrap()).unwrap();
                let objects = snapshot["objects"].as_array().unwrap();
                let trees: Vec<_> = objects.iter().filter(|o| o["type"] == "Tree").collect();
                assert!(trees.iter().any(|o| o["bytes"] == 16));
                assert!(trees.iter().any(|o| o["bytes"] == 32));
                assert!(trees.iter().all(|o| o["bytes"] == 16 || o["bytes"] == 32));
                // A value payload is omitted from scalar reflection metadata,
                // but must retain its actual payload allocation and traced ref.
                assert!(objects.iter().any(|o| o["type"] == "Pack" && o["bytes"] == 40));
                assert!(objects.iter().any(|o| o["type"] == "Pack" && o["bytes"] == 16));
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
    assert!(counts[0].0 >= 50494);
    assert!(counts.iter().all(|count| *count == counts[0]), "{counts:?}");
    std::fs::remove_dir_all(dir).unwrap();
}
