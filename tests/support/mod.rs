use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Keep fixture archives separate from dependency builds, which can replace the
/// top-level archive after Cargo releases its lock. All fixtures request the
/// same package/features in this directory; OnceLock also avoids redundant
/// builds inside each integration-test process.
pub fn runtime_staticlib() -> PathBuf {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target = manifest.join("target/aot-test-runtime");
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let mut command = Command::new(env!("CARGO"));
        command
            .args(["build", "-p", "gcrust-rt", "--target-dir"])
            .arg(&target)
            .current_dir(&manifest);
        if profile == "release" {
            command.arg("--release");
        }
        let status = command.status().expect("build AOT fixture runtime");
        assert!(status.success(), "building fixture runtime failed");
        let lib = target.join(profile).join("libgcrust_rt.a");
        assert!(
            lib.is_file(),
            "runtime archive missing at {}",
            lib.display()
        );
        lib
    })
    .clone()
}
