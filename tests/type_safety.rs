//! Runtime layouts must follow nominal declarations, not familiar spellings.
//! Malformed or unrepresentable types must fail before LLVM/native allocation.
use gcrust::{compile::parse_with_prelude, lower::lower_program, resolve::resolve_module};
use std::process::Command;
mod support;
fn error(source: &str) -> String {
    let (m, _) = parse_with_prelude(source).unwrap();
    let r = match resolve_module(m) {
        Ok(r) => r,
        Err(e) => return e.msg,
    };
    lower_program(&r.globals)
        .err()
        .expect("invalid type accepted")
        .msg
}
#[test]
fn record_offsets_and_embedded_reference_offsets_do_not_wrap() {
    for (field, count) in [("i64", 8192), ("String", 8192)] {
        let fields = (0..count)
            .map(|i| format!("f{i}: {field}"))
            .collect::<Vec<_>>()
            .join(",");
        let src = format!("struct Big {{ {fields} }} fn main() -> i64 {{ let mut b: Big; 0 }}");
        assert!(error(&src).contains("layout exceeds representable"));
    }
    let fields = (0..8190)
        .map(|i| format!("f{i}: i64"))
        .collect::<Vec<_>>()
        .join(",");
    let src = format!(
        "#[value] struct Inner {{ {fields}, last: String }} struct Big {{ pad: String, value: Inner }} fn main() -> i64 {{ let mut b: Big; 0 }}"
    );
    assert!(error(&src).contains("layout exceeds representable"));
    let fields = (0..8193)
        .map(|i| format!("f{i}: i64"))
        .collect::<Vec<_>>()
        .join(",");
    let src =
        format!("#[value] struct Big {{ {fields} }} fn main() -> i64 {{ let mut b: Big; 0 }}");
    assert!(error(&src).contains("layout exceeds representable"));
}
#[test]
fn qualified_user_arrays_are_not_runtime_array_objects() {
    let src = "mod custom { pub struct Array<T> { count: i64, data: T } } fn main() -> i64 { let x = custom::Array { count: 1, data: \"valid\" }; x[0].len() }";
    let e = error(src);
    assert!(e.contains("Index") || e.contains("index"), "{e}");
    for source in [
        "struct Array<T> { data: T } fn main() -> i64 { 0 }",
        "enum Array { Empty } fn main() -> i64 { 0 }",
        "type Array = i64; fn main() -> i64 { 0 }",
        "fn main() -> i64 { let mut a: Array; 0 }",
        "fn main() -> i64 { let mut a: String<i64>; 0 }",
        "mod actual { pub struct Thing { value: i64 } } fn main() -> i64 { let x = missing::Thing { value: 42 }; x.value }",
        "enum Choice { Item(i64) } fn main() -> i64 { let x = missing::Choice::Item(42); 0 }",
    ] {
        assert!(!error(source).is_empty());
    }
}
#[test]
fn redeclared_and_qualified_nominal_types_keep_their_own_layouts() {
    let source = r#"
        mod custom { pub struct Vec<T> { item: T } pub struct Array<T> { data: T } pub enum Option<T> { None(i64), Some(T) } }
        fn main() -> i64 {
            let n = process_args().expect("args").len();
            let v = custom::Vec { item: n + 7 };
            let e: custom::Option<String> = custom::Option::None(35 - n);
            let mut items: Array<custom::Option<String>> = Array::new(2);
            items[0] = e; items[1] = custom::Option::Some("rooted");
            let named: custom::Vec<String> = custom::Vec { item: "other" };
            let ordinary: custom::Array<i64> = custom::Array { data: 5 };
            for i in 0..100 { let text = i.to_string(); if text.len() == 0 { return 1; } }
            if named.item != "other" || ordinary.data != 5 { return 2; }
            match items[0] { custom::Option::None(value) => value + v.item, custom::Option::Some(s) => s.len() }
        }
    "#;
    assert_runtime(source, "nominal-types");
}

