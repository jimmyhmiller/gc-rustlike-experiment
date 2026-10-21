//! Bounded runtime history must retain lifetime metrics and full streamed logs.
use serde_json::Value;
use std::process::Command;
mod support;

const SOURCE: &str = r#"
fn main() -> i64 {
    let mut i = 0;
    let mut bytes = 0;
    while i < 1500 {
        bytes = bytes + ("allocation" + "pressure").len();
        i = i + 1;
    }
    if bytes != 27000 { return 1; }
    0
}
"#;

fn directory(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("gcr-telemetry-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn build(source: &std::path::Path, binary: &std::path::Path) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
    command
        .arg("build")
        .arg(source)
        .arg("-o")
        .arg(binary)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let output = support::run_with_timeout(&mut command, 60);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn streamed_logs_and_lifetime_metrics_survive_rollover_in_jit_and_aot() {
    let directory = directory("rollover");
    let source = directory.join("main.gcr");
    let binary = directory.join("program");
    std::fs::write(&source, SOURCE).unwrap();
    build(&source, &binary);
    for jit in [false, true] {
        let log = directory.join(format!("{jit}.jsonl"));
        let metrics = directory.join(format!("{jit}.json"));
        let mut command = if jit {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
            command.arg("run").arg(&source);
            command
        } else {
            Command::new(&binary)
        };
        command
            .env("GCR_GC_STRESS", "1")
            .env("GCR_GC_WORKERS", "4")
            .env("GCR_GC_VERIFY", "1")
            .env("GCR_GC_STATS", "1")
            .env("GCR_GC_LOG", &log)
            .env("GCR_METRICS_FILE", &metrics);
        let output = support::run_with_timeout(&mut command, 60);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "jit={jit}: {stderr}");
        if jit {
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0");
        }
        let metrics: Value = serde_json::from_slice(&std::fs::read(metrics).unwrap()).unwrap();
        let body = std::fs::read_to_string(log).unwrap();
        let events: Vec<Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(events.len() > 2 * gcrust::gc::GC_EVENT_HISTORY_LIMIT);
        assert_eq!(
            metrics["gc_events_retained"],
            gcrust::gc::GC_EVENT_HISTORY_LIMIT
        );
        assert_eq!(metrics["gc_major"].as_u64().unwrap(), events.len() as u64);
        assert_eq!(metrics["gc_minor"], 0);
        assert_eq!(metrics["gc_pause_quantile_error_ratio"], 1.0 / 64.0);
        let mut pauses = Vec::new();
        let mut total = 0u64;
        let mut reclaimed = 0u64;
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["seq"].as_u64().unwrap(), index as u64);
            assert_eq!(event["kind"], "major");
            assert_eq!(event["elapsed_ns"], event["pause_ns"]);
            let pause = event["pause_ns"].as_u64().unwrap();
            pauses.push(pause);
            total += pause;
            reclaimed += event["reclaimed_bytes"].as_u64().unwrap();
        }
        pauses.sort_unstable();
        // Runtime milliseconds use four decimal places; allow only their
        // rounding quantum in addition to the certified histogram value error.
        for (percent, key) in [(50usize, "gc_pause_p50_ms"), (99, "gc_pause_p99_ms")] {
            let exact_ns = pauses[(pauses.len() * percent).div_ceil(100) - 1];
            let upper_ns = metrics[key].as_f64().unwrap() * 1.0e6;
            assert!(
                upper_ns + 51.0 >= exact_ns as f64,
                "jit={jit} {key}: {upper_ns} < {exact_ns}"
            );
            assert!(upper_ns <= exact_ns as f64 * (1.0 + 1.0 / 64.0) + 51.0);
        }
        assert!(
            (metrics["gc_pause_total_ms"].as_f64().unwrap() * 1.0e6 - total as f64).abs() <= 51.0
        );
        assert_eq!(metrics["gc_reclaimed_bytes"].as_u64().unwrap(), reclaimed);
        assert!(stderr.contains("quantiles upper estimates"), "{stderr}");
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn trace_creation_errors_are_visible_and_prevent_user_execution_in_jit_and_aot() {
    let directory = directory("open-error");
    let source = directory.join("main.gcr");
    let binary = directory.join("program");
    std::fs::write(&source, "fn main() -> i64 { print_int(77); 0 }").unwrap();
    build(&source, &binary);
    for jit in [false, true] {
        let mut command = if jit {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
            command.arg("run").arg(&source);
            command
        } else {
            Command::new(&binary)
        };
        command.env("GCR_GC_LOG", directory.join("missing/trace.jsonl"));
        let output = support::run_with_timeout(&mut command, 30);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            output.stdout.is_empty(),
            "user entry ran despite trace setup failure"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot create GC log"));
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn metrics_write_errors_remain_visible_and_still_flush_complete_traces() {
    let directory = directory("metrics-error");
    let source = directory.join("main.gcr");
    let binary = directory.join("program");
    let text = SOURCE.replace("    0\n}", "    print_int(77);\n    0\n}");
    assert_ne!(text, SOURCE);
    std::fs::write(&source, text).unwrap();
    build(&source, &binary);
    for jit in [false, true] {
        let log = directory.join(format!("{jit}.jsonl"));
        let mut command = if jit {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
            command.arg("run").arg(&source);
            command
        } else {
            Command::new(&binary)
        };
        command
            .env("GCR_GC_LOG", &log)
            .env("GCR_METRICS_FILE", directory.join("missing/metrics.json"))
            .env("GCR_GC_STRESS", "1")
            .env("GCR_GC_STATS", "1")
            .env("GCR_GC_WORKERS", "4");
        let output = support::run_with_timeout(&mut command, 60);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "77");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("cannot write GC metrics"), "{stderr}");
        let collections = stderr
            .lines()
            .find(|line| line.contains("minor +"))
            .unwrap()
            .split_whitespace()
            .nth(4)
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let text = std::fs::read_to_string(log).unwrap();
        let events: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(collections > 2 * gcrust::gc::GC_EVENT_HISTORY_LIMIT);
        assert_eq!(
            events.len(),
            collections,
            "metrics failure must not skip the trace flush"
        );
        assert_eq!(
            events.last().unwrap()["seq"].as_u64().unwrap(),
            collections as u64 - 1
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}
