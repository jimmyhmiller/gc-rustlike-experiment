//! Channel FIFO, typed close outcomes, GC roots and waiter wakeups through both
//! execution modes. Subprocess deadlines bound regressions that strand waiters.
use std::process::Command;
mod support;

const SOURCE: &str = include_str!("fixtures/channel_contract.gcr");

#[test]
fn channel_close_wakes_waiters_and_preserves_values_in_jit_and_aot() {
    let dir = std::env::temp_dir().join(format!("gcr-channel-contract-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("gcr.toml"),
        "[package]\nname = \"channel-contract\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.gcr"), SOURCE).unwrap();
    let executable = dir.join("channel-contract");
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .arg("build")
        .arg(&dir)
        .arg("-o")
        .arg(&executable)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let out = support::run_with_timeout(&mut build, 60);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for jit in [false, true] {
        for stress in [false, true] {
            let mut cmd = if jit {
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
                cmd.arg("run").arg(&dir).arg("--jit");
                cmd
            } else {
                Command::new(&executable)
            };
            cmd.env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                .env("GCR_GC_VERIFY", "1")
                .env("GCR_GC_WORKERS", "4")
                .env("GCR_NURSERY_MB", "1")
                .env("GCR_TENURED_MB", "8");
            let out = support::run_with_timeout(&mut cmd, 60);
            assert_eq!(
                out.status.code(),
                Some(0),
                "jit={jit} stress={stress}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn malformed_channel_intrinsics_report_frontend_errors() {
    use gcrust::{compile::parse_with_prelude, lower::lower_program, resolve::resolve_module};
    for body in [
        "chan_close(42); 0",
        "chan_sender_clone(42); 0",
        "chan_waiting_receivers(42)",
        "let a: Array<i64> = Array::new(0); chan_new(1, a); 0",
        "let a: Array<i64> = Array::new(0); let c = atomic_i64_new(0); chan_recv(a, c)",
        "let c: Channel<String> = Channel::new(1); let wrong = ChannelMessage { value: 42 }; chan_send(c.buf, c.ctrl, wrong)",
    ] {
        let source = format!("fn main() -> i64 {{ unsafe {{ {body} }} }}");
        let (module, _) = parse_with_prelude(&source).unwrap();
        let resolved = resolve_module(module).unwrap();
        assert!(lower_program(&resolved.globals).is_err(), "{body}");
    }
}

#[test]
fn native_buffer_pressure_collects_without_managed_exhaustion_in_jit_and_aot() {
    let dir = std::env::temp_dir().join(format!("gcr-native-pressure-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("gcr.toml"),
        "[package]\nname = \"native-pressure\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.gcr"),
        include_str!("fixtures/native_pressure.gcr"),
    )
    .unwrap();
    let executable = dir.join("native-pressure");
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .arg("build")
        .arg(&dir)
        .arg("-o")
        .arg(&executable)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let output = support::run_with_timeout(&mut build, 60);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for jit in [false, true] {
        for stress in [false, true] {
            let mut run = if jit {
                let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                command.arg("run").arg(&dir).arg("--jit");
                command
            } else {
                Command::new(&executable)
            };
            let metrics_path = dir.join(format!("metrics-{jit}-{stress}.json"));
            run.env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                .env("GCR_GC_VERIFY", "1")
                .env("GCR_GC_WORKERS", "4")
                .env("GCR_NURSERY_MB", "1")
                .env("GCR_TENURED_MB", "8")
                .env("GCR_METRICS_FILE", &metrics_path);
            let output = support::run_with_timeout(&mut run, 60);
            assert_eq!(
                output.status.code(),
                Some(0),
                "jit={jit} stress={stress}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let metrics: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&metrics_path).unwrap()).unwrap();
            assert_eq!(metrics["native_buffer_bytes"], 0);
            let peak = metrics["native_buffer_peak_bytes"].as_u64().unwrap();
            assert!(
                peak > 0 && peak <= (1 << 20) + 262144,
                "jit={jit} stress={stress}: {metrics}"
            );
            if !stress {
                assert!(
                    metrics["native_pressure_collections"].as_u64().unwrap() > 0,
                    "{metrics}"
                );
                assert!(metrics["gc_major"].as_u64().unwrap() > 0, "{metrics}");
            }
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn large_channel_native_phases_preserve_message_graphs_and_release_storage() {
    let dir = std::env::temp_dir().join(format!("gcr-channel-buffers-{}",std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/channel_buffers.gcr");
    let binary=dir.join("app");
    let output=support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr")).arg("build").arg(&source).arg("-o").arg(&binary)
        .env("GCRUST_RUNTIME_LIB",support::runtime_staticlib()),60);
    assert!(output.status.success(),"{output:?}");
    for jit in [false,true] {
        for workers in ["1","4"] {
            let metrics=dir.join(format!("metrics-{jit}-{workers}.json"));
            let mut command=if jit { let mut c=Command::new(env!("CARGO_BIN_EXE_gcr"));c.arg("run").arg(&source).arg("--jit");c }else{Command::new(&binary)};
            let output=support::run_with_timeout(command.env("GCR_GC_WORKERS",workers).env("GCR_GC_STRESS","1")
                .env("GCR_NURSERY_MB","1").env("GCR_TENURED_MB","16").env("GCR_STRESS_HEAP_MB","16")
                .env("GCR_GC_VERIFY","1").env("GCRUST_POISON","1").env("GCR_METRICS_FILE",&metrics),60);
            assert!(output.status.success(),"{output:?}");
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(),if jit {"42\n0"}else{"42"});
            let report:serde_json::Value=serde_json::from_slice(&std::fs::read(metrics).unwrap()).unwrap();
            assert_eq!(report["native_buffer_bytes"],0);
            assert!(report["native_buffer_peak_bytes"].as_u64().unwrap()>=8193*8);
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}
