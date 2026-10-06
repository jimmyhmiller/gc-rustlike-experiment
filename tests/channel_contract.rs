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
        "let a: Array<i64> = array_new(0); chan_new(1, a); 0",
        "let a: Array<i64> = array_new(0); let c = atomic_i64_new(0); chan_recv(a, c)",
        "let c: Channel<String> = Channel::new(1); let wrong = ChannelMessage { value: 42 }; chan_send(c.buf, c.ctrl, wrong)",
    ] {
        let source = format!("fn main() -> i64 {{ {body} }}");
        let (module, _) = parse_with_prelude(&source).unwrap();
        let resolved = resolve_module(module).unwrap();
        assert!(lower_program(&resolved.globals).is_err(), "{body}");
    }
}
