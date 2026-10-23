//! Features added while porting funct (a real Rust crate): std APIs Rust code
//! reaches for, inference that partially-typed expressions need, path and
//! `use` resolution across modules, mutation through `&mut`-style accessors,
//! serde-style derives, `gcr test`, and the optimizer bugs the port exposed.
//! Each program checks itself with `assert_eq!`; programs run under GC stress.

use gcrust::codegen::jit_run_i64_gc;
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;

fn run(src: &str) -> i64 {
    let (module, _) = parse_with_prelude(src).expect("parse");
    let resolved = resolve_module(module).expect("resolve");
    let prog = lower_program(&resolved.globals).expect("lower");
    unsafe { jit_run_i64_gc(&prog, true) }.expect("run")
}

#[test]
fn std_any_downcasts_and_pointer_identity() {
    assert_eq!(run(r#"
        use std::any::Any;
        struct Player { hp: i64 }
        struct Other { s: String }
        fn main() -> i64 {
            let a: Box<dyn Any> = Box::new(Player { hp: 7 });
            let b: Box<dyn Any> = Box::new(5);
            assert!(a.is::<Player>());
            assert!(a.downcast_ref::<Other>().is_none());
            let mut p = a.downcast_mut::<Player>().unwrap();
            p.hp = 9;
            assert_eq!(a.downcast_ref::<Player>().unwrap().hp, 9);
            assert_eq!(b.downcast_ref::<i64>(), Some(5));
            assert_eq!(std::any::type_name::<Vec<i64>>(), "Vec<i64>");
            let x = Player { hp: 1 };
            let y = x;
            let z = Player { hp: 1 };
            assert!(std::ptr::eq(x, y));
            assert!(!std::ptr::eq(x, z));
            0
        }
    "#), 0);
}

#[test]
fn string_patterns_take_str_or_char() {
    assert_eq!(run(r#"
        fn main() -> i64 {
            let s = "a/b/c.ft".to_string();
            assert_eq!(s.find('/'), Some(1));
            assert_eq!(s.rfind("/"), Some(3));
            assert_eq!(s.split('/'), vec!["a", "b", "c.ft"]);
            assert_eq!(s.rsplit('/').next(), Some("c.ft".to_string()));
            assert_eq!(s.split_once('/'), Some(("a".to_string(), "b/c.ft".to_string())));
            assert_eq!(s.strip_suffix(".ft"), Some("a/b/c".to_string()));
            assert_eq!("//x//".trim_end_matches('/'), "//x");
            assert_eq!("//x//".trim_start_matches("/"), "x//");
            assert!(s.starts_with('a') && s.ends_with("ft") && s.contains('.'));
            let lines: Vec<String> = "one\ntwo\r\nthree".lines().collect();
            assert_eq!(lines, vec!["one", "two", "three"]);
            assert_eq!(String::new().len(), 0);
            0
        }
    "#), 0);
}

#[test]
fn paths_numbers_and_time() {
    assert_eq!(run(r#"
        use std::path::{Path, PathBuf};
        use std::time::{Duration, Instant};
        fn main() -> i64 {
            let p = Path::new("dir/sub/file.ft");
            assert_eq!(p.parent(), Some(PathBuf::from("dir/sub")));
            assert_eq!(p.extension().and_then(|e| e.to_str()), Some("ft".to_string()));
            assert_eq!(p.file_name().unwrap().to_str(), Some("file.ft".to_string()));
            assert_eq!(PathBuf::from("a").join("b").display().to_string(), "a/b");
            let big: i64 = 300;
            assert!(u8::try_from(big).is_err());
            let ok: Result<u32, TryFromIntError> = big.try_into();
            assert_eq!(ok.unwrap(), 300);
            assert_eq!(2i64.checked_pow(62), Some(4611686018427387904));
            assert_eq!(i64::MIN.checked_neg(), None);
            assert_eq!(7u128.div_ceil(2), 4);
            assert_eq!(2.5f64.round(), 3.0);
            assert_eq!((-3.0f64).powi(3), -27.0);
            assert!((1.0f64.atan2(1.0) - 0.7853981633974483).abs() < 1e-12);
            assert_eq!(char::from_u32(233), Some('é'));
            let start = Instant::now();
            std::thread::sleep(Duration::from_millis(2));
            assert!(start.elapsed() >= Duration::from_millis(1));
            assert_eq!(Duration::from_secs(2).as_millis(), 2000);
            0
        }
    "#), 0);
}

#[test]
fn atomic_ordering_renames_beside_cmp_ordering() {
    assert_eq!(run(r#"
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        fn main() -> i64 {
            let n = AtomicU64::new(1);
            n.fetch_add(2, Ordering::Relaxed);
            let flag = AtomicBool::new(false);
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(flag.load(Ordering::Acquire));
            assert_eq!(3.cmp(4), std::cmp::Ordering::Less);
            n.load(Ordering::Relaxed) as i64
        }
    "#), 3);
}

#[test]
fn partial_types_merge_across_arms_branches_and_closures() {
    assert_eq!(run(r#"
        struct Fault { msg: String }
        enum V { S(String), I(i64) }
        fn parse(s: String) -> Result<i64, String> { s.parse::<i64>().map_err(|_| format!("bad {}", s)) }
        fn all(xs: Vec<String>) -> Result<Vec<(String, i64)>, String> {
            xs.iter().map(|x| Ok((x.clone(), parse(x)?))).collect::<Result<_, String>>()
        }
        fn main() -> i64 {
            let v = V::I(3);
            let a = match v { V::S(s) => Ok(s), V::I(_) => Err(Fault { msg: "no".to_string() }) };
            let b = if true { Ok(1) } else { Err("x".to_string()) };
            let c = match v { V::S(s) => Ok(s), other => Err(Fault { msg: "other".to_string() }) };
            assert!(a.is_err() && b == Ok(1) && c.is_err());
            assert_eq!(all(vec!["1".to_string(), "2".to_string()]), Ok(vec![("1".to_string(), 1), ("2".to_string(), 2)]));
            assert_eq!(all(vec!["x".to_string()]), Err("bad x".to_string()));
            let early = |n: i64| { if n < 0 { return Err("negative".to_string()); } Ok(n * 2) };
            assert_eq!(early(2), Ok(4));
            let held: Vec<_> = vec![1, 2];
            let items: Vec<Result<i64, String>> = vec![Ok(1), Ok(2)];
            let sum: Result<Vec<i64>, String> = items.into_iter().collect();
            assert_eq!(sum, Ok(held));
            0
        }
    "#), 0);
}

#[test]
fn functions_variants_and_methods_as_values() {
    assert_eq!(run(r#"
        enum E { Parse(String), Other }
        struct W { n: i64 }
        impl W { fn make(n: i64) -> W { W { n } } fn get(self) -> i64 { self.n } }
        fn main() -> i64 {
            let xs: Vec<Option<i64>> = vec![1, 2].into_iter().map(Some).collect();
            assert_eq!(xs, vec![Some(1), Some(2)]);
            let r: Result<i64, E> = Err("x".to_string()).map_err(E::Parse);
            assert!(matches!(r, Err(E::Parse(_))));
            let ws: Vec<W> = vec![3, 4].into_iter().map(W::make).collect();
            let total: i64 = ws.into_iter().map(W::get).sum();
            let names: Vec<String> = vec!["a".to_string()].into_iter().map(String::as_str).collect();
            assert_eq!(names, vec!["a"]);
            total
        }
    "#), 7);
}

#[test]
fn option_and_result_combinators() {
    assert_eq!(run(r#"
        fn main() -> i64 {
            assert_eq!(None.or_else(|| Some(2)), Some(2));
            assert_eq!(Some(3).filter(|x| x > 5), None);
            assert_eq!(Some(1).zip(Some("a")), Some((1, "a")));
            let t: Option<Result<i64, String>> = Some(Ok(4));
            assert_eq!(t.transpose(), Ok(Some(4)));
            assert_eq!(Some(Some(5)).flatten(), Some(5));
            let e: Result<i64, String> = Err("bad".to_string());
            assert_eq!(e.expect_err("must fail"), "bad");
            let ok: Result<i64, String> = Ok(2);
            assert_eq!(ok.and_then(|x| Ok(x + 1)).unwrap_or_else(|_| 0), 3);
            0
        }
    "#), 0);
}

#[test]
fn mutation_through_mut_accessors() {
    assert_eq!(run(r#"
        struct Frame { ip: i64 }
        struct Ctx { frames: Vec<Frame> }
        impl Ctx {
            fn top(mut self) -> Frame { self.frames.last_mut().unwrap() }
        }
        fn main() -> i64 {
            let mut st = Ctx { frames: vec![Frame { ip: 0 }, Frame { ip: 10 }] };
            st.frames.last_mut().unwrap().ip = 11;
            let f = st.frames.last_mut().unwrap();
            f.ip += 1;
            st.top().ip += 1;
            let g = st.top();
            g.ip += 1;
            let bump = |mut fr: Frame| fr.ip += 100;
            bump(st.frames[0]);
            st.frames[1].ip + st.frames[0].ip
        }
    "#), 114);
}

#[test]
fn modules_resolve_their_own_uses_variants_and_constants() {
    assert_eq!(run(r#"
        mod ast {
            pub enum Kind { Int(i64), Neg(i64), List(Vec<i64>) }
        }
        mod val {
            pub enum Value { Int(i64), Str(String) }
            pub const LIMIT: i64 = 1;
        }
        mod eval {
            use crate::ast::Kind;
            use crate::val::Value;
            pub const LIMIT: i64 = 2;
            pub fn kind(k: Kind) -> i64 {
                use Kind::*;
                match k { Int(n) => n, Neg(n) => 0 - n, List(xs) => xs.len() }
            }
            pub fn value(v: Value) -> i64 {
                use Value::*;
                match v { Int(n) => n + LIMIT, Str(_) => 0 }
            }
        }
        fn main() -> i64 {
            eval::kind(ast::Kind::Neg(3)) + eval::value(val::Value::Int(40)) + val::LIMIT
        }
    "#), 40);
}

#[test]
fn vec_clone_clones_elements() {
    assert_eq!(run(r#"
        #[derive(Clone)]
        struct Frame { ip: i64 }
        fn main() -> i64 {
            let frames = vec![Frame { ip: 1 }];
            let snapshot = frames.clone();
            let mut f = frames[0];
            f.ip = 5;
            snapshot[0].ip + frames[0].ip * 10
        }
    "#), 51);
}

#[test]
fn an_escaping_object_stored_in_a_replaced_struct_is_built() {
    // Scalar replacement must allocate `v` where it is stored into `s`,
    // even though `s` itself is replaced.
    assert_eq!(run(r#"
        struct Snap { items: Vec<i64>, n: i64 }
        fn build() -> i64 {
            let mut v: Vec<i64> = Vec::with_capacity(4);
            v.push(1);
            v.push(2);
            let s = Snap { items: v, n: 40 };
            let copy = s.items.clone();
            copy.len() + s.n
        }
        fn main() -> i64 { build() }
    "#), 42);
}

#[test]
fn a_block_arm_ends_before_a_tuple_pattern_arm() {
    assert_eq!(run(r#"
        fn main() -> i64 {
            let p = (1, 2);
            match p {
                (0, _) => { 10 }
                (a, b) => a + b,
            }
        }
    "#), 3);
}

#[test]
fn std_threads_do_not_keep_the_program_running() {
    // A `std::thread::spawn` thread is a daemon, as in Rust: `main` returning
    // ends the program; a `Thread::spawn` child is still joined first.
    let file = std::env::temp_dir().join(format!("gcr-daemon-{}.gcr", std::process::id()));
    std::fs::write(&file, r#"
        use std::sync::atomic::{AtomicBool, Ordering};
        fn main() -> i64 {
            let running = AtomicBool::new(true);
            let r = running.clone();
            let _ticker = std::thread::spawn(move || { while r.load(Ordering::Relaxed) { std::thread::sleep(std::time::Duration::from_millis(1)); } 0 });
            let _child = Thread::spawn(|| { Thread::sleep(5); println!("child done"); 1 });
            42
        }
    "#).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gcr")).arg("run").arg(&file).output().unwrap();
    let _ = std::fs::remove_file(&file);
    assert_eq!(out.status.code(), Some(42));
    assert!(String::from_utf8_lossy(&out.stdout).contains("child done"));
}

#[test]
fn serde_style_derives_round_trip() {
    let serde = include_str!("../ports/funct/src/serde.gcr");
    let src = format!(r#"
        mod serde {{ {serde} }}
        use serde::{{Content, Deserialize, Serialize}};
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub enum SV {{ Unit, Idx(usize), List(Vec<SV>), Pair(String, i64), Rec {{ a: i64, b: Option<String> }}, Boxed(Box<SV>) }}
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        pub struct Snap {{ pub version: u32, pub items: Vec<SV>, #[serde(default)] pub extra: Vec<u32> }}
        fn main() -> i64 {{
            let s = Snap {{ version: 1, items: vec![SV::Unit, SV::Idx(3), SV::List(vec![SV::Pair("x".to_string(), 2)]), SV::Rec {{ a: 1, b: None }}, SV::Boxed(SV::Unit)], extra: vec![] }};
            let c = s.serialize();
            let back = <Snap as Deserialize>::deserialize(c).unwrap();
            assert_eq!(back, s);
            let without_extra = serde::Content::Map(vec![("version".to_string(), serde::Content::U64(2)), ("items".to_string(), serde::Content::Seq(vec![]))]);
            assert_eq!(<Snap as Deserialize>::deserialize(without_extra).unwrap().version, 2);
            0
        }}
    "#);
    assert_eq!(run(&src), 0);
}

#[test]
fn gcr_test_runs_unit_and_integration_tests() {
    let dir = std::env::temp_dir().join(format!("gcr-test-cmd-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("gcr.toml"), "[package]\nname = \"demo-lib\"\n").unwrap();
    std::fs::write(dir.join("src/main.gcr"), r#"
        pub fn add(a: i64, b: i64) -> i64 { a + b }
        fn main() -> i64 { add(1, 2) }
        #[cfg(test)]
        mod tests {
            use super::*;
            #[test]
            fn adds() { assert_eq!(add(2, 2), 4); }
            #[test]
            fn fails() { assert_eq!(add(2, 2), 5); }
        }
    "#).unwrap();
    std::fs::write(dir.join("tests/integ.gcr"), r#"
        use demo_lib::add;
        #[test]
        fn from_outside() { assert_eq!(add(10, 5), 15); }
        #[test]
        fn returns_result() -> Result<(), String> { if add(1, 1) == 2 { Ok(()) } else { Err("bad".to_string()) } }
    "#).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gcr")).arg("test").current_dir(&dir).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!out.status.success(), "{text}");
    for line in ["test tests::adds ... ok", "test tests::fails ... FAILED", "test integ::from_outside ... ok",
                 "test integ::returns_result ... ok", "test result: FAILED. 3 passed; 1 failed"] {
        assert!(text.contains(line), "missing `{line}` in:\n{text}");
    }
}

#[test]
fn arc_and_rc_share_a_box_and_deref_to_it() {
    assert_eq!(run(r#"
        use std::sync::Arc;
        use std::rc::Rc;
        #[derive(Debug, Clone, PartialEq)]
        struct Proto { name: String, code: Vec<i64> }
        #[derive(Debug, Clone)]
        enum Val { Unit, Int(i64), P(Arc<Proto>), F(Arc<dyn Fn(i64) -> i64>) }
        fn total(p: Proto) -> i64 { p.code.iter().sum() }
        fn main() -> i64 {
            let p = Arc::new(Proto { name: "f".to_string(), code: vec![1, 2, 3] });
            let q = p.clone();
            assert!(Arc::ptr_eq(p, q));
            assert!(!Arc::ptr_eq(p, Arc::new(Proto::clone(p))));
            assert_eq!(p.name, "f");
            assert_eq!(p.code.len(), 3);
            assert_eq!(p.code[1], 2);
            assert_eq!(total(q), 6);
            let mut deep = Proto::clone(p);
            deep.code.push(4);
            assert_eq!(p.code.len(), 3);
            let double: Arc<dyn Fn(i64) -> i64> = Arc::new(|x| x * 2);
            let v = vec![Val::Unit, Val::Int(4), Val::P(q), Val::F(double)];
            let mut n = 0;
            for x in v.iter() {
                match x {
                    Val::P(pp) => n += pp.code.len() as i64,
                    Val::Int(i) => n += i,
                    Val::F(f) => n += f(10),
                    Val::Unit => {}
                }
            }
            assert_eq!(n, 27);
            let r: Rc<Vec<i64>> = Rc::new(vec![5, 6]);
            assert_eq!(r.len(), 2);
            assert_eq!(format!("{:?} {}", r, Arc::new(3)), "[5, 6] 3");
            assert!(Arc::new(1) == Arc::new(1));
            let o: Arc<Option<i64>> = Arc::new(Some(2));
            let got = match o { Some(x) => x, None => 0 };
            assert_eq!(got, 2);
            0
        }
    "#), 0);
}
