use gcrust::codegen::jit_run_i64_gc;
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;
use std::process::Command;

fn run(src: &str) -> i64 {
    let (module, _) = parse_with_prelude(src).unwrap();
    let resolved = resolve_module(module).unwrap();
    let prog = lower_program(&resolved.globals).unwrap();
    jit_run_i64_gc(&prog, true).unwrap()
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
            let a = match checked_mul_i64(min, 0 - 1) { Option::None => 1, Option::Some(x) => 0 };
            let b = match checked_mul_i64(0 - 1, min) { Option::None => 1, Option::Some(x) => 0 };
            let c = match checked_mul_i64(min, 1) { Option::Some(x) => if x == min { 1 } else { 0 }, Option::None => 0 };
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
