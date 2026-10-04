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

/// Drain stdout/stderr concurrently while enforcing a deadline. Killing and
/// waiting on a timed-out child prevents a regression from leaving orphan work.
#[allow(dead_code)]
pub fn run_with_timeout(command: &mut Command, seconds: u64) -> std::process::Output {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let err_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            let out = out_reader.join().unwrap();
            let err = err_reader.join().unwrap();
            panic!(
                "command exceeded {seconds}s: {command:?}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    std::process::Output {
        status,
        stdout: out_reader.join().unwrap(),
        stderr: err_reader.join().unwrap(),
    }
}
