#![cfg(unix)]
//! Fatal diagnostics park before blocking on stderr and own their text.
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
mod support;

#[test]
fn blocked_panic_output_allows_other_mutators_to_collect() {
    let root = std::env::temp_dir().join(format!("gcr-panic-progress-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let go = root.join("go");
    let done = root.join("done");
    let source = r#"
        fn main() -> i64 {
            let mut text = "x";
            for i in 0..21 { text = text + text; }
            let worker = Thread::spawn(|| {
                while fs_kind(__GO__).is_err() { Thread::sleep(1); }
                let mut values: Vec<String> = Vec::new();
                for i in 0..200 { values.push(i.to_string()); }
                if values[199] != "199" { return 1; }
                fs_write_text_atomic(__DONE__, "done").expect("completion");
                0
            });
            panic(text);
            worker.join()
        }
    "#;
    let source = source
        .replace("__GO__", &serde_json::to_string(&go).unwrap())
        .replace("__DONE__", &serde_json::to_string(&done).unwrap());
    let path = root.join("main.gcr");
    let binary = root.join("program");
    std::fs::write(&path, source).unwrap();
    let output = support::run_with_timeout(
        Command::new(env!("CARGO_BIN_EXE_gcr"))
            .arg("build")
            .arg(&path)
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
        let mut command = if native {
            Command::new(&binary)
        } else {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
            command.arg("run").arg(&path).arg("--jit");
            command
        };
        let mut child = command
            .env("GCR_GC_STRESS", "1")
            .env("GCR_GC_VERIFY", "1")
            .env_remove("GCR_HEAP_SNAPSHOT_DIR")
            .env_remove("GCR_HEAP_DUMP")
            .env_remove("GCR_GC_STATS")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stderr.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let prefix_reader = std::thread::spawn(move || {
            let mut prefix = [0];
            let result = pipe.read_exact(&mut prefix);
            sender.send((pipe, prefix, result)).unwrap();
        });
        let (mut pipe, prefix, result) = match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(value) => value,
            Err(error) => {
                let _ = child.kill();
                child.wait().unwrap();
                prefix_reader.join().unwrap();
                panic!("panic output did not start: {error}");
            }
        };
        prefix_reader.join().unwrap();
        if result.is_err() || prefix != [b'g'] {
            let _ = child.kill();
            let mut output = Vec::new();
            pipe.read_to_end(&mut output).unwrap();
            child.wait().unwrap();
            panic!(
                "panic output failed: {result:?}, {prefix:?}, {}",
                String::from_utf8_lossy(&output)
            );
        }
        // A multi-megabyte diagnostic cannot fit into this undrained pipe.
        std::fs::write(&go, "go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done.exists() {
            if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                let _ = child.kill();
                child.wait().unwrap();
                panic!("worker could not collect while panic output was blocked");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let drain = std::thread::spawn(move || {
            let mut bytes = vec![b'g'];
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("panic did not finish");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "{status:?}");
        let bytes = drain.join().unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("gc-rust: panic: "));
        assert_eq!(text.len(), "gc-rust: panic: ".len() + 2097152 + 1);
        assert!(text.ends_with('\n'));
        std::fs::remove_file(&go).unwrap();
        std::fs::remove_file(&done).unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_observability_output_preserves_jit_and_native_results() {
    let root = std::env::temp_dir().join(format!("gcr-diagnostic-teardown-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("main.gcr");
    let binary = root.join("program");
    std::fs::write(
        &path,
        "fn main() -> i64 { let text = 42.to_string(); text.len() + 40 }",
    )
    .unwrap();
    let build = support::run_with_timeout(
        Command::new(env!("CARGO_BIN_EXE_gcr"))
            .arg("build")
            .arg(&path)
            .arg("-o")
            .arg(&binary)
            .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()),
        60,
    );
    assert!(build.status.success(), "{build:?}");
    for native in [false, true] {
        for dump_file_failure in [false, true] {
            let mut command = if native {
                Command::new(&binary)
            } else {
                let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                command.arg("run").arg(&path).arg("--jit");
                command
            };
            command
                .env("GCR_HEAP_DUMP", "json")
                .env("GCR_GC_STATS", "1")
                .env("GCR_ALLOC_PROFILE", "1")
                .env("GCR_GC_STRESS", "1")
                .env("GCR_GC_VERIFY", "1")
                .stderr(Stdio::from(std::fs::File::open("/dev/null").unwrap()))
                .stdout(Stdio::null());
            if dump_file_failure {
                command.env("GCR_HEAP_DUMP_FILE", &root);
            } else {
                command.env_remove("GCR_HEAP_DUMP_FILE");
            }
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("failed diagnostic output hung");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(
                status.code(),
                Some(if native { 42 } else { 0 }),
                "{status:?}"
            );
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
