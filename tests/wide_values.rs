use std::process::Command;
#[allow(dead_code)]
mod support;

#[test]
fn wide_scalar_records_compile_without_tracking_scalar_root_lifetimes() {
    let root = std::env::temp_dir().join(format!("gcr-wide-values-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for (count, reference) in [(8190, false), (4096, true)] {
        let fields = (0..count)
            .map(|i| format!("f{i}:i64"))
            .collect::<Vec<_>>()
            .join(",");
        let arguments = (0..count)
            .map(|i| format!("f{i}:0"))
            .collect::<Vec<_>>()
            .join(",");
        let source = root.join(format!("{count}.gcr"));
        let prefix = if reference { "node:Node," } else { "" };
        let initializer = if reference { "node:Node {n:40}," } else { "" };
        let tail = if reference {
            "let later=Node {n:2}; print_int(wide.node.n+later.n); 0"
        } else {
            "wide.f0"
        };
        std::fs::write(&source, format!("#[heap] struct Node {{n:i64}} #[value] struct Wide {{{prefix}{fields}}} fn main()->i64 {{let wide=Wide {{{initializer}{arguments}}}; {tail}}}")).unwrap();
        let output = support::run_with_timeout(
            Command::new(env!("CARGO_BIN_EXE_gcr"))
                .arg("emit")
                .arg("llvm")
                .arg(&source)
                .arg("--no-opt"),
            30,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if reference {
            if cfg!(any(target_os = "macos", target_os = "linux")) {
                // Keep the compiler's 30-second deadline and independently
                // catch quadratic optimizer memory growth. The outer watchdog
                // allows the resource wrapper time to reap/report its child.
                let checker = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("scripts/check_compiler_resources.py");
                let output = support::run_with_timeout(
                    Command::new("python3").arg(checker)
                        .args(["--max-rss-bytes", "1073741824", "--timeout-seconds", "30", "--"])
                        .arg(env!("CARGO_BIN_EXE_gcr")).arg("run").arg(&source).arg("--jit")
                        .env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1"),
                    40,
                );
                assert!(output.status.success(), "stdout: {}\nstderr: {}",
                    String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
                let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(report["stdout"].as_str().unwrap().trim(), "42\n0");
                assert!(report["max_rss_bytes"].as_u64().unwrap() <= 1024 * 1024 * 1024);
            } else {
                let output = support::run_with_timeout(
                    Command::new(env!("CARGO_BIN_EXE_gcr"))
                        .arg("run").arg(&source).arg("--jit")
                        .env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1"), 30);
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42\n0");
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
