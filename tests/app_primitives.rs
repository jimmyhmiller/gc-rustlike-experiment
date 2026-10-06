use gcrust::{
    codegen::jit_run_i64_gc, compile::parse_with_prelude, lower::lower_program,
    resolve::resolve_module,
};
fn run(src: &str) -> i64 {
    let src = format!(
        "fn res_expect<T,E>(r:Result<T,E>,message:String)->T {{ match r {{ Result::Ok(x)=>x,Result::Err(_)=>panic(message) }} }} fn opt_is_none<T>(o:Option<T>)->bool {{ opt_is_some(o) == false }}\n{src}"
    );
    let (m, _) = parse_with_prelude(&src).unwrap();
    let r = resolve_module(m).unwrap();
    let p = lower_program(&r.globals).unwrap();
    jit_run_i64_gc(&p, true).unwrap()
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
            let a = res_expect(direct(Option::Some(3)), "direct");
            let b = res_expect(guarded(4), "guard");
            let c: Result<Option<i64>, String> = match 0 { 0 => Result::Ok(Option::None), _ => Result::Err("unexpected") };
            opt_expect(a, "a") + opt_expect(b, "b") + res_expect(diverging(2), "diverging") + if opt_is_none(res_expect(c,"c")) { 1 } else { 0 }
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
        "fn f(n:i64)->Result<i64,String>{match n{0=>Result::Ok(1),_=>Result::Ok(true)}} fn main()->i64{result_unwrap_or(f(0),0)}",
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
            let max = opt_expect(parse_int("9223372036854775807"),"max");
            let min = opt_expect(parse_int("-9223372036854775808"),"min");
            let mut cases: Vec<String> = vec_new();
            cases = vec_push(cases,"9223372036854775808");
            cases = vec_push(cases,"-9223372036854775809");
            cases = vec_push(cases,"18446744073709551616");
            cases = vec_push(cases,"999999999999999999999999999999999");
            cases = vec_push(cases,"");
            cases = vec_push(cases,"-");
            cases = vec_push(cases,"+1");
            cases = vec_push(cases,"1x");
            cases = vec_push(cases," 1");
            cases = vec_push(cases,"１２");

            let mut i = 0;
            while i < vec_len(cases) { if opt_is_none(parse_int(vec_at(cases,i))) == false { return 0; } i = i + 1; }
            if max + min == 0 - 1 && opt_expect(parse_int("-0000"),"zero") == 0 && opt_expect(parse_int("0000123"),"leading") == 123 { 1 } else { 0 }
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
            let mut row: Vec<String> = vec_new();
            row = vec_push(row,"\u{feff}first");
            let mut fields: Vec<String> = vec_new();
            fields = vec_push(fields,"");
            fields = vec_push(fields,"λ");
            fields = vec_push(fields,"a,b");
            fields = vec_push(fields,"a\"b");
            fields = vec_push(fields,"line\r\nbreak");
            fields = vec_push(fields,"\u{feff}inside");

            let mut i = 0;
            while i < vec_len(fields) { row = vec_push(row,vec_at(fields,i)); i = i + 1; }
            let mut rows: Vec<Vec<String>> = vec_new();
            rows = vec_push(rows,row);
            let mut blank: Vec<String> = vec_new();
            blank = vec_push(blank, "");
            rows = vec_push(rows,blank);
            let encoded = res_expect(csv_encode(rows),"encode");
            let decoded = res_expect(csv_parse(encoded,2,7),"parse");
            if vec_len(decoded) != 2 || vec_len(vec_at(decoded,1)) != 1 { return 0; }
            if str_eq(vec_at(vec_at(decoded,0),0),"\u{feff}first") == false { return 0; }
            i = 0;
            while i < vec_len(fields) { if str_eq(vec_at(vec_at(decoded,0),i+1),vec_at(fields,i)) == false { return 0; } i = i + 1; }
            let mut bad: Vec<String> = vec_new();
            bad = vec_push(bad,"a\"b");
            bad = vec_push(bad,"\"unclosed");
            bad = vec_push(bad,"\"a\"suffix");
            bad = vec_push(bad,"a,b,c");

            i = 0;
            while i < vec_len(bad) { if result_is_ok(csv_parse(vec_at(bad,i),2,2)) { return 0; } i = i + 1; }
            if result_is_ok(csv_parse("a\nb\n",1,1)) || result_is_ok(csv_parse("",0,1)) { return 0; }
            let empty: Vec<String> = vec_new();
            let mut invalid: Vec<Vec<String>> = vec_new();
            invalid = vec_push(invalid,empty);
            if result_is_ok(csv_encode(invalid)) { return 0; }
            let bom = res_expect(csv_parse("\u{feff}a,b\rc,d\ne,f\r\n",3,2),"endings");
            if vec_len(bom) == 3 && str_eq(vec_at(vec_at(bom,0),0),"a") { 1 } else { 0 }
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
        impl Ord for Inline { fn cmp(self, other:Inline)->i64 { self.key - other.key } }
        impl Ord for Item { fn cmp(self, other:Item)->i64 { self.key - other.key } }
        fn main()->i64 {
            let mut a: MinHeap<Inline> = heap_new();
            let mut b: MinHeap<Item> = heap_new();
            let mut i = 0;
            while i < 200 { let key = (i * 73) % 200; let label = to_string(key); heap_push(a,Inline { key:key,label:label }); heap_push(b,Item { key:key,label:label }); i = i + 1; }
            i = 0;
            while i < 200 {
                let x = opt_expect(heap_pop(a),"inline"); let y = opt_expect(heap_pop(b),"ref");
                if x.key != i || y.key != i || str_eq(x.label,to_string(i)) == false || str_eq(y.label,to_string(i)) == false { return 0; }
                i = i + 1;
            }
            if opt_is_none(heap_pop(a)) == false || opt_is_none(heap_peek(b)) == false { return 0; }
            heap_push(a,Inline { key:9,label:"again" });
            if opt_expect(heap_pop(a),"reuse").key == 9 { 1 } else { 0 }
        }
    "#),
        1
    );
}
#[test]
fn nested_enum_payloads_trace_references_and_preserve_mixed_field_offsets() {
    assert_eq!(run(include_str!("fixtures/app_payloads.gcr")), 0);
}
