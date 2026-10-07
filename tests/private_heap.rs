//! Private heap graphs retain aliasing and generational barriers; captured
//! graphs publish their children safely across mutators and moving collections.
use std::process::Command;
mod support;

#[test]
fn private_and_shared_object_graphs_survive_moving_collections() {
    let dir = std::env::temp_dir().join(format!("gcr-private-heap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/private_heap.gcr");
    // Independent weighted-sum model: each increment contributes to all
    // remaining reads. Both array entries alias the same item.
    let model = |n: i64| {
        let increments: i64 = (0..n).map(|i| i % 7).sum();
        let weighted: i64 = (0..n).map(|i| (i % 7) * (n - i)).sum();
        3 * n + weighted + n * (n - 1) + 100 * n + 2 * (3 + increments) + n - 1 + 100
    };
    let expected = model(128) + model(129) + model(130) + 1010 + 1020;
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
                        format!("{expected}\n0")
                    } else {
                        expected.to_string()
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
    assert!(counts[0].0 >= 780);
    assert!(counts.iter().all(|count| *count == counts[0]), "{counts:?}");
    std::fs::remove_dir_all(dir).unwrap();
}
