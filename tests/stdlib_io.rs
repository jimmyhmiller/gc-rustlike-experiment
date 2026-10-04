use gcrust::codegen::jit_run_i64_gc;
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;

fn run(src: &str) -> i64 {
    let (m, _) = parse_with_prelude(src).unwrap();
    let r = resolve_module(m).unwrap();
    let p = lower_program(&r.globals).unwrap();
    jit_run_i64_gc(&p, true).unwrap()
}

#[test]
fn diverging_statement_blocks_unify_with_live_match_and_if_branches() {
    assert_eq!(
        run(r#"
        fn match_first(n: i64) -> i64 {
            let x = match Option::Some(n) { Option::Some(v) => { return v; }, Option::None => 99 };
            x + 1
        }
        fn scalar_match(n: i64) -> i64 { match n { 0 => { return 3; }, _ => 4 } }
        fn first_branch(b: bool) -> i64 { if b { return 5; } else { 6 } }
        fn both_branches(b: bool) -> i64 { if b { return 7; } else { return 8; } }
        fn main() -> i64 { match_first(2) + scalar_match(0) + scalar_match(1) + first_branch(true) + first_branch(false) + both_branches(false) }
    "#),
        28
    );
}

#[test]
fn checked_io_preserves_errors_and_unicode_across_collections() {
    let dir = std::env::temp_dir().join(format!("gcr-stdlib-io-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = serde_json::to_string(dir.join("text").to_str().unwrap()).unwrap();
    let missing = serde_json::to_string(dir.join("missing").to_str().unwrap()).unwrap();
    let source = format!(
        r#"
        fn main() -> i64 {{
            let mut score = 0;
            match fs_write_text_atomic({path}, "hello λ\n") {{ Result::Ok(n) => score = score + 1, Result::Err(e) => score = 0 }};
            match fs_read_text({path}, 100) {{ Result::Ok(s) => if str_eq(s, "hello λ\n") {{ score = score + 1; }}, Result::Err(e) => score = 0 }};
            match fs_read_text({missing}, 100) {{ Result::Err(e) => if e.kind == 1 {{ score = score + 1; }}, Result::Ok(s) => score = 0 }};
            match fs_read_text({path}, 2) {{ Result::Err(e) => if e.kind == 3 {{ score = score + 1; }}, Result::Ok(s) => score = 0 }};
            score
        }}
    "#
    );
    let result = run(&source);
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(result, 4);
}

#[test]
fn string_join_handles_empty_prefix_and_unicode_without_quadratic_growth() {
    assert_eq!(
        run(r#"
        fn main() -> i64 {
            let mut v: Vec<String> = vec_new();
            let mut i = 0;
            while i < 200 { v = vec_push(v, "λ"); i = i + 1; }
            let s = str_join(v, "|");
            let empty: Vec<String> = vec_new();
            if str_len(s) == 599 && str_eq(str_join(empty, "|"), "") && str_eq(str_join_array(v.data, 1, "|"), "λ") { 1 } else { 0 }
        }
    "#),
        1
    );
}

#[test]
fn literal_search_and_json_encoding_cover_byte_and_control_boundaries() {
    assert_eq!(
        run(r#"
        fn main() -> i64 {
            let a = str_find_from("λλxλ", "λ", 2);
            let b = str_find_from("abc", "", 3);
            let c = str_find_from("abc", "a", 4);
            let d = json_quote("λ\n\t\"\\");
            if a == 2 && b == 3 && c == 0 - 1 && str_eq(d, "\"λ\\u000a\\u0009\\\"\\\\\"") { 1 } else { 0 }
        }
    "#),
        1
    );
}

#[test]
fn malformed_host_response_shape_is_a_diagnostic() {
    let (m, _) = parse_with_prelude(
        r#"
        struct IoResponse { text: i64 }
        fn main() -> i64 { let r = host_io(0, "", "", 0); 0 }
    "#,
    )
    .unwrap();
    let r = resolve_module(m).unwrap();
    assert!(
        lower_program(&r.globals)
            .unwrap_err()
            .msg
            .contains("IoResponse")
    );
}

#[test]
fn bounded_string_search_clamps_ranges_and_preserves_absolute_offsets() {
    assert_eq!(run(r#"
        fn main() -> i64 {
            let s = "aλneedle\nneedle";
            if str_find_range(s, "needle", 0, 9) != 3 { return 1; }
            if str_find_range(s, "needle", 0, 8) != 0 - 1 { return 2; }
            if str_find_range(s, "needle", 10, 100) != 10 { return 3; }
            if str_find_range(s, "", 10, 9) != 0 - 1 { return 4; }
            if str_find_range(s, "", 100, 100) != 0 - 1 { return 5; }
            if str_find_range(s, "a", 0 - 1, 1) != 0 { return 6; }
            if str_find_range(s, "", 0, 0 - 1) != 0 { return 7; }
            0
        }
    "#), 0);
}

#[test]
fn embedded_jit_arguments_are_independent_and_inherited_by_threads() {
    use gcrust::codegen::{GcRunMode, jit_run_i64_with_args};
    let source = r#"
        fn score() -> i64 {
            let args = match process_args() { Result::Ok(v) => v, Result::Err(e) => return e.kind };
            if array_len(args) != 2 { return 99; }
            str_len(opt_expect(array_get(args, 1), "argument"))
        }
        fn main() -> i64 { let worker = Thread::spawn(score); score() + worker.join() }
    "#;
    let (m, _) = parse_with_prelude(source).unwrap();
    let r = resolve_module(m).unwrap();
    let p = lower_program(&r.globals).unwrap();
    for (arg, expected) in [("λ", 4), ("abc", 6), ("", 0)] {
        assert_eq!(jit_run_i64_with_args(&p, GcRunMode::Stress, vec!["embedded".into(), arg.into()]).unwrap(), expected);
    }
    #[cfg(unix)] {
        use std::os::unix::ffi::OsStringExt;
        assert_eq!(jit_run_i64_with_args(&p, GcRunMode::Stress, vec!["embedded".into(), std::ffi::OsString::from_vec(vec![0xff])]).unwrap(), 6);
    }
}
