//! Reflection observes SC scalar/reference slots; aggregates have no i64 view.
use std::process::Command;
mod support;

#[test]
fn aggregate_fields_are_rejected_without_reading_their_storage() {
    let root = std::env::temp_dir().join(format!("gcr-reflection-contract-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for (i, fields, initializer) in [
        (0, "", ""),
        (1, "byte: i8", "byte: 7 as i8"),
        (2, "n: i64", "n: 42"),
        (3, "text: String, n: i64", "text: \"rooted\", n: 42"),
    ] {
        let source = root.join(format!("{i}.gcr"));
        let binary = root.join(format!("program-{i}"));
        std::fs::write(&source, format!("#[value] struct V {{ {fields} }} struct Holder {{ value: V }} fn main() -> i64 {{ let holder = Holder {{ value: V {{ {initializer} }} }}; field_i64(holder, 0) }}")).unwrap();
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
            let mut command = if native {
                Command::new(&binary)
            } else {
                let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                command.arg("run").arg(&source).arg("--jit");
                command
            };
            let output = support::run_with_timeout(
                command.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1"),
                30,
            );
            assert!(
                !output.status.success(),
                "aggregate {i} was treated as i64: {output:?}"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("requires a scalar or reference field"),
                "{output:?}"
            );
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reflected_scalars_preserve_width_sign_and_float_bits() {
    use gcrust::{
        codegen::jit_run_i64_gc, compile::parse_with_prelude, lower::lower_program,
        resolve::resolve_module,
    };
    let source = r#"
        struct Scalars { a: i8, b: u8, c: i16, d: u16, e: i32, f: u32, g: i64, h: u64, f32value: f32, f64value: f64, yes: bool, letter: char }
        fn main() -> i64 {
            let values = Scalars { a: -128, b: 255, c: -32768, d: 65535, e: -2147483648, f: 4294967295, g: -9223372036854775808, h: 18446744073709551615, f32value: -1.0, f64value: -2.25, yes: true, letter: 'λ' };
            if field_i64(values, 0) != -128 || field_i64(values, 1) != 255 { return 1; }
            if field_i64(values, 2) != -32768 || field_i64(values, 3) != 65535 { return 2; }
            if field_i64(values, 4) != -2147483648 || field_i64(values, 5) != 4294967295 { return 3; }
            if field_i64(values, 6) != -9223372036854775808 || field_i64(values, 7) != -1 { return 4; }
            if field_i64(values, 8) != 3212836864 || field_i64(values, 9) != float_bits(-2.25) { return 5; }
            if field_i64(values, 10) != 1 || field_i64(values, 11) != 955 { return 6; }
            42
        }
    "#;
    let resolved = resolve_module(parse_with_prelude(source).unwrap().0).unwrap();
    let program = lower_program(&resolved.globals).unwrap();
    assert_eq!(unsafe { jit_run_i64_gc(&program, true) }.unwrap(), 42);
}

#[test]
fn oversized_native_metadata_reports_a_build_error_and_preserves_output() {
    let root = std::env::temp_dir().join(format!("gcr-metadata-limit-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("main.gcr");
    let binary = root.join("program");
    let name = "f".repeat(65536);
    std::fs::write(&source, format!("struct H {{ {name}: i64 }} fn main() -> i64 {{ let h = H {{ {name}: 42 }}; field_i64(h, 0) }}")).unwrap();
    let original = b"existing executable is preserved";
    std::fs::write(&binary, original).unwrap();
    let output = support::run_with_timeout(
        Command::new(env!("CARGO_BIN_EXE_gcr"))
            .arg("build")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()),
        60,
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("reflection name byte count 65536 exceeds format limit 65535"),
        "{error}"
    );
    assert!(!error.contains("panicked"), "{error}");
    assert_eq!(std::fs::read(&binary).unwrap(), original);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_field_indexes_use_language_diagnostics_without_rust_unwinding() {
    let root = std::env::temp_dir().join(format!("gcr-reflection-indexes-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for operation in ["field_kind", "field_i64", "field_name"] {
        for index in [-1, 1] {
            let source = root.join(format!("{operation}-{index}.gcr"));
            let binary = root.join(format!("{operation}-{index}"));
            std::fs::write(&source, format!("struct H {{ n: i64 }} fn main() -> i64 {{ let h = H {{ n: 42 }}; {operation}(h, {index}); 0 }}")).unwrap();
            let output = support::run_with_timeout(
                Command::new(env!("CARGO_BIN_EXE_gcr"))
                    .arg("build")
                    .arg(&source)
                    .arg("-o")
                    .arg(&binary)
                    .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()),
                60,
            );
            assert!(output.status.success(), "{output:?}");
            for native in [false, true] {
                let mut command = if native {
                    Command::new(&binary)
                } else {
                    let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                    command.arg("run").arg(&source).arg("--jit");
                    command
                };
                let output = support::run_with_timeout(
                    command.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1"),
                    30,
                );
                assert!(!output.status.success(), "{output:?}");
                let diagnostic = String::from_utf8_lossy(&output.stderr);
                assert!(
                    diagnostic.contains(&format!(
                        "reflect::{operation}: field index {index} out of range (0..1)"
                    )),
                    "{diagnostic}"
                );
                assert!(!diagnostic.contains("panicked"), "{diagnostic}");
                assert!(!diagnostic.contains("cannot unwind"), "{diagnostic}");
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
