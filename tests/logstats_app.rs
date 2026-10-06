//! Differential application checks: exact atomic work distribution and immutable
//! summary publication, with bounded JIT/AOT processes and moving-GC stress.
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;
mod support;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("gcr-logstats-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn log_analysis_matches_serial_model_in_jit_and_native_under_contention() {
    let fixture = Fixture::new();
    let bin = fixture.0.join("logstats");
    let lib = support::runtime_staticlib();
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .args(["build", "apps/gcr-logstats", "-o"])
        .arg(&bin)
        .env("GCRUST_RUNTIME_LIB", lib)
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    let out = support::run_with_timeout(&mut build, 60);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut paths = Vec::new();
    let mut bytes = 0;
    let mut lines = 0;
    let mut errors = 0;
    let mut warnings = 0;
    for i in 0..24 {
        let body = match i % 4 {
            0 => "INFO α ready\nERROR failure\r\nWARN retry\nERROR WARN both",
            1 => "",
            2 => "\nWARN WARN twice\nERROR ERROR twice\n",
            _ => "info lower case\nER\nROR split token\nlast λ\n",
        };
        let path = fixture.0.join(format!("worker input {i} λ.log"));
        std::fs::write(&path, body).unwrap();
        paths.push(path);
        bytes += body.len();
        for line in body.split_terminator('\n') {
            lines += 1;
            errors += usize::from(line.contains("ERROR"));
            warnings += usize::from(line.contains("WARN"));
        }
    }
    let expected = json!({"files":paths.len(),"bytes":bytes,"lines":lines,"error_lines":errors,"warning_lines":warnings});
    for jit in [false, true] {
        for stress in [false, true] {
            for workers in ["1", "4", "64"] {
                let mut cmd = if jit {
                    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
                    cmd.args(["run", "apps/gcr-logstats", "--jit", "--"]);
                    cmd
                } else {
                    Command::new(&bin)
                };
                cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
                    .arg(workers)
                    .args(&paths)
                    .env("GCR_GC_VERIFY", "1")
                    .env("GCR_GC_WORKERS", "4")
                    .env("GCR_GC_STRESS", if stress { "1" } else { "0" });
                let out = support::run_with_timeout(&mut cmd, 60);
                assert!(
                    out.status.success(),
                    "jit={jit} stress={stress} workers={workers}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let actual: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(
                    actual, expected,
                    "jit={jit} stress={stress} workers={workers}"
                );
            }
        }
        for workers in ["0", "65", "-1", "x", "999999999999999999999"] {
            let mut cmd = if jit {
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
                cmd.args(["run", "apps/gcr-logstats", "--jit", "--"]);
                cmd
            } else {
                Command::new(&bin)
            };
            cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
                .arg(workers)
                .arg(&paths[0]);
            let out = support::run_with_timeout(&mut cmd, 30);
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert!(String::from_utf8_lossy(&out.stderr).contains("worker count"));
        }
        // A failed file must not produce a misleading partial success summary.
        let bad = fixture.0.join("invalid.log");
        std::fs::write(&bad, [0xff, 0xfe]).unwrap();
        let oversized = fixture.0.join("oversized.log");
        std::fs::write(&oversized, vec![b'x'; 1_048_577]).unwrap();
        for path in [&bad, &fixture.0.join("missing.log"), &oversized] {
            let mut cmd = if jit {
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
                cmd.args(["run", "apps/gcr-logstats", "--jit", "--"]);
                cmd
            } else {
                Command::new(&bin)
            };
            cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
                .arg("4")
                .args(&paths)
                .arg(path)
                .env("GCR_GC_STRESS", "1")
                .env("GCR_GC_VERIFY", "1");
            let out = support::run_with_timeout(&mut cmd, 60);
            assert_eq!(
                out.status.code(),
                Some(1),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.stdout.is_empty());
            assert!(String::from_utf8_lossy(&out.stderr).starts_with("gcr-logstats:"));
        }
    }
}
