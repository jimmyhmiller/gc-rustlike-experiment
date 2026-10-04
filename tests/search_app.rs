//! Native gc-rust application correctness against a simple serial model.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
mod support;

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("gcr-search-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn write(&self, path: &str, bytes: &[u8]) {
        let p = self.0.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn executable() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("gcr-search-executable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("gcr-search");
        let lib = support::runtime_staticlib();
        let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
        command
            .args(["build", "apps/gcr-search", "-o"])
            .arg(&bin)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("GCRUST_RUNTIME_LIB", lib);
        let out = support::run_with_timeout(&mut command, 60);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        bin
    })
    .as_path()
}
fn run(args: &[&str], stress: bool) -> Output {
    let mut cmd = Command::new(executable());
    cmd.args(args).env_remove("GCR_GC_STRESS");
    if stress {
        cmd.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1");
    }
    support::run_with_timeout(&mut cmd, 20)
}
fn run_jit(args: &[&str], stress: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["run", "apps/gcr-search", "--jit"])
        .env_remove("GCR_GC_STRESS");
    if stress { cmd.arg("--gc-stress").env("GCR_GC_VERIFY", "1"); }
    cmd.arg("--").args(args);
    support::run_with_timeout(&mut cmd, 30)
}
fn successful(out: Output) -> Output {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}
fn rows(out: Output) -> Vec<Value> {
    let out = successful(out);
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn reference(files: &[(&str, &str)], needle: &str) -> Vec<Value> {
    let mut result = Vec::new();
    for (path, body) in files {
        // split_terminator preserves internal empty lines and final CR bytes.
        for (line, text) in body.split_terminator('\n').enumerate() {
            if let Some(column) = text.find(needle) {
                result.push(json!({"path":path,"line":line+1,"byte_column":column+1,"text":text}));
            }
        }
    }
    result
}

#[test]
fn index_reload_and_search_match_serial_reference_under_gc_stress() {
    let f = Fixture::new("reference");
    let files = [
        ("a.txt", "alpha α needle\n\nneedle needle\r\nlast"),
        ("dir/b.txt", "quoted \"needle\" \\ tab\t\nneedle"),
        ("dir/empty.txt", ""),
        ("line\nname λ.txt", "αneedle\n"),
        ("z.txt", "a needle appears here\n"),
    ];
    for (path, body) in files {
        f.write(&format!("tree/{path}"), body.as_bytes());
    }
    f.write("tree/.git/ignored", b"needle");
    f.write("tree/target/ignored", b"needle");
    f.write("tree/invalid.bin", &[0xff, 0xfe]);
    f.write("tree/nul.bin", b"needle\0hidden");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(f.0.join("tree"), f.0.join("tree/cycle")).unwrap();
    }
    let root = f.0.join("tree");
    let index = f.0.join("snapshot");
    let root = root.to_str().unwrap();
    let index = index.to_str().unwrap();
    successful(run(&["index", root, index], true));
    let before = std::fs::read(index).unwrap();
    successful(run_jit(&["index", root, index], true));
    assert_eq!(std::fs::read(index).unwrap(), before);
    for needle in ["needle", "α", "\"", "not present"] {
        assert_eq!(
            rows(run(&["query", needle, index], true)),
            reference(&files, needle),
            "query {needle}"
        );
        assert_eq!(rows(run_jit(&["query", needle, index], true)), reference(&files, needle), "JIT query {needle}");
    }
    // An index is a snapshot, independent of subsequent source changes.
    f.write("tree/a.txt", b"changed source");
    assert_eq!(
        rows(run(&["query", "needle", index], false)),
        reference(&files, "needle")
    );
    successful(run(&["index", root, index], false));
    assert_ne!(std::fs::read(index).unwrap(), before);
    // Repeated indexing is byte-for-byte deterministic.
    let refreshed = std::fs::read(index).unwrap();
    successful(run(&["index", root, index], false));
    assert_eq!(std::fs::read(index).unwrap(), refreshed);
}

#[test]
fn malformed_index_and_cli_errors_are_diagnostics_not_crashes() {
    let f = Fixture::new("invalid");
    let index = f.0.join("snapshot");
    let path = index.to_str().unwrap();
    let bad = [
        "",
        "GCRSEARCH2\n0\n",
        "GCRSEARCH1\n1\n100\nx",
        "GCRSEARCH1\n9223372036854775808\n",
        "GCRSEARCH1\n0\ntrailing",
        "GCRSEARCH1\n1\n0\n0\n",
    ];
    for encoded in bad {
        std::fs::write(&index, encoded).unwrap();
        let out = run(&["query", "x", path], true);
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).starts_with("gcr-search:"));
    }
    let valid = b"GCRSEARCH1\n1\n5\na.txt6\nhello\n";
    for length in 0..valid.len() {
        std::fs::write(&index, &valid[..length]).unwrap();
        assert_eq!(
            run(&["query", "hello", path], true).status.code(),
            Some(1),
            "accepted truncated prefix {length}"
        );
    }
    std::fs::write(&index, bad[bad.len() - 1]).unwrap();
    let usage = run(&[], false);
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&usage.stderr).contains("usage:"));
    assert_eq!(run(&["query", "", path], false).status.code(), Some(1));
    assert_eq!(
        run(&["query", "two\nlines", path], false).status.code(),
        Some(1)
    );
    let missing = run(&["index", "/gcr-search-does-not-exist", path], false);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("stat"));
    // Failure doesn't replace an existing index.
    assert_eq!(std::fs::read_to_string(&index).unwrap(), bad[bad.len() - 1]);
}

#[test]
fn output_inside_source_tree_is_not_indexed_and_empty_tree_is_supported() {
    let f = Fixture::new("self");
    let root = f.0.to_str().unwrap();
    let index = f.0.join("custom-index");
    let index = index.to_str().unwrap();
    successful(run(&["index", root, index], true));
    assert!(rows(run(&["query", "GCRSEARCH", index], true)).is_empty());
    f.write("a.txt", b"hello\n");
    successful(run(&["index", root, index], true));
    let before = std::fs::read(index).unwrap();
    successful(run(&["index", root, index], true));
    assert_eq!(std::fs::read(index).unwrap(), before);
}
