//! Rust surface parity: format specs and Display/Debug, derives, std paths,
//! closure traits and shared mutable captures, trait objects, associated
//! types and lazy iterators, and `macro_rules!`. Each program checks its own
//! results with `assert_eq!` and returns an `i64`; programs run under GC stress.

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

#[test]
fn format_specs_match_rust() {
    assert_eq!(run(r#"
        fn main() -> i64 {
            assert_eq!(format!("[{:>6}] [{:<6}] [{:^6}] [{:*^7}]", 42, "ab", "mid", "c"), "[    42] [ab    ] [ mid  ] [***c***]");
            assert_eq!(format!("[{:06.2}] [{:+}] [{:.3}] [{:e}]", 3.14159, 7, 2.0, 1234.5), "[003.14] [+7] [2.000] [1.2345e3]");
            assert_eq!(format!("{:x} {:X} {:#x} {:o} {:#b} {:08b}", 255, 255, 255, 8, 5, 5), "ff FF 0xff 10 0b101 00000101");
            assert_eq!(format!("{:x}", -1), "ffffffffffffffff");
            assert_eq!(format!("{:x}", -1i32), "ffffffff");
            let w = 8;
            let p = 2;
            assert_eq!(format!("[{:w$.p$}] [{:>1$}] [{:.*}]", 1.5, 5, 3, 0.123456), "[    1.50] [    5] [0.123]");
            let name = "z";
            assert_eq!(format!("{name:>4}|"), "   z|");
            assert_eq!(format!("{:?} {:?} {:?}", 1.0, "a\n\"b\"", 'x'), "1.0 \"a\\n\\\"b\\\"\" 'x'");
            assert_eq!(format!("{:?} {:?}", vec![1, 2], (1, "a")), "[1, 2] (1, \"a\")");
            let none: Option<i64> = None;
            let r: Result<i64, String> = Err("bad");
            assert_eq!(format!("{:?} {:?} {:?}", Some(5), none, r), "Some(5) None Err(\"bad\")");
            assert_eq!(format!("{:#?}", vec![(1, 2)]), "[\n    (\n        1,\n        2,\n    ),\n]");
            0
        }
    "#), 0);
}

#[test]
fn display_and_debug_traits_and_derives() {
    assert_eq!(run(r#"
        use std::fmt;
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        struct Point { x: i64, y: i64 }
        impl fmt::Display for Point {
            fn fmt(self, f: fmt::Formatter) -> fmt::Result { write!(f, "({}, {})", self.x, self.y) }
        }
        #[derive(Debug, Clone, PartialEq)]
        enum Shape { Circle(f64), Rect { w: f64, h: f64 }, Empty }
        #[derive(Debug, Default)]
        struct Config { name: String, retries: i64, items: Vec<i64> }
        fn main() -> i64 {
            let a = Point { x: 1, y: -2 };
            let b = a.clone();
            assert_eq!(format!("{} {:?}", a, b), "(1, -2) Point { x: 1, y: -2 }");
            assert!(a == b);
            assert!(a < Point { x: 1, y: 0 });
            assert_eq!(format!("{:>9}|", a), "  (1, -2)|");
            assert_eq!(format!("{:#?}", a), "Point {\n    x: 1,\n    y: -2,\n}");
            let s = vec![Shape::Circle(1.5), Shape::Rect { w: 2.0, h: 3.0 }, Shape::Empty];
            assert_eq!(format!("{:?}", s), "[Circle(1.5), Rect { w: 2.0, h: 3.0 }, Empty]");
            assert!(s[1] == Shape::Rect { w: 2.0, h: 3.0 });
            assert_eq!(format!("{:?}", Config::default()), "Config { name: \"\", retries: 0, items: [] }");
            let mut m: HashMap<Point, i64> = HashMap::new();
            m.insert(a, 7);
            assert_eq!(m.get(b), Some(7));
            assert_eq!(a.to_string(), "(1, -2)");
            0
        }
    "#), 0);
}

#[test]
fn trait_qualified_calls_disambiguate_same_named_methods() {
    assert_eq!(run(r#"
        trait A { fn name(self) -> i64; }
        trait B { fn name(self) -> i64; }
        struct S { v: i64 }
        impl A for S { fn name(self) -> i64 { self.v + 1 } }
        impl B for S { fn name(self) -> i64 { self.v + 100 } }
        impl S { fn get(self) -> i64 { self.v } }
        fn main() -> i64 { let s = S { v: 1 }; A::name(s) + B::name(s) + S::get(s) }
    "#), 104);
    let msg = compile_error(r#"
        trait A { fn name(self) -> i64; }
        trait B { fn name(self) -> i64; }
        struct S { v: i64 }
        impl A for S { fn name(self) -> i64 { 1 } }
        impl B for S { fn name(self) -> i64 { 2 } }
        fn main() -> i64 { S { v: 0 }.name() }
    "#);
    assert!(msg.contains("multiple traits define `name`"), "{msg}");
}

#[test]
fn closure_traits_impl_trait_and_shared_mutable_captures() {
    assert_eq!(run(r#"
        use std::fmt::Display;
        fn apply<F: Fn(i64) -> i64>(f: F, x: i64) -> i64 { f(x) }
        fn apply2<F>(f: F, x: i64) -> i64 where F: FnMut(i64) -> i64 { f(x) }
        fn make_adder(n: i64) -> impl Fn(i64) -> i64 { move |x| x + n }
        fn boxed(n: i64) -> Box<dyn Fn(i64) -> i64> { Box::new(move |x| x * n) }
        fn show(x: impl Display) -> String { format!("<{}>", x) }
        fn call_twice<F: FnMut()>(f: F) { f(); f(); }
        fn main() -> i64 {
            assert_eq!(apply(|x| x + 1, 1) + apply2(|x| x * 2, 4) + make_adder(5)(1), 16);
            let fs: Vec<Box<dyn Fn(i64) -> i64>> = vec![boxed(2), boxed(3)];
            assert_eq!(fs[0](7) + fs[1](7), 35);
            assert_eq!(show(5) + show("s"), "<5><s>");
            let mut count = 0;
            let mut inc = || count += 1;
            inc();
            inc();
            let mut total = 0;
            call_twice(|| total += 5);
            let mut log: Vec<String> = Vec::new();
            let mut push = |s: String| log.push(s);
            push("a");
            push("b");
            assert_eq!(log.len(), 2);
            count * 100 + total
        }
    "#), 210);
}

#[test]
fn trait_objects() {
    assert_eq!(run(r#"
        use std::fmt;
        trait Shape {
            fn area(self) -> f64;
            fn name(self) -> String { "shape".to_string() }
            fn scale(mut self, k: f64);
        }
        struct Circle { r: f64 }
        struct Square { s: f64 }
        impl Shape for Circle {
            fn area(self) -> f64 { 3.0 * self.r * self.r }
            fn name(self) -> String { "circle".to_string() }
            fn scale(mut self, k: f64) { self.r = self.r * k; }
        }
        impl Shape for Square {
            fn area(self) -> f64 { self.s * self.s }
            fn scale(mut self, k: f64) { self.s = self.s * k; }
        }
        trait Named: fmt::Display { fn id(self) -> i64; }
        struct Thing { n: i64 }
        impl fmt::Display for Thing { fn fmt(self, f: fmt::Formatter) -> fmt::Result { write!(f, "Thing#{}", self.n) } }
        impl Named for Thing { fn id(self) -> i64 { self.n } }
        fn make(big: bool) -> Box<dyn Shape> {
            if big { Box::new(Square { s: 10.0 }) } else { Box::new(Circle { r: 1.0 }) }
        }
        fn total(shapes: Vec<Box<dyn Shape>>) -> f64 {
            let mut t = 0.0;
            for s in shapes { t += s.area(); }
            t
        }
        fn main() -> i64 {
            let shapes: Vec<Box<dyn Shape>> = vec![Box::new(Circle { r: 2.0 }), Box::new(Square { s: 3.0 })];
            assert_eq!(shapes[0].name() + shapes[1].name(), "circleshape");
            let mut m = make(true);
            m.scale(2.0);
            assert_eq!(m.area(), 400.0);
            let n: Box<dyn Named> = Box::new(Thing { n: 4 });
            assert_eq!(format!("{} {}", n, n.id()), "Thing#4 4");
            total(vec![make(false), make(true)]) as i64
        }
    "#), 103);
    let msg = compile_error(r#"
        trait Shape { fn area(self) -> f64; }
        struct Rock {}
        fn main() -> i64 { let s: Box<dyn Shape> = Box::new(Rock {}); 0 }
    "#);
    assert!(msg.contains("does not implement `Shape`"), "{msg}");
}

#[test]
fn associated_types_and_iterators() {
    assert_eq!(run(r#"
        trait Container {
            type Item;
            fn get(self, i: i64) -> Self::Item;
            fn first(self) -> Self::Item { self.get(0) }
        }
        struct Nums { v: Vec<i64> }
        impl Container for Nums { type Item = i64; fn get(self, i: i64) -> i64 { self.v[i] } }
        fn first_of<C: Container>(c: C) -> C::Item { c.first() }
        fn sum_all<C: Container<Item = i64>>(c: C, n: i64) -> i64 {
            let mut t = 0;
            for i in 0..n { t += c.get(i); }
            t
        }
        struct Countdown { n: i64 }
        impl Iterator for Countdown {
            type Item = i64;
            fn next(mut self) -> Option<i64> {
                if self.n == 0 { None } else { self.n -= 1; Some(self.n + 1) }
            }
        }
        fn main() -> i64 {
            let n = Nums { v: vec![3, 4, 5] };
            assert_eq!(first_of(n) + sum_all(n, 3), 15);
            let v = vec![1, 2, 3, 4, 5];
            let doubled: Vec<i64> = v.iter().map(|x| x * 2).collect();
            assert_eq!(doubled, vec![2, 4, 6, 8, 10]);
            assert_eq!(v.iter().filter(|x| x % 2 == 0).count(), 2);
            let mut pairs = Vec::new();
            for (i, x) in v.iter().enumerate() { pairs.push(i * x); }
            assert_eq!(pairs, vec![0, 2, 6, 12, 20]);
            let s: i64 = (1..=10).sum();
            assert_eq!(s, 55);
            assert_eq!((0..5).map(|x| x * x).fold(0, |a, b| a + b), 30);
            assert_eq!((0..20).step_by(5).collect::<Vec<_>>(), vec![0, 5, 10, 15]);
            assert_eq!(v.iter().rev().take(2).collect::<Vec<i64>>(), vec![5, 4]);
            assert_eq!(v.iter().min_by_key(|x| (x - 3) * (x - 3)), Some(3));
            assert_eq!((1..4).flat_map(|n| (0..n)).collect::<Vec<_>>(), vec![0, 0, 1, 0, 1, 2]);
            let r: String = "hello".chars().rev().collect();
            assert_eq!(r, "olleh");
            let m = (0..3).map(|i| (i, i * i)).collect::<HashMap<_, _>>();
            assert_eq!(m.get(2), Some(4));
            let mut it = v.iter().peekable();
            assert_eq!(it.peek(), Some(1));
            assert_eq!(it.next(), Some(1));
            let names = vec!["a", "b"];
            let zipped: Vec<String> = names.iter().zip(1..10).map(|p| format!("{}{}", p.0, p.1)).collect();
            assert_eq!(zipped.join(","), "a1,b2");
            let mut down = 0;
            for x in (Countdown { n: 3 }) { down = down * 10 + x; }
            assert_eq!(down, 321);
            assert_eq!(Countdown { n: 4 }.sum(), 10);
            let d: i64 = Default::default();
            d
        }
    "#), 0);
}

#[test]
fn macro_rules_expand_expressions_statements_and_items() {
    assert_eq!(run(r#"
        macro_rules! square { ($x:expr) => { $x * $x }; }
        macro_rules! max_of {
            ($x:expr) => { $x };
            ($x:expr, $($rest:expr),+) => { { let a = $x; let b = max_of!($($rest),+); if a > b { a } else { b } } };
        }
        macro_rules! my_vec {
            () => { Vec::new() };
            ($($x:expr),+ $(,)?) => { { let mut v = Vec::new(); $( v.push($x); )+ v } };
        }
        macro_rules! point_struct {
            ($name:ident, $($field:ident),*) => { #[derive(Debug)] struct $name { $($field: i64),* } };
        }
        point_struct!(P3, x, y, z);
        trait Describe { fn describe(self) -> String; }
        macro_rules! impl_describe {
            ($($t:ty => $s:literal),* $(,)?) => { $( impl Describe for $t { fn describe(self) -> String { format!("{} {}", $s, self) } } )* };
        }
        impl_describe!(i64 => "int", bool => "bool");
        macro_rules! getter { ($name:ident, $v:expr) => { fn $name() -> i64 { $v } }; }
        getter!(one, 1);
        getter!(two, 2);
        macro_rules! hashmap {
            ($($k:expr => $v:expr),* $(,)?) => {{ let mut m = HashMap::new(); $( m.insert($k, $v); )* m }};
        }
        fn main() -> i64 {
            assert_eq!(square!(3 + 1), 16);
            assert_eq!(max_of!(3, 9, 2, 7), 9);
            let v: Vec<i64> = my_vec![1, 2, 3,];
            assert_eq!(v.len(), 3);
            assert_eq!(format!("{:?}", P3 { x: 1, y: 2, z: 3 }), "P3 { x: 1, y: 2, z: 3 }");
            assert_eq!(5.describe() + true.describe(), "int 5bool true");
            let m = hashmap!{ "a" => 1, "b" => 2 };
            assert_eq!(m.get("b"), Some(2));
            one() + two()
        }
    "#), 3);
    let msg = compile_error("macro_rules! m { ($x:expr) => { $x }; } fn main() -> i64 { m!(1, 2) }");
    assert!(msg.contains("no rule of macro `m!` matches"), "{msg}");
}

#[test]
fn rust_entry_points_and_std_paths() {
    assert_eq!(run(r#"
        use std::collections::{HashMap, HashSet};
        use std::fmt::{self, Display};
        #[allow(dead_code)]
        fn helper() {}
        #[cfg(test)]
        mod tests { fn not_compiled() { nope(); } }
        struct Node { v: i64, next: Option<Box<Node>> }
        fn main() {
            let n = Node { v: 1, next: Some(Box::new(Node { v: 2, next: None })) };
            let mut m: std::collections::HashMap<i64, i64> = HashMap::new();
            m.insert(1, 2);
            match n.next { Some(b) => assert_eq!(b.v + n.v, 3), None => unreachable!() }
        }
    "#), 0);
    assert_eq!(run(r#"
        fn main() -> Result<(), String> { Err("boom") }
    "#), 1);
}

#[test]
fn or_range_and_at_patterns_constants_and_char_methods() {
    assert_eq!(run(r#"
        fn classify(n: i64) -> String {
            match n {
                i64::MIN..=-1 => "neg".to_string(),
                0 => "zero".to_string(),
                1 | 2 | 3 => "small".to_string(),
                x @ 4..=9 => format!("digit {}", x),
                10..100 => "tens".to_string(),
                _ => "big".to_string(),
            }
        }
        fn kind(c: char) -> i64 { match c { 'a'..='z' | 'A'..='Z' => 1, '0'..='9' => 2, _ => 0 } }
        fn main() -> i64 {
            let got: Vec<String> = vec![-5, 0, 2, 7, 42, 500].iter().map(|n| classify(n)).collect();
            assert_eq!(got.join(" "), "neg zero small digit 7 tens big");
            assert_eq!(kind('q') * 100 + kind('7') * 10 + kind('#'), 120);
            let hit = match (1, Some(3)) { (0 | 1, Some(2 | 3)) => 1, _ => 0 };
            let ranged = if let Some(1..=5) = Some(4) { 1 } else { 0 };
            let neg = match -3 { -3 => 1, _ => 0 };
            assert_eq!(i64::MAX, 9223372036854775807);
            assert_eq!(i64::MIN + 1, -9223372036854775807);
            assert_eq!(u8::MAX as i64 + i32::MIN as i64, -2147483393);
            assert!(f64::INFINITY > f64::MAX);
            assert!(f64::consts::PI > 3.14 && f64::consts::PI < 3.15);
            assert!('7'.is_ascii_digit() && !'x'.is_ascii_digit() && 'x'.is_alphabetic());
            assert_eq!('f'.to_digit(16), Some(15));
            assert_eq!('a'.to_ascii_uppercase(), 'A');
            hit + ranged + neg
        }
    "#), 3);
    let msg = compile_error("fn main() -> i64 { let 1 | 2 = 1; 0 }");
    assert!(msg.contains("or-patterns and range patterns are supported in"), "{msg}");
}

#[test]
fn impl_trait_results_and_dyn_iterators() {
    assert_eq!(run(r#"
        use std::fmt::Display;
        fn evens(limit: i64) -> impl Iterator<Item = i64> { (0..limit).filter(|x| x % 2 == 0) }
        fn squares_of(v: Vec<i64>) -> impl Iterator<Item = i64> { v.into_iter().map(|x| x * x) }
        fn label(n: i64) -> impl Display {
            if n > 0 { return format!("+{}", n); }
            format!("{}", n)
        }
        fn chained() -> impl Iterator<Item = i64> { evens(6).chain(squares_of(vec![3, 4])) }
        struct Grid { w: i64 }
        impl Grid {
            fn cells(self) -> impl Iterator<Item = (i64, i64)> {
                let w = self.w;
                (0..w * w).map(move |i| (i / w, i % w))
            }
        }
        fn boxed(desc: bool) -> Box<dyn Iterator<Item = i64>> {
            if desc { Box::new((0..5).rev()) } else { Box::new(evens(10)) }
        }
        fn main() -> i64 {
            assert_eq!(evens(10).collect::<Vec<_>>(), vec![0, 2, 4, 6, 8]);
            assert_eq!(squares_of(vec![1, 2, 3]).sum::<i64>(), 14);
            assert_eq!(format!("{} {}", label(5), label(-2)), "+5 -2");
            assert_eq!(chained().collect::<Vec<_>>(), vec![0, 2, 4, 9, 16]);
            assert_eq!((Grid { w: 2 }).cells().count(), 4);
            assert_eq!(boxed(true).map(|x| x * 10).collect::<Vec<_>>(), vec![40, 30, 20, 10, 0]);
            let mut total = 0;
            for x in boxed(false) { total += x; }
            let shown: Box<dyn std::fmt::Display> = Box::new(42);
            assert_eq!(shown.to_string(), "42");
            total
        }
    "#), 20);
    let msg = compile_error("fn f() -> impl Iterator<Item = String> { 0..3 } fn main() -> i64 { f().count() }");
    assert!(msg.contains("has `Item` = `i64`, not `String`"), "{msg}");
    let msg = compile_error("fn f(n: i64) -> impl Iterator<Item = i64> { if n == 0 { 0..1 } else { f(n - 1) } } fn main() -> i64 { f(2).count() }");
    assert!(msg.contains("calls itself"), "{msg}");
}

#[test]
fn loops_labels_let_else_and_matches() {
    assert_eq!(run(r#"
        fn find(grid: Vec<Vec<i64>>, target: i64) -> Option<(i64, i64)> {
            let mut found = None;
            'rows: for (r, row) in grid.iter().enumerate() {
                for (c, v) in row.iter().enumerate() {
                    if v == target { found = Some((r, c)); break 'rows; }
                    if v < 0 { continue 'rows; }
                }
            }
            found
        }
        fn first_even(v: Vec<i64>) -> i64 {
            let Some(x) = v.iter().find(|x| x % 2 == 0) else { return -1; };
            x
        }
        fn lifetimes<'a, T: 'a + Clone>(x: T) -> T where T: Sized { x }
        fn main() -> i64 {
            assert_eq!(find(vec![vec![1, -1, 9], vec![4, 9, 6]], 9), Some((1, 1)));
            let mut count = 0;
            let r = 'outer: loop {
                let mut i = 0;
                while i < 10 {
                    i += 1;
                    count += 1;
                    if count == 25 { break 'outer count * 2; }
                    if i == 5 { continue 'outer; }
                }
            };
            assert_eq!((r, count), (50, 25));
            let mut i = 0;
            let x = loop { i += 1; if i == 3 { break i * 2; } };
            assert_eq!(x, 6);
            let y;
            if x > 5 { y = "big"; } else { y = "small"; }
            assert_eq!(y, "big");
            assert_eq!(first_even(vec![1, 3, 8]) + first_even(vec![1]), 7);
            assert!(matches!(Some(3), Some(1..=5)) && !matches!(Some(9), Some(n) if n < 5));
            let n: usize = 2;
            let letters: Vec<char> = vec!['x', 'y', 'z'];
            assert_eq!(letters[n], 'z');
            let mut abc = "".to_string();
            for c in 'a'..='c' { abc = abc + c.to_string(); }
            assert_eq!(abc, "abc");
            lifetimes(0)
        }
    "#), 0);
}

#[test]
fn struct_forms_enums_consts_modules_tuples_and_turbofish() {
    assert_eq!(run(r##"
        #[derive(Debug, Clone, PartialEq)]
        struct Meters(f64);
        struct Pair(i64, String);
        #[derive(Debug)]
        struct Marker;
        #[derive(Debug, Clone)]
        struct P { x: i64, y: i64 }
        enum Shape { Unit, Circle(Meters) }
        enum Color { R = 10, G, B = -1 }
        const BASE: i64 = 5;
        static DOUBLE: i64 = BASE * 2;
        impl Pair {
            fn new(n: i64) -> Self { Self(n, format!("#{}", n)) }
            fn swap(self) -> (String, i64) { let Self(a, b) = self; (b, a) }
        }
        impl Shape { fn r(self) -> f64 { match self { Self::Circle(Meters(m)) => m, Self::Unit => 0.0 } } }
        mod geo {
            pub(crate) const SCALE: i64 = 10;
            pub mod shapes {
                use super::SCALE;
                pub struct Sq(pub i64);
                impl Sq { pub fn area(self) -> i64 { self.0 * self.0 * SCALE + super::bonus() } }
            }
            fn bonus() -> i64 { crate::base() }
        }
        fn base() -> i64 { 100 }
        fn zero<T: Default>() -> T { T::default() }
        fn main() -> i64 {
            fn twice(x: i64) -> i64 { x * 2 }
            let Meters(inner) = Meters(2.5);
            assert_eq!(inner, 2.5);
            assert_eq!(Pair::new(3).swap(), ("#3", 3));
            assert_eq!(format!("{:?} {}", Marker, Shape::Circle(Meters(1.5)).r()), "Marker 1.5");
            let a = P { x: 1, y: 2 };
            let b = P { x: 9, ..a };
            assert_eq!((b.x, b.y), (9, 2));
            let mut m = Meters(1.0);
            m.0 += 1.0;
            assert_eq!(m, Meters(2.0));
            assert_eq!((Color::R as i64, Color::G as i64, Color::B as i64), (10, 11, -1));
            assert_eq!(DOUBLE, 10);
            assert_eq!(twice(geo::shapes::Sq(2).area()), 280);
            let mut v = vec![(3, "c"), (1, "z"), (1, "a")];
            v.sort();
            assert_eq!(v[0], (1, "a"));
            assert!((1, "b") < (1, "c") && (2, 0) > (1, 9));
            let mut hm: HashMap<(i64, i64), i64> = HashMap::new();
            hm.insert((1, 2), 3);
            assert_eq!(hm.get((1, 2)), Some(3));
            let e = Vec::<i64>::new();
            assert_eq!(vec![1, 2, 3].iter().sum::<i64>() + e.len(), 6);
            assert_eq!(zero::<i64>(), 0);
            0
        }
    "##), 0);
}

#[test]
fn associated_constants() {
    assert_eq!(run(r#"
        trait Shape {
            const SIDES: i64;
            const NAME: String = "shape";
            fn describe(self) -> String { format!("{} with {} sides", Self::NAME, Self::SIDES) }
        }
        struct Tri;
        struct Sq;
        impl Shape for Tri { const SIDES: i64 = 3; }
        impl Shape for Sq { const SIDES: i64 = 4; const NAME: String = "square"; }
        fn sides<T: Shape>(_s: T) -> i64 { T::SIDES * 10 }
        struct Grid;
        impl Grid { const W: i64 = 4; const AREA: i64 = Self::W * Self::W; }
        fn main() -> i64 {
            assert_eq!(Tri.describe(), "shape with 3 sides");
            assert_eq!(Sq.describe(), "square with 4 sides");
            sides(Sq) + Grid::AREA
        }
    "#), 56);
    let msg = compile_error("trait T { const X: i64; } struct A; impl T for A {} fn main() -> i64 { 0 }");
    assert!(msg.contains("missing constant `X`"), "{msg}");
}

#[test]
fn rust_shaped_operator_traits_and_multiple_impls() {
    assert_eq!(run(r#"
        use std::ops::{Add, Sub, Mul, Neg, AddAssign};
        use std::cmp::Ordering;
        #[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
        struct V2 { x: f64, y: f64 }
        impl Add for V2 { type Output = V2; fn add(self, o: V2) -> V2 { V2 { x: self.x + o.x, y: self.y + o.y } } }
        impl Sub for V2 { type Output = Self; fn sub(self, o: Self) -> Self { V2 { x: self.x - o.x, y: self.y - o.y } } }
        impl Mul<f64> for V2 { type Output = V2; fn mul(self, k: f64) -> V2 { V2 { x: self.x * k, y: self.y * k } } }
        impl Mul<V2> for V2 { type Output = f64; fn mul(self, o: V2) -> f64 { self.x * o.x + self.y * o.y } }
        impl Neg for V2 { type Output = V2; fn neg(self) -> V2 { V2 { x: -self.x, y: -self.y } } }
        impl AddAssign for V2 { fn add_assign(mut self, o: V2) { self.x += o.x; self.y += o.y; } }
        #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
        enum Level { Low, Mid(i64), High }
        struct Money(i64);
        impl PartialEq for Money { fn eq(self, o: Money) -> bool { self.0 == o.0 } }
        impl Eq for Money {}
        impl PartialOrd for Money { fn partial_cmp(self, o: Money) -> Option<Ordering> { Some(self.cmp(o)) } }
        impl Ord for Money { fn cmp(self, o: Money) -> Ordering { self.0.cmp(o.0) } }
        struct Meters { v: i64 }
        impl From<i64> for Meters { fn from(n: i64) -> Meters { Meters { v: n } } }
        impl From<bool> for Meters { fn from(b: bool) -> Meters { Meters { v: if b { 1 } else { 0 } } } }
        fn sum_all<T: Add<Output = T> + Copy>(xs: Vec<T>, zero: T) -> T { let mut acc = zero; for x in xs { acc = acc + x; } acc }
        fn main() -> i64 {
            let a = V2 { x: 1.0, y: 2.0 };
            let b = V2 { x: 3.0, y: 4.0 };
            let mut c = a + b * 2.0 - -a;
            c += V2 { x: 0.5, y: 0.5 };
            assert_eq!(c, V2 { x: 8.5, y: 12.5 });
            assert_eq!(a * b, 11.0);
            assert!(a < b && Level::Mid(3) < Level::High && Level::Mid(3) > Level::Mid(2));
            assert!(Money(5) > Money(3) && !(Money(2) != Money(2)));
            assert_eq!(sum_all(vec![a, b], V2 { x: 0.0, y: 0.0 }), V2 { x: 4.0, y: 6.0 });
            assert_eq!(2.0f64.partial_cmp(3.0), Some(Ordering::Less));
            let m1 = Meters::from(5);
            let m2: Meters = From::from(true);
            sum_all(vec![1, 2, 3], 0) + Money(1).max(Money(7)).0 + m1.v + m2.v
        }
    "#), 19);
}