fn assert_runtime(source: &str, label: &str) {
    let root = std::env::temp_dir().join(format!("gcr-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("main.gcr");
    let bin = root.join("program");
    std::fs::write(&path, source).unwrap();
    let output = support::run_with_timeout(
        Command::new(env!("CARGO_BIN_EXE_gcr"))
            .arg("build")
            .arg(&path)
            .arg("-o")
            .arg(&bin)
            .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib()),
        60,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for native in [false, true] {
        let mut cmd = if native {
            Command::new(&bin)
        } else {
            let mut c = Command::new(env!("CARGO_BIN_EXE_gcr"));
            c.arg("run").arg(&path);
            c
        };
        let output =
            support::run_with_timeout(cmd.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1"), 30);
        if native {
            assert_eq!(output.status.code(), Some(42), "{output:?}");
        } else {
            assert!(output.status.success(), "{output:?}");
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn generic_and_runtime_symbol_spellings_do_not_define_identity() {
    assert_runtime(
        r#"
        struct I64 { text: String }
        struct Box<T> { value: T }
        #[value] struct ValueBox<T> { value: T }
        mod foo { pub struct Bar { text: String } }
        struct foo_Bar { number: i64 }
        fn wrap<T>(value: T) -> Box<T> { Box { value } }
        fn identity<T>(value: T) -> T { value }
        fn ai_read_file() -> i64 { 7 }
        fn __closure_user() -> i64 { 7 }
        fn main() -> i64 {
            let no_captures = || 7;
            if __closure_user() != no_captures() { return 4; }
            let number = wrap(30);
            let scalar_value: ValueBox<i64> = ValueBox { value: 9 };
            let pointer_value: ValueBox<I64> = ValueBox { value: I64 { text: "value-root" } };
            let scalar_array: Array<ValueBox<i64>> = Array::new(1);
            let pointer_array: Array<ValueBox<I64>> = Array::new(1);
            let mut scalar_array = scalar_array;
            let mut pointer_array = pointer_array;
            scalar_array[0] = scalar_value;
            pointer_array[0] = pointer_value;
            let reference = wrap(I64 { text: "five!" });
            let qualified = identity(foo::Bar { text: "abc" });
            let underscored = identity(foo_Bar { number: 3 });
            for i in 0..100 { let text = i.to_string(); if text.len() == 0 { return 1; } }
            if reference.value.text != "five!" || qualified.text != "abc" { return 2; }
            if scalar_array[0].value != 9 || pointer_array[0].value.text != "value-root" { return 3; }
            number.value + reference.value.text.len() + underscored.number + ai_read_file() - 3
        }
    "#,
        "identity-collisions",
    );
}

#[test]
fn builtin_container_spellings_respect_nominal_declarations() {
    use gcrust::{
        layout::LayoutRegistry,
        lexer::lex,
        parser::parse_module,
        types::{Prim, Ty, TyCtx},
    };
    let module = parse_module(
        &lex("struct Vec<T> { payload: T } enum Option<T> { None(i64), Some(T) }").unwrap(),
    )
    .unwrap();
    let resolved = resolve_module(module).unwrap();
    let ctx = TyCtx::from_globals(&resolved.globals);
    let mut layouts = LayoutRegistry::new(&ctx);
    let vector = layouts
        .repr(&Ty::Named {
            name: "Vec".into(),
            args: vec![Ty::Prim(Prim::I64)],
        })
        .unwrap();
    let option = layouts
        .repr(&Ty::Named {
            name: "Option".into(),
            args: vec![Ty::Prim(Prim::Str)],
        })
        .unwrap();
    let gcrust::core::Repr::Ref(vector) = vector else {
        panic!("reference struct expected")
    };
    let gcrust::core::Repr::Ref(option) = option else {
        panic!("reference enum expected")
    };
    assert_eq!(layouts.layouts[vector as usize].field_map.len(), 1);
    assert_eq!(layouts.layouts[vector as usize].ptr_fields, 0);
    assert_eq!(layouts.layouts[vector as usize].raw_bytes, 8);
    assert_eq!(layouts.layouts[option as usize].raw_bytes, 8);
    assert_eq!(layouts.layouts[option as usize].ptr_fields, 1);
}

#[test]
fn entry_point_has_the_runner_abi() {
    for source in [
        "fn main(x: String) -> i64 { x.len() }",
        "fn main<T>() -> i64 { 0 }",
        "fn main() -> String { \"wrong ABI\" }",
        "fn main() -> Result<i64, String> { Ok(0) }",
        "mod nested { pub fn main() -> i64 { 0 } }",
    ] {
        let e = error(source);
        assert!(e.contains("entry point") || e.contains("no `main`"), "{e}");
    }
}

#[test]
fn record_initializers_follow_written_order_and_preserve_roots() {
    assert_runtime(
        r#"
        struct Trace { value: i64 }
        struct Pair { first: String, second: String }
        #[value] struct ValuePair { first: String, second: String }
        enum Named { Pair { first: String, second: String } }
        #[value] enum ValueNamed { Pair { first: String, second: String } }
        fn mark(mut trace: Trace, tag: i64) -> String {
            trace.value = trace.value * 10 + tag;
            for i in 0..100 { let text = i.to_string(); if text.len() == 0 { panic("allocation"); } }
            tag.to_string()
        }
        fn main() -> i64 {
            let first = Trace { value: 0 };
            let record = Pair { second: mark(first, 2), first: mark(first, 1) };
            let second = Trace { value: 0 };
            let value = ValuePair { second: mark(second, 2), first: mark(second, 1) };
            if record.first != "1" || record.second != "2" || value.first != "1" || value.second != "2" { return 1; }
            let third = Trace { value: 0 };
            let named = Named::Pair { second: mark(third, 2), first: mark(third, 1) };
            let fourth = Trace { value: 0 };
            let value_named = ValueNamed::Pair { second: mark(fourth, 2), first: mark(fourth, 1) };
            match named { Named::Pair { first, second } => if first != "1" || second != "2" { return 2; } }
            match value_named { ValueNamed::Pair { first, second } => if first != "1" || second != "2" { return 3; } }
            (first.value + second.value + third.value + fourth.value) / 2
        }
    "#,
        "record-evaluation-order",
    );
    for (source, diagnostic) in [
        (
            "struct X { a: i64 } fn main() -> i64 { let x = X { a: 1, a: 2 }; 0 }",
            "duplicate field",
        ),
        (
            "struct X { a: i64 } fn main() -> i64 { let x = X { a: 1, b: 2 }; 0 }",
            "unknown field",
        ),
        (
            "struct X { a: i64, b: i64 } fn main() -> i64 { let x = X { a: 1 }; 0 }",
            "missing field",
        ),
    ] {
        let e = error(source);
        assert!(e.contains(diagnostic), "{e}");
    }
}

#[test]
fn aggregate_sizes_and_closure_pointer_counts_are_checked() {
    let mut source = "#[value] struct V0 { n: i64 }\n".to_string();
    for i in 1..=30 {
        source.push_str(&format!(
            "#[value] struct V{i} {{ left: V{}, right: V{} }}\n",
            i - 1,
            i - 1
        ));
    }
    source.push_str("fn main() -> i64 { let mut huge: V30; 0 }");
    assert!(error(&source).contains("layout exceeds representable"));

    use gcrust::{core::Repr, layout::LayoutRegistry, types::TyCtx};
    let resolved = resolve_module(parse_with_prelude("fn main() -> i64 { 0 }").unwrap().0).unwrap();
    let ctx = TyCtx::from_globals(&resolved.globals);
    for count in [8192, 65536] {
        let mut layouts = LayoutRegistry::new(&ctx);
        assert!(
            layouts
                .closure_env("oversized", &vec![Repr::Ref(0); count])
                .is_err()
        );
    }
}

#[test]
fn ambiguous_short_types_require_an_explicit_path_or_import() {
    let definitions =
        "mod a { pub struct Shared { n: i64 } } mod b { pub struct Shared { text: String } }";
    let source = format!("{definitions} fn main() -> i64 {{ let mut x: Shared; 0 }}");
    let e = error(&source);
    assert!(e.contains("unknown") || e.contains("ambiguous"), "{e}");
    let source = format!(
        r#"{definitions} use b::Shared; fn main() -> i64 {{ let value: Shared = Shared {{ text: "selected" }}; value.text.len() + 34 }}"#
    );
    assert_runtime(&source, "unambiguous-type-import");
}

#[test]
fn inline_cycles_are_rejected_but_reference_cycles_and_nested_generics_are_valid() {
    for definition in [
        "#[value] struct Bad { child: Bad }",
        "#[value] enum Bad { Child(Bad), End }",
        "#[value] struct Bad { child: Other } #[value] struct Other { child: Bad }",
        "#[value] struct Bad<T> { child: Bad<(T, T)> }",
    ] {
        let ty = if definition.contains("Bad<T>") {
            "Bad<i64>"
        } else {
            "Bad"
        };
        let e = error(&format!(
            "{definition} fn main() -> i64 {{ let mut bad: {ty}; 0 }}"
        ));
        assert!(e.contains("no finite representation"), "{e}");
    }
    assert_runtime(
        r#"
        #[value] struct Node { next: Option<Link>, text: String }
        struct Link { node: Node }
        #[value] struct Wrap<T> { value: T }
        fn main() -> i64 {
            let mut link = Link { node: Node { next: None, text: "rooted" } };
            link.node = Node { next: Some(link), text: "rooted" };
            let nested: Wrap<Wrap<Wrap<i64>>> = Wrap { value: Wrap { value: Wrap { value: 36 } } };
            for i in 0..100 { let text = i.to_string(); if text.len() == 0 { return 1; } }
            link.node.next.unwrap().node.text.len() + nested.value.value.value
        }
    "#,
        "recursive-value-layouts",
    );
    // Start registration with the value type, before the reference descriptor.
    let source = "#[value] struct Node { link: Link } struct Link { node: Node } fn main() -> i64 { let mut n: Node; 0 }";
    let resolved = resolve_module(parse_with_prelude(source).unwrap().0).unwrap();
    lower_program(&resolved.globals).unwrap();
}

#[test]
fn failed_layout_registration_rolls_back_placeholders_and_worklists() {
    use gcrust::{
        layout::LayoutRegistry,
        types::{Ty, TyCtx},
    };
    let source = "#[value] struct Bad { next: Bad } struct Holder { bad: Bad } #[heap] struct Good { n: i64 } fn main() -> i64 { 0 }";
    let resolved = resolve_module(parse_with_prelude(source).unwrap().0).unwrap();
    let ctx = TyCtx::from_globals(&resolved.globals);
    let mut layouts = LayoutRegistry::new(&ctx);
    let bad = Ty::Named {
        name: "Holder".into(),
        args: vec![],
    };
    for _ in 0..2 {
        assert!(layouts.repr(&bad).is_err());
        assert!(layouts.layouts.is_empty());
        assert!(layouts.values.is_empty());
    }
    layouts
        .repr(&Ty::Named {
            name: "Good".into(),
            args: vec![],
        })
        .unwrap();
    assert_eq!(layouts.layouts.len(), 1);
    assert_eq!(layouts.layouts[0].raw_bytes, 8);
}

#[test]
fn recursive_inline_array_elements_use_completed_box_descriptors() {
    assert_runtime(
        r#"
        #[value] struct Node { children: Array<Node>, text: String }
        #[value] enum Tree { Leaf(String), Branch(Array<Tree>) }
        fn main() -> i64 {
            let mut children: Array<Node> = Array::new(1);
            children[0] = Node { children: Array::new(0), text: "leaf" };
            let root = Node { children, text: "parent" };
            let mut branches: Array<Tree> = Array::new(1);
            branches[0] = Tree::Leaf("leaf");
            let tree = Tree::Branch(branches);
            for i in 0..100 { let text = i.to_string(); if text.len() == 0 { return 1; } }
            let score = match tree {
                Tree::Leaf(_) => 0,
                Tree::Branch(values) => match values[0] { Tree::Leaf(text) => text.len(), Tree::Branch(_) => 0 }
            };
            root.children[0].text.len() + score + 34
        }
    "#,
        "recursive-inline-arrays",
    );
}

#[test]
fn recursive_reference_container_fallbacks_do_not_require_a_prelude() {
    use gcrust::{
        layout::LayoutRegistry,
        lexer::lex,
        parser::parse_module,
        types::{Ty, TyCtx},
    };
    for field in ["Option<Node>", "Vec<Node>", "Result<Node, i64>"] {
        let source = format!("#[value] struct Node {{ next: {field} }}");
        let resolved = resolve_module(parse_module(&lex(&source).unwrap()).unwrap()).unwrap();
        let ctx = TyCtx::from_globals(&resolved.globals);
        let mut layouts = LayoutRegistry::new(&ctx);
        layouts
            .repr(&Ty::Named {
                name: "Node".into(),
                args: vec![],
            })
            .unwrap();
        assert_eq!(layouts.values.len(), 1);
        assert!(
            layouts
                .layouts
                .iter()
                .all(|layout| !layout.name.contains("pending"))
        );
    }
}
