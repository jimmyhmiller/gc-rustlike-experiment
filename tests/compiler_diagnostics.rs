//! Compiler errors must reach the command line without crashing the renderer.
use std::process::Command;
#[allow(dead_code)]
mod support;

#[test]
fn array_length_intrinsic_rejects_non_arrays_before_codegen() {
    let root = std::env::temp_dir().join(format!("gcr-array-length-types-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("main.gcr");
    let binary = root.join("program");
    for source in [
        "fn main() -> i64 { array_len(42) }",
        "fn main() -> i64 { array_len(true) }",
        "fn main() -> i64 { array_len(\"abc\") }",
        "struct Count { n: i64 } fn main() -> i64 { array_len(Count { n: 42 }) }",
        "fn main() -> i64 { array_len(()) }",
        "fn length<T>(value: T) -> i64 { array_len(value) } fn main() -> i64 { length(42) }",
    ] {
        std::fs::write(&path, source).unwrap();
        for mode in ["check", "build", "jit"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
            command.arg(if mode == "jit" { "run" } else { mode }).arg(&path);
            if mode == "jit" {
                command.arg("--jit");
            } else if mode == "build" {
                command.arg("-o").arg(&binary);
            }
            let output = support::run_with_timeout(&mut command, 30);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{mode}: {source}: {stderr}");
            assert!(stderr.contains("array_len on a non-array"), "{mode}: {source}: {stderr}");
            assert!(stderr.contains(&format!("{}:", path.display())), "{mode}: {stderr}");
            assert!(stderr.contains('^'), "{mode}: {stderr}");
            assert!(!stderr.contains("panicked"), "{mode}: {stderr}");
            assert!(!binary.exists(), "invalid array length reached native output");
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_unicode_escapes_and_eof_report_the_user_file() {
    let root = std::env::temp_dir().join(format!("gcr-source-diagnostics-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("main.gcr");
    for source in [
        "é",
        "🦀",
        "fn main() -> i64 { let x = \"\\é\"; 0 }",
        "fn main() -> i64 { let x = \"\\🦀\"; 0 }",
        "fn main() -> i64 { let x = \"\\",
        "fn main() -> i64 { let x = \"\\u{41",
        "fn main() -> i64 {",
        "fn main() -> i64 {\n",
    ] {
        std::fs::write(&path, source).unwrap();
        let output = support::run_with_timeout(
            Command::new(env!("CARGO_BIN_EXE_gcr"))
                .arg("check")
                .arg(&path),
            30,
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{source:?}: {stderr}");
        assert!(stderr.contains("error:"), "{source:?}: {stderr}");
        assert!(
            stderr.contains(&format!("{}:", path.display())),
            "{source:?}: {stderr}"
        );
        assert!(stderr.contains('^'), "{source:?}: {stderr}");
        assert!(!stderr.contains("panicked"), "{source:?}: {stderr}");
        assert!(!stderr.contains("standard library"), "{source:?}: {stderr}");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn duplicate_declarations_report_source_errors_without_replacing_output() {
    let root = std::env::temp_dir().join(format!("gcr-duplicate-members-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("main.gcr");
    let binary = root.join("program");
    std::fs::write(&binary, b"preserve existing output").unwrap();
    for (declaration, diagnostic) in [
        ("struct Record {x:i64,x:i64}", "duplicate field `x`"),
        ("#[value] struct Record {x:i64,x:i64}", "duplicate field `x`"),
        ("enum Choice {Value{x:i64,x:i64}}", "duplicate field `x`"),
        ("enum Choice {Value,Value}", "duplicate variant `Value`"),
        ("fn value(x:i64,x:i64)->i64{x}", "duplicate parameter `x`"),
        (r#"extern "C" fn value(x:i64,x:i64)->i64;"#, "duplicate parameter `x`"),
        ("struct Record {} impl Record {fn value(x:i64,x:i64)->i64{x}}", "duplicate parameter `x`"),
        ("trait Contract {fn value(x:i64,x:i64)->i64;}", "duplicate parameter `x`"),
        ("trait Contract {fn value(x:i64,x:i64)->i64{x}}", "duplicate parameter `x`"),
        ("fn helper()->i64{let value=|x:i64,x:i64| x;42}", "duplicate parameter `x`"),
    ] {
        std::fs::write(&path, format!("{declaration} fn main()->i64 {{42}}")).unwrap();
        for mode in ["check", "build", "jit"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
            command.arg(if mode == "jit" { "run" } else { mode }).arg(&path);
            if mode == "jit" { command.arg("--jit"); }
            if mode == "build" { command.arg("-o").arg(&binary); }
            let output = support::run_with_timeout(&mut command, 30);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{mode}: {declaration}: {stderr}");
            assert!(stderr.contains(diagnostic), "{mode}: {stderr}");
            assert!(stderr.contains(&format!("{}:", path.display())), "{mode}: {stderr}");
            assert!(stderr.contains('^'), "{mode}: {stderr}");
            assert!(!stderr.contains("panicked"), "{mode}: {stderr}");
            assert_eq!(std::fs::read(&binary).unwrap(), b"preserve existing output");
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
