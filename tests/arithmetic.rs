use gcrust::codegen::jit_run_i64_gc;
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;
use std::process::Command;

mod support;

fn native_output(src: &str, name: &str) -> std::process::Output {
    let root = std::env::temp_dir().join(format!("gcr-arithmetic-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("test.gcr");
    let binary = root.join("test");
    std::fs::write(&source, src).unwrap();
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
    let output = support::run_with_timeout(&mut Command::new(&binary), 30);
    std::fs::remove_dir_all(root).unwrap();
    output
}

#[test]
fn char_cast_trap_child() {
    if let Ok(value) = std::env::var("GCR_TEST_CHAR_CAST") {
        run(&format!(
            "fn convert(x: i64) -> char {{ x as char }} fn main() -> i64 {{ convert({value}) as i64 }}"
        ));
        panic!("invalid char cast did not abort");
    }
}

#[test]
fn char_casts_validate_before_narrowing_in_jit_and_native() {
    for (index, value) in ["0 - 1", "55296", "57343", "1114112", "4294967361"]
        .iter()
        .enumerate()
    {
        let output = support::run_with_timeout(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "char_cast_trap_child", "--nocapture"])
                .env("GCR_TEST_CHAR_CAST", value),
            30,
        );
        let source = format!(
            "fn convert(x: i64) -> char {{ x as char }} fn main() -> i64 {{ convert({value}) as i64 }}"
        );
        for output in [output, native_output(&source, &format!("char-{index}"))] {
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("invalid Unicode scalar value in char cast"),
                "{output:?}"
            );
        }
    }
    let source = "fn main() -> i64 { if (0 as char) as i64 == 0 && (55295 as char) as i64 == 55295 && (57344 as char) as i64 == 57344 && (1114111 as char) as i64 == 1114111 && ((65 as u8) as char) == 'A' { 42 } else { 1 } }";
    assert_eq!(run(source), 42);
    assert_eq!(native_output(source, "char-valid").status.code(), Some(42));
}

#[test]
fn invalid_casts_are_diagnosed_before_codegen() {
    for src in [
        "fn main() -> i64 { let p = 0 as RawPtr; 0 }",
        "extern \"C\" fn pointer() -> RawPtr; fn main() -> i64 { (unsafe { pointer() }) as i64 }",
        "fn main() -> i64 { \"text\" as i64 }",
        "#[value] struct V { n: i64 } fn main() -> i64 { V { n: 1 } as i64 }",
        "fn main() -> i64 { let b = 2 as bool; 0 }",
        "fn main() -> i64 { let c = 1.0 as char; 0 }",
        "fn main() -> i64 { let x = true as f64; 0 }",
    ] {
        let (module, _) = parse_with_prelude(src).unwrap();
        let resolved = resolve_module(module).unwrap();
        let error = lower_program(&resolved.globals)
            .err()
            .expect("invalid cast accepted");
        assert!(
            error.msg.contains("invalid cast from"),
            "{}: {src}",
            error.msg
        );
    }
    assert_eq!(
        run("fn main() -> i64 { (true as i64) + ('A' as i64) + ((258 as u8) as i64) }"),
        68
    );
}

#[test]
fn float_cast_ir_has_no_poison_producing_conversion() {
    // Execution alone cannot prove this: poison permits optimizers to discard
    // the checks themselves and return the apparent success value.
    let (module, _) = parse_with_prelude(include_str!("fixtures/float_casts.gcr")).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    for optimized in [false, true] {
        let ir = gcrust::codegen::emit_llvm_ir(&prog, optimized).unwrap();
        for line in ir.lines() {
            assert!(
                !line.contains(" = fptosi ") && !line.contains(" = fptoui "),
                "{line}"
            );
        }
        if !optimized {
            for float in ["f32", "f64"] {
                for integer in ["i8", "i16", "i32", "i64"] {
                    for intrinsic in ["fptosi", "fptoui"] {
                        assert!(ir.contains(&format!("llvm.{intrinsic}.sat.{integer}.{float}")));
                    }
                }
            }
        }
    }
}

