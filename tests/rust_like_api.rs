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
    let (ok, stderr) = run_cli("substr_oob", "fn main() -> i64 { \"abc\"[1..4].len() }\n");
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

#[test]
fn let_types_are_inferred_from_later_use() {
    let r = run(r#"
        struct Bag { items: Vec<String> }
        fn total(v: Vec<i64>) -> i64 { v.sum() }
        fn main() -> i64 {
            let mut v = Vec::new();
            let before = v.len();              // used before the use that types it
            v.push(40);
            let mut names = Vec::new();
            let mut counts = HashMap::new();
            names.push("a");                   // a later `let` solved first
            counts.insert("a", 1);
            let mut best = None;
            for i in 0..3 { best = Some(i); }
            let empty = Vec::new();            // typed by the call it is passed to
            let items = Vec::new();            // typed by a struct field
            let bag = Bag { items: items };
            v[0] + before + names.len() + counts.get("a").unwrap() + best.unwrap() + total(empty) + bag.items.len() - 2
        }
    "#);
    assert_eq!(r, 42);
    let msg = compile_error("fn main() -> i64 { let v = Vec::new(); 0 }");
    assert!(msg.contains("cannot infer the type of `v`; add a type annotation"), "{msg}");
}

#[test]
fn for_loops_over_ranges_and_collections() {
    let r = run(r#"
        fn main() -> i64 {
            let mut s = 0;
            for i in 0..10 { s = s + i }                // tail without `;` sees this iteration's i
            for i in 0..10 { if i % 2 == 0 { continue; } s += 100; }
            for i in 3..=3 { s += i * 1000; }
            let mut n = 0;
            for _ in 9223372036854775806..=9223372036854775807 { n += 1; }   // no overflow past MAX
            for _ in 5..=4 { n += 100; }
            let mut v: Vec<i64> = Vec::new();
            v.push(7); v.push(8);
            for x in v { s += x * 10000; }
            let mut a: Array<String> = Array::new(2);
            a[0] = "ab"; a[1] = "c";
            for w in a { s += w.len() * 1000000; }
            s * 10 + n
        }
    "#);
    // 45 + 500 + 3000 + 150000 + 3000000 = 3153545
    assert_eq!(r, 31535452);
}

#[test]
fn if_let_and_while_let() {
    let r = run(r#"
        enum Shape { Circle(i64), Square(i64), Dot }
        fn area(s: Shape) -> i64 {
            if let Shape::Circle(r) = s { r * 3 } else if let Shape::Square(w) = s { w * w } else { 0 }
        }
        fn main() -> i64 {
            let mut stack: Vec<i64> = Vec::new();
            for i in 1..=5 { stack.push(i); }
            let mut total = 0;
            while let Some(top) = stack.pop() { total += top; }
            let none: Option<i64> = None;
            if let Some(x) = none { total += x * 1000; }
            total + area(Shape::Square(5)) + area(Shape::Circle(2)) + area(Shape::Dot)
        }
    "#);
    assert_eq!(r, 15 + 25 + 6);
}

#[test]
fn nested_tuple_and_struct_patterns() {
    let r = run(r#"
        struct Point { x: i64, y: i64 }
        enum Shape { Circle { r: i64 }, Rect(Point, Point), Empty }
        enum Tree { Leaf(i64), Node(Tree, Tree) }
        fn sum(t: Tree) -> i64 {
            match t {
                Tree::Node(Tree::Leaf(a), Tree::Leaf(b)) => a + b,
                Tree::Node(l, r) => sum(l) + sum(r),
                Tree::Leaf(v) => v,
            }
        }
        fn area(s: Shape) -> i64 {
            match s {
                Shape::Circle { r } => 3 * r * r,
                Shape::Rect(Point { x: x1, y: y1 }, Point { x: x2, .. }) => (x2 - x1) * y1,
                Shape::Empty => 0,
            }
        }
        fn classify(o: Option<Option<i64>>) -> i64 {
            match o {
                Some(Some(0)) => 1,
                Some(Some(n)) if n > 10 => 2,
                Some(Some(_)) => 3,
                Some(None) => 4,
                None => 5,
            }
        }
        fn main() -> i64 {
            let (a, (b, c)) = (1, (2, 3));
            let Point { x, y } = Point { x: 4, y: 5 };
            let t = Tree::Node(Tree::Node(Tree::Leaf(1), Tree::Leaf(2)), Tree::Leaf(3));
            let shapes = area(Shape::Rect(Point { x: 1, y: 2 }, Point { x: 4, y: 0 })) + area(Shape::Circle { r: 2 }) + area(Shape::Empty);
            let kinds = classify(Some(Some(0))) * 10000 + classify(Some(Some(50))) * 1000 + classify(Some(Some(5))) * 100
                + classify(Some(None)) * 10 + classify(None);
            (a + b + c + x + y) * 1000000000 + sum(t) * 10000000 + shapes * 100000 + kinds
        }
    "#);
    assert_eq!(r, 15 * 1000000000 + 6 * 10000000 + 18 * 100000 + 12345);
}

#[test]
fn pattern_exhaustiveness_and_refutability_are_checked() {
    // A refutable payload does not cover its variant.
    let msg = compile_error("enum E { A(i64), B } fn main() -> i64 { match E::A(1) { E::A(1) => 1, E::B => 2 } }");
    assert!(msg.contains("non-exhaustive match"), "{msg}");
    let msg = compile_error("fn main() -> i64 { match Some(Some(1)) { Some(Some(_)) => 1, None => 2 } }");
    assert!(msg.contains("non-exhaustive match"), "{msg}");
    // A guarded arm does not count toward coverage.
    let msg = compile_error("fn main() -> i64 { let o: Option<i64> = None; match o { Some(n) if n > 0 => 1, None => 2 } }");
    assert!(msg.contains("non-exhaustive match"), "{msg}");
    let msg = compile_error("fn main() -> i64 { let o: Option<i64> = Some(1); let Some(x) = o; x }");
    assert!(msg.contains("refutable pattern in `let`"), "{msg}");
    let msg = compile_error("struct P { x: i64, y: i64 } fn main() -> i64 { let P { x } = P { x: 1, y: 2 }; x }");
    assert!(msg.contains("does not mention every field"), "{msg}");
    // Booleans have two constructors.
    assert_eq!(run("fn main() -> i64 { match (true, false) { (true, _) => 1, (false, true) => 2, (false, false) => 3 } }"), 1);
}

#[test]
fn format_vec_and_assert_macros() {
    let r = run(r#"
        struct P { x: i64 }
        impl ToString for P { fn to_string(self) -> String { format!("P({})", self.x) } }
        fn main() -> i64 {
            let v = vec![1, 2, 3];
            let names = vec!["a", "b"];
            let zeros = vec![0; 4];
            let empty: Vec<i64> = vec![];
            let who = "world";
            let s = format!("hello {who}, {} + {} = {}; {{x}} {0} {}", 1, 2, 3, P { x: 7 });
            assert!(v.len() == 3);
            assert_eq!(s, "hello world, 1 + 2 = 3; {x} 1 P(7)", "got {}", s);
            assert_ne!(v[0], v[1]);
            assert_eq!(names.join("|"), "a|b");
            v.sum() + zeros.len() + empty.len()
        }
    "#);
    assert_eq!(r, 10);
    for (src, want) in [
        (r#"format!("{:?}", 1)"#, "format spec `{:?}` is not supported"),
        (r#"format!("{} {}", 1)"#, "uses argument 1 but 1 argument(s) were given"),
        (r#"format!("{}", 1, 2)"#, "format argument 1 is never used"),
        (r#"format!("a } b")"#, "unmatched `}`"),
        (r#"nope!(1)"#, "unknown macro `nope!`"),
    ] {
        let msg = compile_error(&format!("fn main() -> i64 {{ let s = {}; 0 }}", src));
        assert!(msg.contains(want), "{src}: {msg}");
    }
    let (ok, stderr) = run_cli("assert_fail", "fn main() -> i64 { assert_eq!(1 + 1, 3, \"math is {}\", \"broken\"); 0 }\n");
    assert!(!ok);
    assert!(stderr.contains("math is broken"), "{stderr}");
}

#[test]
fn range_indexing_copies_part_of_a_string_or_vec() {
    let r = run(r#"
        fn main() -> i64 {
            let s = "hello world";
            let v = vec![1, 2, 3, 4, 5];
            let mut ok = 0;
            if s[0..5] == "hello" && s[6..] == "world" && s[..5] == "hello" && s[..] == s && s[4..=4] == "o" { ok += 1; }
            if v[1..3].sum() == 5 && v[3..].len() == 2 && v[..2] == vec![1, 2] && v[0..=4].len() == 5 && v[2..2].is_empty() { ok += 2; }
            let mut copy = v[0..2];
            copy.push(9);
            if v.len() == 5 && copy.len() == 3 { ok += 4; }      // a copy, not a view
            ok
        }
    "#);
    assert_eq!(r, 7);
    let (ok, stderr) = run_cli("vec_range_oob", "fn main() -> i64 { let v = vec![1, 2]; v[1..3].len() }\n");
    assert!(!ok);
    assert!(stderr.contains("range 1..3 is out of bounds for a Vec of length 2"), "{stderr}");
    let msg = compile_error("fn main() -> i64 { let a: Array<i64> = Array::new(3); a[0..1].len() }");
    assert!(msg.contains("range indexing is not supported on `Array`"), "{msg}");
}

#[test]
fn trait_impls_are_checked_and_default_methods_apply() {
    for (src, want) in [
        ("struct P { x: i64 } impl Ord for P { fn cmp(self, o: P) -> i64 { 0 } }", "it should return `Ordering`, found `i64`"),
        ("struct P { x: i64 } impl Eq for P { }", "`impl Eq for P` is missing method `eq`"),
        ("struct P { x: i64 } impl Eq for P { fn eq(self, o: P) -> bool { true } fn extra(self) -> i64 { 1 } }", "method `extra` is not a member of trait `Eq`"),
        ("struct P { x: i64 } impl Frobnicate for P { }", "unknown trait `Frobnicate`"),
        ("struct P { x: i64 } impl Index for P { fn index(self, i: i64) -> i64 { 0 } }", "trait `Index` takes 1 type argument(s)"),
        ("struct P { x: i64 } impl Eq for P { fn eq(mut self, o: P) -> bool { true } }", "takes `self`, not `mut self`"),
        ("struct P { x: i64 } impl Eq for P { fn eq(self, o: i64) -> bool { true } }", "parameter 1 should be `P`, found `i64`"),
    ] {
        let msg = compile_error(&format!("{src} fn main() -> i64 {{ 0 }}"));
        assert!(msg.contains(want), "{src}: {msg}");
    }
    let r = run(r#"
        trait Shape {
            fn area(self) -> i64;
            fn describe(self) -> String { format!("area {}", self.area()) }
            fn double(self) -> i64 { self.area() * 2 }
        }
        struct Square { side: i64 }
        struct Circle { r: i64 }
        impl Shape for Square { fn area(self) -> i64 { self.side * self.side } }
        impl Shape for Circle {
            fn area(self) -> i64 { 3 * self.r * self.r }
            fn describe(self) -> String { "a circle" }
        }
        fn main() -> i64 {
            let s = Square { side: 3 };
            let c = Circle { r: 1 };
            assert_eq!(s.describe(), "area 9");
            assert_eq!(c.describe(), "a circle");
            s.double() + c.double()
        }
    "#);
    assert_eq!(r, 24);
}

#[test]
fn self_tail_call_beside_a_value_return_in_nested_if() {
    // Tail-call elimination turns `find(..)` into a loop `continue`; the inner
    // `if` still yields an inline `Option` from its other branch.
    let r = run(r#"
        struct Pt { x: i64 }
        fn find(target: i64, i: i64) -> Option<Pt> {
            if i >= 50 { None }
            else { let p = Pt { x: i }; if p.x == target { Some(p) } else { find(target, i + 1) } }
        }
        fn main() -> i64 {
            let hit = match find(42, 0) { Some(p) => p.x, None => 0 };
            let miss = match find(99, 0) { Some(p) => p.x, None => 1 };
            hit + miss
        }
    "#);
    assert_eq!(r, 43);
}

#[test]
fn array_literals() {
    let r = run(r#"
        struct P { x: i64 }
        fn total(a: [i64; 3]) -> i64 { let mut s = 0; for x in a { s += x; } s }
        fn main() -> i64 {
            let a = [10, 20, 12];
            let mut b: [i64; 4] = [0; 4];
            b[2] = 5;
            let names = ["x", "yy"];
            let dynamic: Array<i64> = [1, 2, 3, 4, 5];
            let n = 3;
            let filled: Array<String> = ["z"; n];
            let ps = [P { x: 1 }, P { x: 2 }];
            total(a) + b[2] + b.len() + names[1].len() + dynamic.len() + filled.len() + ps[1].x
        }
    "#);
    assert_eq!(r, 63);
    for (src, want) in [
        ("let a: [i64; 2] = [1, 2, 3];", "expected an array of length 2, this literal has length 3"),
        ("let a = [];", "cannot infer the element type of an empty array literal"),
        ("let n = 2; let a = [0; n];", "must be an integer literal"),
        ("let a = [1, \"x\"];", "type mismatch: expected `i64`, found `String`"),
    ] {
        let msg = compile_error(&format!("fn main() -> i64 {{ {src} 0 }}"));
        assert!(msg.contains(want), "{src}: {msg}");
    }
}

#[test]
fn methods_on_none_take_the_type_from_their_arguments() {
    assert_eq!(run("fn main() -> i64 { None.unwrap_or(7) + Option::None.unwrap_or(30) + Some(5).unwrap_or(0) }"), 42);
}
