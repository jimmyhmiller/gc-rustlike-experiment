//! Assignment locations are captured before reading their old value or RHS.
use std::process::Command;
mod support;

const SOURCE: &str = include_str!("fixtures/compound_places.gcr");

#[test]
fn compound_places_evaluate_once_before_rhs_in_jit_and_native() {
    let root = std::env::temp_dir().join(format!("gcr-compound-places-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("main.gcr");
    let binary = root.join("program");
    std::fs::write(&source, SOURCE).unwrap();
    let output = support::run_with_timeout(
        Command::new(env!("CARGO_BIN_EXE_gcr"))
            .arg("build")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()),
        60,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for native in [false, true] {
        for stress in [false, true] {
            let mut command = if native {
                Command::new(&binary)
            } else {
                let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                command.arg("run").arg(&source).arg("--jit");
                command
            };
            let output = support::run_with_timeout(
                command
                    .env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                    .env("GCR_GC_VERIFY", "1"),
                30,
            );
            if native {
                assert_eq!(output.status.code(), Some(42), "{output:?}");
            } else {
                assert!(output.status.success(), "{output:?}");
                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
