//! Heap-explorer P2: the on-demand mid-execution `heap_snapshot()` intrinsic.
//! A running program snapshots its OWN heap at a chosen point; objects live on
//! the stack at that point are correctly reported reachable (real mid-execution
//! roots), unlike the program-end dump where everything is garbage.

use std::process::Command;

#[test]
fn heap_snapshot_intrinsic_captures_live_roots_midexec() {
    // x and y are live (rooted on the stack) when heap_snapshot() runs.
    let src = "\
struct Box3 { a: i64, b: i64, c: i64 }
fn main() -> i64 {
    let x = Box3 { a: 1, b: 2, c: 3 };
    let y = Box3 { a: 4, b: 5, c: 6 };
    heap_snapshot();
    x.a + y.c
}
";
    let dir = std::env::temp_dir().join(format!("gcr_snap_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let srcp = dir.join("snap.gcr");
    std::fs::write(&srcp, src).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_gcr"))
        .args(["run", srcp.to_str().unwrap()])
        .env("GCR_HEAP_SNAPSHOT_DIR", &dir)
        .output()
        .expect("run gcr");
    assert!(
        out.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let snap = dir.join("snapshot-0000.json");
    assert!(snap.exists(), "snapshot file not written to GCR_HEAP_SNAPSHOT_DIR");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snap).unwrap()).expect("valid snapshot JSON");
    let s = &json["summary"];
    assert_eq!(s["objects"].as_i64(), Some(2), "two Box3 allocated");
    // The whole point of a mid-execution snapshot: x and y are live → reachable
    // (program-end dump would show 0 reachable). Proves the frame-publish +
    // real-roots scan works for the requesting thread.
    assert_eq!(
        s["reachable_objects"].as_i64(),
        Some(2),
        "live stack roots must be reachable mid-execution; summary: {s}"
    );
    assert_eq!(s["roots"].as_array().map(|a| a.len()), Some(2), "two rooted objects");
    assert!(s["snapshot_pause_ns"].as_i64().unwrap_or(0) > 0, "STW pause must be measured");

    let _ = std::fs::remove_dir_all(&dir);
}


mod support;

#[test]
fn heap_snapshot_renders_compiled_inline_enum_variants_and_payloads() {
    let root = std::env::temp_dir().join(format!("gcr-inline-enum-snapshot-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("main.gcr");
    std::fs::write(&source, r#"
        struct Node { n: i64 }
        #[value] struct Pair { left: Node, right: Node, n: i64 }
        #[value] enum Nested { Empty, Full(Node), Pair(Pair) }
        #[value] enum Cell { Empty, Full(Node), Pair(Pair), Nested(Nested), Named { node: Node, n: i64 } }
        struct Board { c: Cell }
        enum BoxChoice { Empty, Full(Pair) }
        fn main() -> i64 {
            let empty = Board { c: Cell::Empty };
            let full = Board { c: Cell::Full(Node { n: 42 }) };
            let paired = Board { c: Cell::Pair(Pair { left: Node { n: 11 }, right: Node { n: 22 }, n: 7 }) };
            let nested = Board { c: Cell::Nested(Nested::Pair(Pair { left: Node { n: 33 }, right: Node { n: 44 }, n: 9 })) };
            let boxed = BoxChoice::Full(Pair { left: Node { n: 55 }, right: Node { n: 66 }, n: 13 });
            let named = Board { c: Cell::Named { node: Node { n: 77 }, n: 5 } };
            heap_snapshot();
            let n = match full.c { Cell::Full(node) => node.n, _ => 0 };
            let p = match paired.c { Cell::Pair(pair) => pair.n, _ => 0 };
            let q = match nested.c { Cell::Nested(value) => match value { Nested::Pair(pair) => pair.n, _ => 0 }, _ => 0 };
            let e = match empty.c { Cell::Empty => 0, _ => 1 };
            let b = match boxed { BoxChoice::Full(pair) => pair.n, _ => 0 };
            let z = match named.c { Cell::Named { node: point, n: count } => point.n + count, _ => 0 };
            if n + p + q + e + b + z == 153 { 0 } else { 1 }
        }
    "#).unwrap();
    let binary = root.join("program");
    let built = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr"))
        .arg("build").arg(&source).arg("-o").arg(&binary)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()), 60);
    assert!(built.status.success(), "{built:?}");
    for (mode, native, stress) in [("jit-normal", false, false), ("jit-stress", false, true),
                                  ("native-normal", true, false), ("native-stress", true, true)] {
        let directory = root.join(mode);
        std::fs::create_dir(&directory).unwrap();
        let mut command = if native { Command::new(&binary) } else {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
            command.args(["run", source.to_str().unwrap(), "--jit"]);
            if stress { command.arg("--gc-stress"); }
            command
        };
        command.env("GCR_HEAP_SNAPSHOT_DIR", &directory).env("GCR_GC_VERIFY", "1").env_remove("GCR_GC_STRESS");
        if native && stress { command.env("GCR_GC_STRESS", "1"); }
        let output = support::run_with_timeout(&mut command, 60);
        assert!(output.status.success(), "stress={stress}: {output:?}");
        let snapshot: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("snapshot-0000.json")).unwrap()).unwrap();
        let objects = snapshot["objects"].as_array().unwrap();
        let node = objects.iter().find(|object| object["render"] == "Node { n: 42 }").unwrap();
        let expected = format!("Board {{ c: Cell::Full(#{}) }}", node["id"].as_u64().unwrap());
        assert!(objects.iter().any(|object| object["render"] == expected), "{snapshot}");
        assert!(objects.iter().any(|object| object["render"] == "Board { c: Cell::Empty }"), "{snapshot}");
        let node_id = |n| objects.iter().find(|object| object["render"] == format!("Node {{ n: {n} }}")).unwrap()["id"].as_u64().unwrap();
        let pair = format!("Board {{ c: Cell::Pair(Pair {{ left: #{}, right: #{}, n: 7 }}) }}", node_id(11), node_id(22));
        let nested = format!("Board {{ c: Cell::Nested(Nested::Pair(Pair {{ left: #{}, right: #{}, n: 9 }})) }}", node_id(33), node_id(44));
        let boxed = format!("BoxChoice::Full(Pair {{ left: #{}, right: #{}, n: 13 }})", node_id(55), node_id(66));
        assert!(objects.iter().any(|object| object["render"] == pair), "mode={mode}: {snapshot}");
        assert!(objects.iter().any(|object| object["render"] == nested), "mode={mode}: {snapshot}");
        assert!(objects.iter().any(|object| object["render"] == boxed), "mode={mode}: {snapshot}");
        let named = format!("Board {{ c: Cell::Named(node: #{}, n: 5) }}", node_id(77));
        assert!(objects.iter().any(|object| object["render"] == named), "mode={mode}: {snapshot}");
        assert_eq!(snapshot["summary"]["reachable_objects"], 14, "mode={mode}: {snapshot}");
    }
    std::fs::remove_dir_all(root).unwrap();
}