#[test]
fn float_integer_casts_are_defined_for_all_widths_in_optimized_jit_and_native() {
    let src = include_str!("fixtures/float_casts.gcr");
    assert_eq!(run(src), 42);

    let output = native_output(src, "float-casts");
    assert_eq!(output.status.code(), Some(42), "{output:?}");
}

fn run(src: &str) -> i64 {
    let (module, _) = parse_with_prelude(src).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    unsafe { jit_run_i64_gc(&prog, true) }.unwrap()
}

#[test]
fn signed_division_overflow_wraps_and_remainder_is_zero_at_each_width() {
    for (ty, max) in [
        ("i8", "127"),
        ("i16", "32767"),
        ("i32", "2147483647"),
        ("i64", "9223372036854775807"),
    ] {
        let src = format!(
            r#"
            fn check(min: {ty}, neg: {ty}) -> i64 {{
                if min / neg == min && min % neg == 0 {{ 1 }} else {{ 0 }}
            }}
            fn main() -> i64 {{ check((0 - {max} - 1) as {ty}, (0 - 1) as {ty}) }}
        "#
        );
        assert_eq!(run(&src), 1, "{ty}");
    }
}

#[test]
fn shifts_mask_counts_at_each_signed_and_unsigned_width() {
    for (ty, bits) in [
        ("i8", 8),
        ("u8", 8),
        ("i16", 16),
        ("u16", 16),
        ("i32", 32),
        ("u32", 32),
        ("i64", 64),
        ("u64", 64),
    ] {
        let src = format!(
            r#"
            fn check(one: {ty}, count: {ty}) -> i64 {{
                let high = one << (({bits} - 1) as {ty});
                if one << count == one && high >> count == high && one << ((0 - 1) as {ty}) == high {{ 1 }} else {{ 0 }}
            }}
            fn main() -> i64 {{ check(1 as {ty}, {bits} as {ty}) }}
        "#
        );
        assert_eq!(run(&src), 1, "{ty}");
    }
}

