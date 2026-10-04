use gcrust::codegen::{jit_run_i64_gc};
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;
mod support;
const SOURCE: &str = r#"
#[value]
struct Item { n: i64, label: String }
#[value]
struct Nested { head: i8, item: Item, tail: i64 }
#[value]
enum Envelope { Empty, Full(Nested), Text(String) }
fn keep(value: Option<Envelope>, trigger: String) -> Envelope { opt_expect(value, trigger) }
fn identity<T>(value: T) -> T { value }
fn main() -> i64 {
    let mut items: Vec<Item> = vec_new();
    let mut i = 0;
    while i < 11 { items = vec_push(items, Item { n: i, label: "λ" }); i = i + 1; }
    let active = AtomicI64::new(0);
    let peak = AtomicI64::new(0);
    let mapped = match vec_parallel_map(items, 3, |item: Item| {
        let count = active.fetch_add(1) + 1;
        let mut observed = peak.load();
        while count > observed && !peak.compare_and_set(observed, count) { observed = peak.load(); }
        Thread::sleep(2);
        let result = str_concat(to_string(item.n), item.label);
        active.fetch_add(0 - 1);
        result
    }) { Result::Ok(v) => v, Result::Err(_) => return 90 };
    let mut j = 0;
    while j < vec_len(mapped) {
        if !str_eq(vec_at(mapped, j), str_concat(to_string(j), "λ")) { return 91; }
        j = j + 1;
    }
    let envelopes = match vec_parallel_map(items, 2, |item: Item| { let nested = Nested { head: 7, item: item, tail: 99 }; Envelope::Full(nested) }) { Result::Ok(v) => v, Result::Err(_) => return 101 };
    let mut k = 0;
    while k < vec_len(envelopes) {
        match vec_at(envelopes, k) { Envelope::Full(v) => if v.head != 7 || v.item.n != k || !str_eq(v.item.label, "λ") || v.tail != 99 { return 102; }, _ => return 103 };
        k = k + 1;
    }
    if active.load() != 0 || peak.load() < 1 || peak.load() > 3 { return 96; }
    let nested = Nested { head: 7, item: Item { n: 42, label: "λ" }, tail: 99 };
    let envelope = keep(Option::Some(Envelope::Full(nested)), str_concat("trigger", "collection"));
    match envelope { Envelope::Full(v) => if v.head != 7 || v.item.n != 42 || !str_eq(v.item.label, "λ") || v.tail != 99 { return 97; }, _ => return 98 };
    let values: Vec<i64> = vec_push(vec_new(), 8);
    let identities = match vec_parallel_map(values, 1, identity) { Result::Ok(v) => v, Result::Err(_) => return 99 };
    if vec_at(identities, 0) != 8 { return 100; }
    let empty: Vec<i64> = vec_new();
    match vec_parallel_map(empty, 1, |n: i64| n) { Result::Ok(v) => if vec_len(v) != 0 { return 92; }, Result::Err(_) => return 93 };
    match vec_parallel_map(empty, 0, |n: i64| n) { Result::Err(_) => {}, Result::Ok(_) => return 94 };
    match vec_parallel_map(empty, 65, |n: i64| n) { Result::Err(_) => {}, Result::Ok(_) => return 95 };
    11
}
"#;
#[test]
fn ordered_parallel_map_jit_and_aot_under_gc_stress() {
    let (m, _) = parse_with_prelude(SOURCE).unwrap();
    let r = resolve_module(m).unwrap();
    let p = lower_program(&r.globals).unwrap();
    assert_eq!(jit_run_i64_gc(&p, true).unwrap(), 11);
    let path = std::env::temp_dir().join(format!("gcr-parallel-{}", std::process::id()));
    let lib = support::runtime_staticlib();
    let source = path.with_extension("gcr");
    std::fs::write(&source, SOURCE).unwrap();
    let mut build = std::process::Command::new(env!("CARGO_BIN_EXE_gcr"));
    build.arg("build").arg(&source).arg("-o").arg(&path).env("GCRUST_RUNTIME_LIB", lib);
    let result = support::run_with_timeout(&mut build, 60);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    std::fs::remove_file(source).unwrap();
    let mut cmd = std::process::Command::new(&path);
    cmd.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1");
    let out = support::run_with_timeout(&mut cmd, 30);
    std::fs::remove_file(path).unwrap();
    assert_eq!(out.status.code(), Some(11), "{}", String::from_utf8_lossy(&out.stderr));
}
