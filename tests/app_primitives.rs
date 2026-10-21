use gcrust::{
    codegen::jit_run_i64_gc, compile::parse_with_prelude, lower::lower_program,
    resolve::resolve_module,
};
fn run(src: &str) -> i64 {
    let (m, _) = parse_with_prelude(src).unwrap();
    let r = resolve_module(m).unwrap();
    let p = lower_program(&r.globals).unwrap();
    unsafe { jit_run_i64_gc(&p, true) }.unwrap()
}
#[test]
fn generic_match_branches_receive_context_for_both_lowering_paths() {
    assert_eq!(
        run(r#"
        fn direct(n: Option<i64>) -> Result<Option<i64>, String> {
            match n { Option::Some(x) => Result::Ok(Option::Some(x)), Option::None => Result::Ok(Option::None) }
        }
        fn guarded(n: i64) -> Result<Option<i64>, String> {
            match n { x if x > 0 => Result::Ok(Option::Some(x)), _ => Result::Ok(Option::None) }
        }
        fn diverging(n: i64) -> Result<i64, String> {
            match n { 0 => { return Result::Err("zero"); }, _ => Result::Ok(n) }
        }
        fn main() -> i64 {
            let a = direct(Option::Some(3)).expect("direct");
            let b = guarded(4).expect("guard");
            let c: Result<Option<i64>, String> = match 0 { 0 => Result::Ok(Option::None), _ => Result::Err("unexpected") };
            a.expect("a") + b.expect("b") + diverging(2).expect("diverging") + if c.expect("c").is_none() { 1 } else { 0 }
        }
    "#),
        10
    );
}
#[test]
fn mutable_enum_payloads_work_for_reference_value_and_guarded_matches() {
    assert_eq!(
        run(r#"
        struct Boxed { value: i64 }
        fn main() -> i64 {
            let a: Result<Boxed,String> = Result::Ok(Boxed { value: 1 });
            let b: Option<i64> = Option::Some(2);
            let x = match a { Result::Ok(mut v) => { v.value = 10; v = Boxed { value: v.value + 1 }; v.value }, Result::Err(_) => 0 };
            let y = match b { Option::Some(mut v) => { v = v + 20; v }, Option::None => 0 };
            let z = match b { Option::Some(mut v) if v > 0 => { v = v + 30; v }, _ => 0 };
            x + y + z
        }
    "#),
        65
    );
}
#[test]
fn match_context_rejects_incompatible_branches_and_immutable_payloads() {
    for source in [
        "fn f(n:i64)->Result<i64,String>{match n{0=>Result::Ok(1),_=>Result::Ok(true)}} fn main()->i64{f(0).unwrap_or(0)}",
        "fn main()->i64 { match Option::Some(1) { Option::Some(v)=>{v=2;v}, Option::None=>0 } }",
    ] {
        let (m, _) = parse_with_prelude(source).unwrap();
        let r = resolve_module(m).unwrap();
        assert!(lower_program(&r.globals).is_err());
    }
}
#[test]
fn decimal_parser_preserves_full_i64_range_and_rejects_wrapping() {
    assert_eq!(
        run(r#"
        fn main()->i64 {
            let max = "9223372036854775807".parse_i64().expect("max");
            let min = "-9223372036854775808".parse_i64().expect("min");
            let mut cases: Vec<String> = Vec::new();
            cases.push("9223372036854775808");
            cases.push("-9223372036854775809");
            cases.push("18446744073709551616");
            cases.push("999999999999999999999999999999999");
            cases.push("");
            cases.push("-");
            cases.push("+");
            cases.push("+-1");
            cases.push("1x");
            cases.push(" 1");
            cases.push("１２");

            let mut i = 0;
            while i < cases.len() { if cases[i].parse_i64().is_some() { return 0; } i = i + 1; }
            if max + min == 0 - 1 && "-0000".parse_i64().expect("zero") == 0 && "0000123".parse_i64().expect("leading") == 123 && "+1".parse_i64() == Some(1) { 1 } else { 0 }
        }
    "#),
        1
    );
}
#[test]
fn csv_roundtrips_quoted_unicode_empty_and_multiline_fields_and_enforces_limits() {
    assert_eq!(
        run(r#"
        fn main()->i64 {
            let mut row: Vec<String> = Vec::new();
            row.push("\u{feff}first");
            let mut fields: Vec<String> = Vec::new();
            fields.push("");
            fields.push("λ");
            fields.push("a,b");
            fields.push("a\"b");
            fields.push("line\r\nbreak");
            fields.push("\u{feff}inside");

            let mut i = 0;
            while i < fields.len() { row.push(fields[i]); i = i + 1; }
            let mut rows: Vec<Vec<String>> = Vec::new();
            rows.push(row);
            let mut blank: Vec<String> = Vec::new();
            blank.push("");
            rows.push(blank);
            let encoded = csv_encode(rows).expect("encode");
            let decoded = csv_parse(encoded,2,7).expect("parse");
            if decoded.len() != 2 || decoded[1].len() != 1 { return 0; }
            if decoded[0][0] != "\u{feff}first" { return 0; }
            i = 0;
            while i < fields.len() { if decoded[0][i+1] != fields[i] { return 0; } i = i + 1; }
            let mut bad: Vec<String> = Vec::new();
            bad.push("a\"b");
            bad.push("\"unclosed");
            bad.push("\"a\"suffix");
            bad.push("a,b,c");

            i = 0;
            while i < bad.len() { if csv_parse(bad[i],2,2).is_ok() { return 0; } i = i + 1; }
            if csv_parse("a\nb\n",1,1).is_ok() || csv_parse("",0,1).is_ok() { return 0; }
            let empty: Vec<String> = Vec::new();
            let mut invalid: Vec<Vec<String>> = Vec::new();
            invalid.push(empty);
            if csv_encode(invalid).is_ok() { return 0; }
            let bom = csv_parse("\u{feff}a,b\rc,d\ne,f\r\n",3,2).expect("endings");
            if bom.len() == 3 && bom[0][0] == "a" { 1 } else { 0 }
        }
    "#),
        1
    );
}
#[test]
fn heap_orders_inline_and_reference_elements_across_growth_and_reuse() {
    assert_eq!(
        run(r#"
        #[value] struct Inline { key: i64, label: String }
        struct Item { key: i64, label: String }
        impl Ord for Inline { fn cmp(self, other:Inline)->Ordering { self.key.cmp(other.key) } }
        impl Ord for Item { fn cmp(self, other:Item)->Ordering { self.key.cmp(other.key) } }
        fn main()->i64 {
            let mut a: MinHeap<Inline> = MinHeap::new();
            let mut b: MinHeap<Item> = MinHeap::new();
            let mut i = 0;
            while i < 200 { let key = (i * 73) % 200; let label = key.to_string(); a.push(Inline { key:key,label:label }); b.push(Item { key:key,label:label }); i = i + 1; }
            i = 0;
            while i < 200 {
                let x = a.pop().expect("inline"); let y = b.pop().expect("ref");
                if x.key != i || y.key != i || (x.label == i.to_string()) == false || (y.label == i.to_string()) == false { return 0; }
                i = i + 1;
            }
            if a.pop().is_some() || b.peek().is_some() { return 0; }
            a.push(Inline { key:9,label:"again" });
            if a.pop().expect("reuse").key == 9 { 1 } else { 0 }
        }
    "#),
        1
    );
}
#[test]
fn nested_enum_payloads_trace_references_and_preserve_mixed_field_offsets() {
    assert_eq!(run(include_str!("fixtures/app_payloads.gcr")), 0);
}