#[test]
fn checked_multiply_rejects_minimum_times_minus_one_in_both_orders() {
    assert_eq!(
        run(r#"
        fn main() -> i64 {
            let min = 0 - 9223372036854775807 - 1;
            let a = match min.checked_mul(0 - 1) { Option::None => 1, Option::Some(x) => 0 };
            let b = match (0 - 1).checked_mul(min) { Option::None => 1, Option::Some(x) => 0 };
            let c = match min.checked_mul(1) { Option::Some(x) => if x == min { 1 } else { 0 }, Option::None => 0 };
            a + b + c
        }
    "#),
        3
    );
}

#[test]
fn arithmetic_trap_child() {
    if let Ok(operator) = std::env::var("GCR_TEST_ZERO_OPERATOR") {
        run(&format!(
            "fn divide(a: i64, b: i64) -> i64 {{ a {operator} b }} fn main() -> i64 {{ divide(42, 0) }}"
        ));
        panic!("zero divisor did not abort");
    }
}

#[test]
fn division_and_remainder_by_zero_abort_with_diagnostic() {
    for (operator, diagnostic) in [
        ("/", "integer division by zero"),
        ("%", "integer remainder by zero"),
    ] {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "arithmetic_trap_child", "--nocapture"])
            .env("GCR_TEST_ZERO_OPERATOR", operator)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(diagnostic),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn literal_ranges_and_scalar_operator_domains_are_checked() {
    for body in [
        "let x: i8 = 128; 0",
        "let x: i8 = -129; 0",
        "let x = 256u8; 0",
        "let x = 9223372036854775808; 0",
        "let x = -9223372036854775809; 0",
        "let x: f32 = 1e100; 0",
        "let x = 1e100f32; 0",
        "let x = 1e999; 0",
        "let x = -'A'; 0",
        "let x = !'A'; 0",
        "let x = !1.0; 0",
        "let mut x = 'A'; x += 'B'; 0",
        "let mut x = true; x += false; 0",
        "let x = 1.0 << 2.0; 0",
        "let mut x = 1.0; x %= 2.0; let y = x & 1.0; 0",
    ] {
        let source = format!("fn main() -> i64 {{ {body} }}");
        let (module, _) = parse_with_prelude(&source).unwrap();
        let resolved = resolve_module(module).unwrap();
        assert!(lower_program(&resolved.globals).is_err(), "{source}");
    }
    assert_eq!(
        run(
            "fn main() -> i64 { let x: i8 = -128; let y = -9223372036854775808; let z: u64 = 18446744073709551615; if x == -128i8 && y == -9223372036854775808i64 && z == 18446744073709551615u64 { 42 } else { 1 } }"
        ),
        42
    );
    assert_eq!(
        run(
            "fn main() -> i64 { let mut p: RawPtr; p = unsafe { atomic_i64_new(42) }; if p == p { unsafe { atomic_i64_load(p) } } else { 1 } }"
        ),
        42
    );
}

#[test]
fn numeric_literal_suffixes_cannot_create_invalid_core_representations() {
    for source in ["0b1f32", "0o7f64", "1.0i8", "1e2u64"] {
        assert!(gcrust::lexer::lex(source).is_err(), "{source}");
    }
}

#[test]
fn floating_exceptional_values_use_ieee_comparisons_and_signed_zero() {
    let source = r#"
        fn f32check(zero: f32) -> bool {
            let nan = zero / zero;
            let inf = (1.0 as f32) / zero;
            nan != nan && nan != (1.0 as f32) && !(nan == nan) &&
            !(nan < inf) && !(nan <= inf) && !(nan > inf) && !(nan >= inf) &&
            (zero - zero) == (zero * (-1.0 as f32)) &&
            inf > (1.0 as f32) && zero - inf < zero &&
            inf % (1.0 as f32) != inf % (1.0 as f32)
        }
        fn f64check(zero: f64) -> bool {
            let nan = zero / zero;
            let inf = 1.0 / zero;
            nan != nan && nan != 1.0 && !(nan == nan) &&
            !(nan < inf) && !(nan <= inf) && !(nan > inf) && !(nan >= inf) &&
            (zero - zero) == (zero * -1.0) &&
            float_bits(zero * -1.0) == -9223372036854775808 &&
            inf > 1.0 && zero - inf < zero &&
            inf % 1.0 != inf % 1.0
        }
        fn main() -> i64 { if f32check(0.0 as f32) && f64check(0.0) { 42 } else { 1 } }
    "#;
    assert_eq!(run(source), 42);
    assert_eq!(
        native_output(source, "float-exceptions").status.code(),
        Some(42)
    );
}

#[test]
fn propagated_negative_constants_preserve_each_signed_width() {
    for (ty, minimum) in [("i8", "-128"), ("i16", "-32768"), ("i32", "-2147483648"), ("i64", "-9223372036854775808")] {
        let source = format!(r#"
            fn identity(x: {ty}) -> {ty} {{ x }}
            fn main() -> i64 {{
                let minimum: {ty} = {minimum};
                let negative: {ty} = -1;
                let zero: {ty} = 0;
                if minimum != {minimum}{ty} {{ return 1; }}
                if negative != -1{ty} {{ return 2; }}
                if minimum >= zero || negative >= zero {{ return 3; }}
                if identity(minimum) != minimum {{ return 4; }}
                if identity(negative) != negative {{ return 5; }}
                42
            }}
        "#);
        assert_eq!(run(&source), 42, "{ty}");
        assert_eq!(native_output(&source, &format!("propagated-{ty}")).status.code(), Some(42), "{ty}");
    }
}
