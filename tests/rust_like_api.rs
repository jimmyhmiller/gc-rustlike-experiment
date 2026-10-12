//! The Rust-style surface: operator and indexing traits, method-based prelude
//! collections, and the diagnostics that keep that surface unambiguous. Each
//! program ends in an `i64` encoding its checks; programs run under GC stress.

use std::process::Command;

use gcrust::codegen::jit_run_i64_gc;
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;

fn run(src: &str) -> i64 {
    let (module, _) = parse_with_prelude(src).expect("parse");
    let resolved = resolve_module(module).expect("resolve");
    let prog = lower_program(&resolved.globals).expect("lower");
    jit_run_i64_gc(&prog, true).expect("run")
}

/// The first compile-time error for `src`, from whichever phase reports it.
fn compile_error(src: &str) -> String {
    let module = match parse_with_prelude(src) {
        Ok((module, _)) => module,
        Err(e) => return e.msg,
    };
    let resolved = match resolve_module(module) {
        Ok(r) => r,
        Err(e) => return e.msg,
    };
    match lower_program(&resolved.globals) {
        Ok(_) => panic!("expected a compile error"),
        Err(e) => e.msg,
    }
}

/// Run `src` through the CLI and return (success, stderr).
fn run_cli(name: &str, src: &str) -> (bool, String) {
    let dir = std::env::temp_dir().join(format!("gcr_rust_like_{}_{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.gcr");
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gcr")).args(["run", path.to_str().unwrap()]).output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn string_operators_use_eq_ord_and_add() {
    let r = run(r#"
        fn main() -> i64 {
            let a = "ab" + "c";
            let mut score = 0;
            if a == "abc" { score = score + 1; }
            if a != "abd" { score = score + 2; }
            if "abc" < "abd" && "b" > "abc" && "abc" <= "abc" && "abd" >= "abc" { score = score + 4; }
            if "ab".cmp("ab") == Ordering::Equal { score = score + 8; }
            score
        }
    "#);
    assert_eq!(r, 15);
}

#[test]
fn user_types_get_operators_from_trait_impls() {
    let r = run(r#"
        struct V { x: i64, y: i64 }
        impl Add for V { fn add(self, o: V) -> V { V { x: self.x + o.x, y: self.y + o.y } } }
        impl Sub for V { fn sub(self, o: V) -> V { V { x: self.x - o.x, y: self.y - o.y } } }
        impl Eq for V { fn eq(self, o: V) -> bool { self.x == o.x && self.y == o.y } }
        impl Ord for V {
            fn cmp(self, o: V) -> Ordering {
                match self.x.cmp(o.x) { Ordering::Equal => self.y.cmp(o.y), order => order }
            }
        }
        fn main() -> i64 {
            let s = V { x: 1, y: 2 } + V { x: 10, y: 20 } - V { x: 1, y: 1 };
            let mut score = s.x * 100 + s.y;               // 10 * 100 + 21
            let expected = V { x: 10, y: 21 };
            let low = V { x: 1, y: 9 };
            let high = V { x: 2, y: 0 };
            if s == expected { score = score + 10000; }
            if low < high && !(high < low) { score = score + 20000; }
            score
        }
    "#);
    assert_eq!(r, 31021);
}

#[test]
fn operators_require_a_trait_impl_not_an_inherent_method() {
    let msg = compile_error(r#"
        struct V { x: i64 }
        impl V { fn add(self, o: V) -> V { V { x: self.x + o.x } } }
        fn main() -> i64 { let v = V { x: 1 } + V { x: 2 }; v.x }
    "#);
    assert!(msg.contains("`+` is not defined for `V`") && msg.contains("impl Add for V"), "{msg}");
    let msg = compile_error(r#"
        struct V { x: i64 }
        fn main() -> i64 { let a = V { x: 1 }; if a == a { 1 } else { 0 } }
    "#);
    assert!(msg.contains("`==` is not defined for `V`"), "{msg}");
}

#[test]
fn indexing_dispatches_to_index_and_index_set() {
    let r = run(r#"
        struct Grid { w: i64, cells: Array<i64> }
        impl Index<i64> for Grid { fn index(self, i: i64) -> i64 { self.cells[i] } }
        impl IndexSet<i64> for Grid { fn index_set(mut self, i: i64, v: i64) { self.cells[i] = v * 10; } }
        fn main() -> i64 {
            let mut g = Grid { w: 2, cells: Array::new(4) };
            g[3] = 4;
            let mut v: Vec<i64> = Vec::new();
            v.push(1);
            v.push(2);
            v[1] = v[1] + g[3];
            v[1]
        }
    "#);
    assert_eq!(r, 42);
    let msg = compile_error(r#"
        struct P { x: i64 }
        fn main() -> i64 { let p = P { x: 1 }; p[0] }
    "#);
    assert!(msg.contains("indexing is not defined for `P`"), "{msg}");
}

#[test]
fn vec_index_out_of_bounds_aborts_with_rust_message() {
    let (ok, stderr) = run_cli("vec_oob", "fn main() -> i64 { let mut v: Vec<i64> = Vec::new(); v.push(1); v[1] }\n");
    assert!(!ok);
    assert!(stderr.contains("index out of bounds: the len is 1 but the index is 1"), "{stderr}");
}

#[test]
fn string_byte_access_aborts_out_of_range() {
    let (ok, stderr) = run_cli("str_oob", "fn main() -> i64 { \"abc\".byte_at(3) }\n");
    assert!(!ok);
    assert!(stderr.contains("byte index 3 is out of bounds of a string of length 3"), "{stderr}");
    let (ok, stderr) = run_cli("substr_oob", "fn main() -> i64 { \"abc\".substring(1, 4).len() }\n");
    assert!(!ok);
    assert!(stderr.contains("byte range 1..4 is out of bounds"), "{stderr}");
}

#[test]
fn duplicate_methods_and_unknown_attributes_are_errors() {
    let msg = compile_error(r#"
        struct P { x: i64 }
        impl P { fn get(self) -> i64 { 1 } }
        impl P { fn get(self) -> i64 { 2 } }
        fn main() -> i64 { P { x: 0 }.get() }
    "#);
    assert!(msg.contains("duplicate method `get` on `P`"), "{msg}");
    let msg = compile_error("#[derive(Clone)]\nstruct P { x: i64 }\nfn main() -> i64 { 0 }");
    assert!(msg.contains("unknown attribute `#[derive]`"), "{msg}");
    let msg = compile_error("#[value]\nfn main() -> i64 { 0 }");
    assert!(msg.contains("`#[value]` applies only to a struct or enum"), "{msg}");
}

#[test]
fn shadowing_a_prelude_type_drops_its_prelude_impls() {
    // The user's own Option has its own `unwrap_or`; without dropping the
    // prelude's impls this would be a duplicate method.
    let r = run(r#"
        enum Option<T> { None, Some(T) }
        impl<T> Option<T> {
            fn unwrap_or(self, d: T) -> T { match self { Option::Some(x) => x + 1, Option::None => d } }
        }
        fn main() -> i64 { Option::Some(41).unwrap_or(0) }
    "#);
    assert_eq!(r, 42);
}

#[test]
fn mut_self_methods_mutate_in_place_and_need_a_mut_root() {
    let r = run(r#"
        struct Bag { items: Vec<i64> }
        fn main() -> i64 {
            let mut v: Vec<i64> = Vec::new();
            let alias = v;
            v.push(5);
            let mut copy = v.clone();
            copy.push(6);
            let mut bag = Bag { items: Vec::new() };
            bag.items.push(7);
            alias.len() * 100 + copy.len() * 10 + bag.items[0]
        }
    "#);
    // The alias sees the push (one element); the clone is independent (two).
    assert_eq!(r, 127);
    let msg = compile_error(r#"
        fn main() -> i64 { let v: Vec<i64> = Vec::new(); v.push(1); v.len() }
    "#);
    assert!(msg.contains("immutable binding `v`"), "{msg}");
}

#[test]
fn vec_methods() {
    let r = run(r#"
        struct P { name: String, rank: i64 }
        fn main() -> i64 {
            let mut v: Vec<i64> = Vec::new();
            for i in 0..20 { v.push((i * 7) % 20); }
            v.sort();
            let mut ok = 0;
            if v[0] == 0 && v[19] == 19 && v.contains(13) { ok = ok + 1; }
            if v.pop() == Some(19) && v.len() == 19 && v.last() == Some(18) { ok = ok + 2; }
            v.truncate(3);
            v.reverse();
            if v.len() == 3 && v[0] == 2 && v.get(3).is_none() { ok = ok + 4; }
            // sort_by is stable: equal ranks keep their insertion order.
            let mut ps: Vec<P> = Vec::new();
            ps.push(P { name: "b", rank: 2 });
            ps.push(P { name: "a", rank: 1 });
            ps.push(P { name: "c", rank: 2 });
            ps.sort_by(|x: P, y: P| x.rank.cmp(y.rank));
            if ps[0].name == "a" && ps[1].name == "b" && ps[2].name == "c" { ok = ok + 8; }
            let w = v.map(|x: i64| x * 2).filter(|x: i64| x > 0);
            if w.len() == 2 && w.position(|x: i64| x == 2) == Some(1) && w.find(|x: i64| x > 100).is_none() { ok = ok + 16; }
            let mut names: Vec<String> = Vec::new();
            names.push("x"); names.push("y");
            if names.join("-") == "x-y" && names == names.clone() { ok = ok + 32; }
            ok
        }
    "#);
    assert_eq!(r, 63);
}

#[test]
fn hashmap_methods() {
    let r = run(r#"
        fn main() -> i64 {
            let mut m: HashMap<String, i64> = HashMap::new();
            let mut ok = 0;
            if m.insert("a", 1).is_none() && m.insert("a", 2) == Some(1) { ok = ok + 1; }
            for i in 0..50 { m.insert(i.to_string(), i); }
            let copy = m.clone();
            m.insert("only-in-m", 0);
            if m.len() == 52 && copy.len() == 51 && !copy.contains_key("only-in-m") { ok = ok + 2; }
            if m.get("49") == Some(49) && m.get("a") == Some(2) && m.get("zz").is_none() { ok = ok + 4; }
            if m.keys().len() == 52 && m.values().len() == 52 { ok = ok + 8; }
            ok
        }
    "#);
    assert_eq!(r, 15);
}

#[test]
fn string_methods() {
    let r = run(r#"
        fn main() -> i64 {
            let mut ok = 0;
            let s = "  one,two,,three \t";
            let parts = s.trim().split(",");
            if parts.len() == 4 && parts[2] == "" && parts[3] == "three" { ok = ok + 1; }
            if s.find("two") == Some(6) && s.find("four").is_none() && s.contains(",,") { ok = ok + 2; }
            if "a-b-c".replace("-", "+") == "a+b+c" && "ab".repeat(3) == "ababab" { ok = ok + 4; }
            if "Héllo".to_uppercase() == "HéLLO" && "Héllo".chars().len() == 5 { ok = ok + 8; }
            if "12".parse_i64() == Some(12) && "1e3".parse_f64() == Some(1000.0) && "1e".parse_f64().is_none() && "".parse_f64().is_none() { ok = ok + 16; }
            if "abc".starts_with("ab") && "abc".ends_with("bc") && !"abc".ends_with("abcd") { ok = ok + 32; }
            if 'x'.to_string() == "x" && true.to_string() == "true" && (0 - 3).abs() == 3 { ok = ok + 64; }
            ok
        }
    "#);
    assert_eq!(r, 127);
}

#[test]
fn option_and_result_methods() {
    let r = run(r#"
        fn half(n: i64) -> Result<i64, String> { if n % 2 == 0 { Result::Ok(n / 2) } else { Result::Err("odd") } }
        fn main() -> i64 {
            let mut ok = 0;
            let some: Option<i64> = Some(4);
            if some.map(|x: i64| x + 1) == Some(5) && some.ok_or("none").is_ok() { ok = ok + 1; }
            if half(3).is_err() && half(3).unwrap_or(7) == 7 && half(8).unwrap() == 4 { ok = ok + 2; }
            if half(3).err() == Some("odd") && half(4).ok() == Some(2) { ok = ok + 4; }
            if half(4).map_err(|e: String| e.len()).unwrap() == 2 { ok = ok + 8; }
            ok
        }
    "#);
    assert_eq!(r, 15);
}

#[test]
fn compound_assignment_uses_operator_traits() {
    let r = run(r#"
        struct Acc { text: String }
        struct V { x: i64 }
        impl Add for V { fn add(self, o: V) -> V { V { x: self.x + o.x } } }
        fn main() -> i64 {
            let mut s = "ab";
            s += "cd";
            let mut acc = Acc { text: "x" };
            acc.text += "yz";
            let mut v = V { x: 1 };
            v += V { x: 41 };
            if s == "abcd" && acc.text == "xyz" { v.x } else { 0 }
        }
    "#);
    assert_eq!(r, 42);
}
