//! String constructors preserve UTF-8; clamping cannot panic across the native
//! ABI, and invalid character boundaries produce a defined language failure.
use gcrust::codegen::jit_run_i64_gc;
use gcrust::{compile::parse_with_prelude, lower::lower_program, resolve::resolve_module};
use std::process::Command;
mod support;
fn run(source: &str) -> i64 {
    let (m, _) = parse_with_prelude(source).unwrap();
    let r = resolve_module(m).unwrap();
    unsafe { jit_run_i64_gc(&lower_program(&r.globals).unwrap(), true) }.unwrap()
}
fn build(root: &std::path::Path, name: &str, source: &str) -> std::path::PathBuf {
    let path = root.join(format!("{name}.gcr"));
    let bin = root.join(name);
    std::fs::write(&path, source).unwrap();
    let output = support::run_with_timeout(
        Command::new(env!("CARGO_BIN_EXE_gcr"))
            .arg("build")
            .arg(&path)
            .arg("-o")
            .arg(&bin)
            .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()),
        60,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    bin
}
#[test]
fn substrings_validate_utf8_and_clamp_before_ordering_bounds() {
    let source = r#"
        fn main() -> i64 {
            let s = "é🦀";
            if s[0..2] != "é" || s[2..6] != "🦀" { return 1; }
            if s.chars().len() != 2 { return 2; }
            if str_substring(s, 9223372036854775807, 0) != "" { return 3; }
            if str_substring(s, -9223372036854775808, 6) != s { return 4; }
            if str_substring("abc", 2, 1) != "" { return 5; }
            42
        }
    "#;
    assert_eq!(run(source), 42);
    let root = std::env::temp_dir().join(format!("gcr-string-boundary-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let bin = build(&root, "valid", source);
    let output = support::run_with_timeout(
        Command::new(&bin)
            .env("GCR_GC_STRESS", "1")
            .env("GCR_GC_VERIFY", "1"),
        30,
    );
    assert_eq!(output.status.code(), Some(42), "{output:?}");
    for (i, expression) in ["\"é\"[0..1]", "\"é\"[1..2]", "str_substring(\"🦀\", 1, 1)"]
        .iter()
        .enumerate()
    {
        let source = format!("fn main() -> i64 {{ {expression}.len() }}");
        let name = format!("invalid-{i}");
        let bin = build(&root, &name, &source);
        for native in [true, false] {
            let mut cmd = if native {
                Command::new(&bin)
            } else {
                let mut c = Command::new(env!("CARGO_BIN_EXE_gcr"));
                c.arg("run").arg(root.join(format!("{name}.gcr")));
                c
            };
            let output = support::run_with_timeout(cmd.env("GCR_GC_STRESS", "1"), 30);
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("string byte range is not on UTF-8 character boundaries"),
                "{output:?}"
            );
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn legacy_text_read_rejects_binary_input() {
    let root = std::env::temp_dir().join(format!("gcr-string-file-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("invalid.bin");
    std::fs::write(&path, [0xff, 0xc0, 0x80]).unwrap();
    let path = serde_json::to_string(path.to_str().unwrap()).unwrap();
    let source =
        format!("fn main() -> i64 {{ if read_file({path}).is_empty() {{ 42 }} else {{ 1 }} }}");
    assert_eq!(run(&source), 42);
    let bin = build(&root, "binary-read", &source);
    let output = support::run_with_timeout(
        Command::new(&bin)
            .env("GCR_GC_STRESS", "1")
            .env("GCR_GC_VERIFY", "1"),
        30,
    );
    assert_eq!(output.status.code(), Some(42), "{output:?}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn blocked_output_publishes_roots_and_allows_child_collection() {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let root = std::env::temp_dir().join(format!("gcr-blocked-output-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let done = root.join("done.txt");
    let quoted = serde_json::to_string(done.to_str().unwrap()).unwrap();
    let go = root.join("go.txt");
    let quoted_go = serde_json::to_string(go.to_str().unwrap()).unwrap();
    let source = format!(
        r#"
        fn main() -> i64 {{
            let mut text = "x";
            for i in 0..21 {{ text = text + text; }}
            let worker = Thread::spawn(|| {{
                while fs_kind({quoted_go}).is_err() {{ Thread::sleep(1); }}
                let mut items: Vec<String> = Vec::new();
                for i in 0..200 {{ items.push(i.to_string()); }}
                if items[199] != "199" {{ return 1; }}
                fs_write_text_atomic({quoted}, "done").expect("completion file");
                0
            }});
            print!("{{}}", text);
            worker.join();
            42
        }}
    "#
    );
    let bin = build(&root, "blocked-output", &source);
    let mut child = Command::new(&bin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GCR_GC_STRESS", "1")
        .env("GCR_GC_VERIFY", "1")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stdout = child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let prefix_reader = std::thread::spawn(move || {
        let mut prefix = [0];
        let result = stdout.read_exact(&mut prefix);
        sender.send((stdout, prefix, result)).unwrap();
    });
    let (mut stdout, prefix, result) = match receiver.recv_timeout(Duration::from_secs(30)) {
        Ok(value) => value,
        Err(error) => {
            let _ = child.kill();
            child.wait().unwrap();
            prefix_reader.join().unwrap();
            panic!("output did not start: {error}");
        }
    };
    prefix_reader.join().unwrap();
    if let Err(error) = result {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!("output failed: {error}; {output:?}");
    }
    assert_eq!(prefix, [b'x']);
    // Observe the first output byte before allowing the worker to finish. Then
    // leave the rest unread: the oversized write cannot return or safepoint,
    // and child collection must complete while that native output is blocked.
    std::fs::write(go, "go").unwrap();
    while !done.exists() {
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("child GC did not progress while output was blocked: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read_to_string(&done).unwrap(), "done");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
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
            panic!("output worker did not finish");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(42));
    let bytes = reader.join().unwrap();
    assert_eq!(bytes.len(), (1 << 21) - 1);
    assert!(bytes.iter().all(|b| *b == b'x'));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn string_scans_reload_caller_roots_after_runtime_calls() {
    for (expression, symbol) in [("str_hash(s)", "ai_str_hash_v2"), ("if str_eq(s, t) { 1 } else { 0 }", "ai_str_eq_v2")] {
        let source = format!("fn main() -> i64 {{ let s = \"source\"; let t = \"other\"; let r = {expression}; r + str_get(s, 0) + str_get(t, 0) }}");
        let (module, _) = parse_with_prelude(&source).unwrap();
        let resolved = resolve_module(module).unwrap();
        let program = lower_program(&resolved.globals).unwrap();
        let ir = gcrust::codegen::emit_llvm_ir(&program, false).unwrap();
        assert!(ir.lines().any(|line| line.contains("icmp ule i64") && line.contains(&gcrust::runtime::STRING_SCAN_LEAF_BYTES.to_string())), "missing bounded scan guard");
        let call = ir.lines().position(|line| line.contains("call i64") && line.contains(&format!("@{symbol}("))).unwrap();
        let after: String = ir.lines().skip(call + 1).take_while(|line| !line.contains("call i64 @ai_str_get(")).collect::<Vec<_>>().join("\n");
        assert!(after.contains("gc.relocated"), "{symbol} must check relocation before reusing caller roots:\n{after}");
        assert!(after.contains("roots.reload"), "{symbol}: missing caller root reload");
    }
}

#[test]
fn generated_string_scans_preserve_roots_during_concurrent_collection() {
    let source = include_str!("fixtures/string_scans.gcr");
    assert_eq!(run(source), 0);
    let root = std::env::temp_dir().join(format!("gcr-string-scans-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let binary = build(&root, "scans", source);
    for workers in ["1", "4"] {
        let output = support::run_with_timeout(Command::new(&binary).env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1").env("GCRUST_POISON", "1").env("GCR_GC_WORKERS", workers), 30);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn float_parse_call_guards_and_reloads_typed_results() {
    let source = r#"fn main() -> i64 { let s = " 3.25 "; let value = str_to_float(s); if value == 3.25 { str_get(s, 1) } else { 0 } }"#;
    let (module, _) = parse_with_prelude(source).unwrap();
    let resolved = resolve_module(module).unwrap();
    let program = lower_program(&resolved.globals).unwrap();
    let ir = gcrust::codegen::emit_llvm_ir(&program, false).unwrap();
    assert!(ir.lines().any(|line| line.contains("icmp ule i64") && line.contains(&gcrust::runtime::STRING_SCAN_LEAF_BYTES.to_string())), "missing bounded float-parse guard");
    let call = ir.lines().position(|line| line.contains("call double @ai_str_to_float_v2(")).unwrap();
    let after: String = ir.lines().skip(call+1).take_while(|line| !line.contains("call i64 @ai_str_get(")).collect::<Vec<_>>().join("\n");
    assert!(after.contains("gc.relocated") && after.contains("roots.reload"), "float parse must reload caller roots: {after}");
    assert!(ir.lines().any(|line| line.contains("phi double") && line.contains("%strtof")), "float parse merge must preserve f64 ABI");
    assert_eq!(run(source), 51);
}

#[test]
fn generated_float_parsing_preserves_caller_roots() {
    let source = include_str!("fixtures/float_parse.gcr");
    assert_eq!(run(source), 0);
    let root = std::env::temp_dir().join(format!("gcr-float-parse-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let binary = build(&root,"parse",source);
    for workers in ["1","4"] {
        let output=support::run_with_timeout(Command::new(&binary).env("GCR_GC_STRESS","1").env("GCR_GC_VERIFY","1").env("GCRUST_POISON","1").env("GCR_GC_WORKERS",workers),30);
        assert!(output.status.success(),"{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(),"42");
    }
    std::fs::remove_dir_all(root).unwrap();
}
