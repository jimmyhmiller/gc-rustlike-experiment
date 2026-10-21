//! Transparent aliases must behave consistently across CLI checking and runners.
use std::process::Command;
#[allow(dead_code)]
mod support;

#[test]
fn aliases_work_through_cli_jit_and_native_under_collection() {
    let root = std::env::temp_dir().join(format!("gcr-alias-cli-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("main.gcr");
    let binary = root.join("program");
    for source in [
        "trait Bound {} impl Bound for i64 {} type Words<T:Bound> = Array<T>; fn consume<T>(values:Array<T>,sample:T)->i64 {array_len(values)+40} fn main()->i64 {consume(Words::new(2),42)}",
        "fn take<T>(values:Array<T>,sample:T)->i64 {array_len(values)+42} fn main()->i64 {let mut order=0; let result=take({let scratch=[1,2];let action=||42;order=order+1;[]},{order=order*10+2;42});if order==12 && result==42 {42}else{0}}",
        "fn main() -> i64 { let mut values = HashMap::new(); if values.contains_key(\"answer\") { return 0; } let before = values.len(); values.insert(\"answer\", 42); values.get(\"answer\").unwrap_or(0) + before }",
        "trait Bound {} impl Bound for i64 {} type Values<T: Bound> = Vec<T>; fn main() -> i64 { let mut values = Values::new(); values.push(42); values[0] }",
        "struct Factory {} impl Factory { fn empty<T>() -> Array<T> { [] } } fn take(values: Array<i64>) -> i64 { array_len(values) + 42 } fn main() -> i64 { let values = Factory::empty(); take(values) }",
        "trait Bound {} impl Bound for i64 {} type Values<T: Bound> = Array<T>; fn first<T: Bound>(values: Values<T>) -> T { values[0] } fn main() -> i64 { let values: Values<i64> = [42]; first(values) }",
        "trait Bound {} impl Bound for i64 {} struct Boxed<T> { value: T } impl<T> Boxed<T> { fn make(value: T) -> Self { Boxed { value } } } type R<T: Bound> = Boxed<T>; fn main() -> i64 { let value = R::make(42); match value { R { value } => value } }",
        "trait Bound {} impl Bound for i64 {} type Restricted<T: Bound> = Array<T>; type Chained<U: Bound> = Restricted<U>; type Words = Chained<i64>; fn main() -> i64 { let mut values = Words::new(2); values[0] = 40; values[1] = 2; values[0] + values[1] }",
        r###"type Word = i64;
type Pointer = RawPtr;
type Comparator = extern fn(Pointer, Pointer) -> i32;
type Words = Array<Word>;
extern "C" fn qsort(mut base: Pointer, n: Word, size: Word, compar: Comparator);
fn compare(a: Pointer, b: Pointer) -> i32 {
    let x = unsafe { ptr_read_i64(a) }; let y = unsafe { ptr_read_i64(b) };
    if x < y { -1i32 } else if x > y { 1i32 } else { 0i32 }
}
fn main() -> i64 {
    let mut values = Words::new(2); values[0] = 40; values[1] = 2;
    unsafe { qsort(as_c_bytes(values), 2, 8, compare) };
    if values[0] == 2 && values[1] == 40 { 42 } else { 0 }
}
"###,
        "type Words<T> = Array<T>; fn main() -> i64 { let mut values = Words::new(2); values[0] = 40; values[1] = 2; values[0] + values[1] }",
        r#"type Text = Array<String>; fn main() -> i64 { let mut values = Text::new(2); values[0] = "forty"; values[1] = "two"; str_len(values[0]) + str_len(values[1]) + 34 }"#,
        "fn truth<i64>(value: i64) -> i64 { value } fn identity<String>(value: String) -> String { value } type Identity<i64> = i64; fn main() -> i64 { let enabled: Identity<bool> = true; if truth(enabled) { identity(42) } else { 0 } }",
        "trait Base {} trait Derived: Base {} struct Boxed<T> { value: T } impl<T> Base for Boxed<T> {} impl<T> Derived for Boxed<T> {} fn accept<T: Derived>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: true }) }",
        "enum Choice<T> { Empty, Value(T) } type Alias<T> = Choice<T>; impl<T> Choice<T> { fn replace(self, other: Self) -> Self { other } } fn main() -> i64 { let value = Alias::Empty.replace(Alias::Value(42)); match value { Alias::Empty => 0, Alias::Value(n) => n } }",
        "struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<Array<T>>>; fn main() -> i64 { let mut calls = 0; let rows = Rows { value: [{ calls = calls + 1; [] }, { calls = calls * 10 + 2; [42] }] }; if calls == 12 { rows.value[1][0] + array_len(rows.value[0]) } else { 0 } }",
        "enum Choice<T> { Values { sample: T, items: Array<T> } } type Batch<T> = Choice<T>; fn main() -> i64 { let mut calls = 0; let batch = Batch::Values { items: { let scratch = [1, 2]; let action = || 42; calls = calls + 1; [] }, sample: { calls = calls * 10 + 2; 42 } }; match batch { Batch::Values { items, sample } => if calls == 12 && array_len(items) == 0 { sample } else { 0 } } }",
        "struct Pair<T> { items: Array<T>, sample: T } type Batch<T> = Pair<T>; fn main() -> i64 { let mut calls = 0; let batch = Batch { items: { let scratch = [1, 2]; let action = || 42; calls = calls + 1; [] }, sample: { calls = calls * 10 + 2; 42 } }; if calls == 12 && array_len(batch.items) == 0 { batch.sample } else { 0 } }",
        r#"struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<Array<T>>>; fn main() -> i64 { let rows = Rows { value: [["forty"], ["two"]] }; str_len(rows.value[0][0]) + str_len(rows.value[1][0]) + 34 }"#,
        "struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<Array<T>>>; fn main() -> i64 { let rows = Rows { value: [[40], [2]] }; rows.value[0][0] + rows.value[1][0] }",
        "struct Boxed<T> { value: T } impl<T> Boxed<T> { fn identity<T>(value: T) -> T { value } } type Boolean = Boxed<bool>; fn main() -> i64 { Boolean::identity(42) }",
        "struct Boxed<T> { value: T } impl<T> Boxed<T> { fn identity<T>(self, value: T) -> T { value } } fn main() -> i64 { Boxed { value: true }.identity(42) }",
        "type Count = i64; fn identity(value: Count) -> Count { value } fn main() -> i64 { identity(42) }",
        "struct Boxed<T> { value: T } type Wrapped<U> = Boxed<U>; fn main() -> i64 { let value = Wrapped { value: 42 }; match value { Wrapped { value } => value } }",
        "enum Choice<T> { Value(T), Empty } type Integer = Choice<i64>; fn main() -> i64 { let value = Integer::Value(42); match value { Integer::Value(n) => n, Integer::Empty => 0 } }",
        "type Batch = ((Array<i64>, i64), Array<i64>); fn main() -> i64 { let value: Batch = (([40], 0), [2]); value.0.0[0] + value.1[0] }",
        "struct Boxed<T> { value: T } impl<T> Boxed<T> { fn make(value: T) -> Self { Boxed { value } } fn get(self) -> T { self.value } } type Integer = Boxed<i64>; fn main() -> i64 { Integer::make(42).get() }",
    ] {
        std::fs::write(&path, source).unwrap();
        let output = support::run_with_timeout(
            Command::new(env!("CARGO_BIN_EXE_gcr"))
                .arg("check")
                .arg(&path),
            30,
        );
        assert!(output.status.success(), "{source}: {output:?}");
        let output = support::run_with_timeout(
            Command::new(env!("CARGO_BIN_EXE_gcr"))
                .arg("build")
                .arg(&path)
                .arg("-o")
                .arg(&binary)
                .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()),
            60,
        );
        assert!(output.status.success(), "{source}: {output:?}");
        for native in [false, true] {
            let mut command = if native {
                Command::new(&binary)
            } else {
                let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                command.arg("run").arg(&path).arg("--jit");
                command
            };
            let output = support::run_with_timeout(
                command.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1"),
                30,
            );
            if native {
                assert_eq!(output.status.code(), Some(42), "{source}: {output:?}");
            } else {
                assert!(output.status.success(), "{source}: {output:?}");
                assert_eq!(
                    String::from_utf8_lossy(&output.stdout).trim(),
                    "42",
                    "{source}"
                );
            }
        }
        std::fs::remove_file(&binary).unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_aliases_fail_with_source_diagnostics_before_native_output() {
    let root = std::env::temp_dir().join(format!("gcr-alias-errors-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("main.gcr");
    let binary = root.join("program");
    for (source, diagnostic) in [
        ("trait Bound {} type Words<T:Bound> = Array<T>; fn take<T>(values:Array<T>,sample:T)->i64 {42} fn main()->i64 {take(Words::new(0),true)}", "not satisfied"),
        ("struct Key { value: i64 } fn main() -> i64 { let mut values = HashMap::new(); values.insert(Key { value: 42 }, 42); 42 }", "not satisfied"),
        ("trait Bound {} type Values<T: Bound> = Vec<T>; fn main() -> i64 { let mut values = Values::new(); values.push(true); 42 }", "not satisfied"),
        ("trait Bound {} type Values<T: Bound> = Array<T>; fn main() -> i64 { let values: Values<i64> = [42]; values[0] }", "required trait obligation"),
        ("trait Bound {} type R<T: Bound> = T; fn unused<T>(value: R<T>) -> T { value } fn main() -> i64 { 42 }", "required trait obligation"),
        ("trait Bound {} struct Boxed<T> { value: T } type R<T: Bound> = Boxed<T>; fn main() -> i64 { let value = R { value: true }; 42 }", "not satisfied"),
        ("trait Bound {} enum Choice<T> { Value(T), Empty } type R<T: Bound> = Choice<T>; fn main() -> i64 { let value = R::Value(true); 42 }", "not satisfied"),
        ("trait Bound {} type R<T: Bound> = Array<T>; fn main() -> i64 { let mut value = R::new(1); value[0] = true; 42 }", "not satisfied"),
        ("trait Bound {} struct Boxed<T> { value: T } impl<T> Boxed<T> { fn make(value: T) -> Self { Boxed { value } } } type R<T: Bound> = Boxed<T>; fn main() -> i64 { let value = R::make(true); 42 }", "not satisfied"),
        ("trait Bound {} struct Boxed<T> { value: T } type R<T: Bound> = Boxed<T>; fn main() -> i64 { let value = Boxed { value: true }; match value { R { value } => 42 } }", "not satisfied"),
        ("trait Bound {} type Restricted<T: Bound> = Array<T>; type Bad = Restricted<i64>; fn main() -> i64 { 42 }", "required trait obligation"),
        ("trait Bound {} type Restricted<T: Bound> = Array<T>; type Bad<T> = Restricted<T>; fn main() -> i64 { 42 }", "required trait obligation"),
        ("trait Bound {} type Restricted<T: Bound> = i64; type Bad = Restricted<bool>; fn main() -> i64 { 42 }", "required trait obligation"),
        ("type Values<T: Missing> = Array<T>; fn main() -> i64 { 42 }", "unknown trait"),
        ("trait Bound<A> {} type Values<T: Bound> = Array<T>; fn main() -> i64 { 42 }", "takes 1"),
        ("mod hidden { trait Bound {} } type Values<T: hidden::Bound> = Array<T>; fn main() -> i64 { 42 }", "private"),
        ("type words = Array<i64>; mod words {} fn main() -> i64 { 42 }", "conflicts with a type alias"),
        ("mod words {} type words = Array<i64>; fn main() -> i64 { 42 }", "conflicts with a type alias"),
        (r###"type Pointer = RawPtr;
type Comparator = extern fn(Pointer, Pointer) -> i32;
extern "C" fn takes_callback(value: Comparator);
fn wrong(a: Pointer, b: Pointer) -> i64 { 42 }
fn main() -> i64 { unsafe { takes_callback(wrong) }; 42 }
"###, "callback"),
        ("type Words = Array<i64>; fn main() -> i64 { let values: Array<bool> = Words::new(2); 0 }", "type mismatch"),
        (
            "trait Base {} trait Derived: Base {} struct Item { value: i64 } impl Derived for Item {} fn main() -> i64 { 42 }",
            "required trait obligation",
        ),
        (
            "trait First: Second {} trait Second: First {} fn main() -> i64 { 42 }",
            "cyclic supertraits",
        ),
        (
            "enum Choice<T> { Empty, Value(T) } type Alias<T> = Choice<T>; impl<T> Choice<T> { fn identity<T>(self, value: T) -> T { value } } fn main() -> i64 { Alias::Empty.identity(42) }",
            "cannot infer",
        ),
        (
            "type First = Second; type Second = First; fn main() -> i64 { 42 }",
            "cyclic type aliases",
        ),
        (
            "type Missing = unknown::Type; fn main() -> i64 { 42 }",
            "unknown",
        ),
        (
            "type Identity<T> = T; fn main() -> i64 { let value: Identity = 42; value }",
            "argument",
        ),
        (
            "struct Boxed<T> { value: T } type Integer = Boxed<i64>; fn main() -> i64 { let value = Integer { value: true }; 42 }",
            "type mismatch",
        ),
        (
            "enum Choice<T> { Value(T) } type Integer = Choice<i64>; fn main() -> i64 { let value = Choice::Value(true); match value { Integer::Value(n) => 42 } }",
            "type mismatch",
        ),
    ] {
        std::fs::write(&path, source).unwrap();
        for mode in ["check", "build", "jit"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
            command
                .arg(if mode == "jit" { "run" } else { mode })
                .arg(&path);
            if mode == "jit" {
                command.arg("--jit");
            }
            if mode == "build" {
                command.arg("-o").arg(&binary);
            }
            let output = support::run_with_timeout(&mut command, 30);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{mode}: {source}: {stderr}");
            assert!(stderr.contains(diagnostic), "{mode}: {source}: {stderr}");
            assert!(
                stderr.contains(&format!("{}:", path.display())),
                "{mode}: {stderr}"
            );
            assert!(stderr.contains('^'), "{mode}: {stderr}");
            assert!(!stderr.contains("panicked"), "{mode}: {stderr}");
            assert!(!binary.exists(), "invalid alias produced native output");
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
