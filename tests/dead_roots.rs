//! Last-use root clearing preserves live values and changes reachability only
//! in optimized modes. Full-debug keeps lexical roots for inspection.
use std::process::Command;
mod support;
#[test]
fn snapshots_exclude_dead_roots_and_keep_later_reads_live() {
    let dir = std::env::temp_dir().join(format!("gcr-dead-roots-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("main.gcr");
    std::fs::write(&source, r#"
        struct Item { n: i64 }
        fn main() -> i64 {
            let dead = Item { n: 0 };
            let live = Item { n: 42 };
            print_int(dead.n);
            heap_snapshot();
            print_int(live.n);
            0
        }
    "#).unwrap();
    for mode in ["jit", "native", "debug"] {
        let binary = dir.join(mode);
        if mode != "jit" {
            let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
            build.arg("build").arg(&source).arg("-o").arg(&binary)
                .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
            if mode == "debug" { build.arg("--debug"); }
            let output = support::run_with_timeout(&mut build, 60);
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        for stress in [false, true] {
            let snapshots = dir.join(format!("{mode}-{stress}"));
            std::fs::create_dir_all(&snapshots).unwrap();
            let mut run = if mode == "jit" {
                let mut run = Command::new(env!("CARGO_BIN_EXE_gcr"));
                run.arg("run").arg(&source).arg("--jit"); run
            } else { Command::new(&binary) };
            run.env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                .env("GCR_GC_VERIFY", "1").env("GCR_GC_WORKERS", "4")
                .env("GCR_HEAP_SNAPSHOT_DIR", &snapshots);
            let output = support::run_with_timeout(&mut run, 30);
            assert!(output.status.success(), "{mode}/{stress}: {}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(),
                if mode == "jit" { "0\n42\n0" } else { "0\n42" });
            let snapshot: serde_json::Value = serde_json::from_slice(
                &std::fs::read(snapshots.join("snapshot-0000.json")).unwrap()).unwrap();
            let objects: Vec<_> = snapshot["objects"].as_array().unwrap().iter()
                .filter(|object| object["type"] == "Item").collect();
            assert_eq!(objects.len(), 2);
            let reachable = objects.iter().filter(|object| object["reachable"] == true).count();
            assert_eq!(reachable, if mode == "debug" { 2 } else { 1 }, "{mode}/{stress}: {objects:?}");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}
