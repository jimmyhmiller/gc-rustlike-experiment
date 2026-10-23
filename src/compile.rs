//! The front-to-back compile pipeline with prelude injection.
//!
//! `parse_with_prelude` parses the embedded `prelude.gcr` and prepends every
//! prelude item whose name the user program does NOT already declare. This lets
//! programs use `Option`/`Result` and the prelude helpers without declaring
//! them, while existing programs that DO declare their own (e.g. the examples)
//! keep working — the user's declaration shadows the prelude's.

use crate::ast::{Item, ItemKind, ModDef, Module};
use crate::core::SourceEntry;
use crate::lexer::{lex, lex_with_source, SourceId};
use crate::parser::{ParseError, parse_module};
use std::path::{Path, PathBuf};

thread_local! {
    static PRELUDE_SOURCE: std::cell::Cell<Option<SourceId>> = const { std::cell::Cell::new(None) };
}

/// The source id the standard library was lexed with, once injected.
pub fn prelude_source() -> Option<SourceId> {
    PRELUDE_SOURCE.with(|p| p.get())
}

const PRELUDE_SRC: &str = concat!(
    include_str!("prelude.gcr"),
    "\n",
    include_str!("stdlib/io.gcr"),
    "\n",
    include_str!("stdlib/parallel.gcr"),
    "\n",
    include_str!("stdlib/csv.gcr"),
    "\n",
    include_str!("stdlib/heap.gcr")
);

/// The SourceMap built during prelude/module merging: one [`SourceEntry`] per
/// [`SourceId`] (0 = user, then `mod` files, then the prelude). Returned by the
/// parse entries and attached to `CoreProgram.sources` so spans resolve against
/// their real source (multi-source span resolution — debugger foundation).
pub type SourceMap = Vec<SourceEntry>;

/// Register a source, returning its [`SourceId`] (= its index in the map). Fails
/// loudly rather than silently wrapping if a program somehow exceeds the
/// `SourceId` (u16) capacity — the `id == map index` invariant must hold.
fn add_source(sources: &mut SourceMap, path: String, text: String) -> SourceId {
    let id = SourceId::try_from(sources.len())
        .expect("source count exceeds SourceId (u16) capacity");
    sources.push(SourceEntry { path, text });
    id
}

/// Resolve a `mod foo;` declaration to its file. Following modern Rust, a module
/// `foo` declared in a file living in `dir` is loaded from either `dir/foo.gcr`
/// or `dir/foo/mod.gcr`. Returns the chosen path (and the directory that nested
/// `mod` declarations inside it should resolve against).
fn module_file(dir: &Path, name: &str) -> Result<(PathBuf, PathBuf), ParseError> {
    let flat = dir.join(format!("{}.gcr", name));
    let nested = dir.join(name).join("mod.gcr");
    if flat.exists() {
        // Nested mods inside `foo.gcr` resolve against `dir/foo/`.
        Ok((flat, dir.join(name)))
    } else if nested.exists() {
        Ok((nested.clone(), dir.join(name)))
    } else {
        Err(ParseError {
            msg: format!(
                "cannot find module `{}`: expected `{}` or `{}`",
                name, flat.display(), nested.display()
            ),
            span: crate::lexer::Span::new(0, 0),
        })
    }
}

/// Recursively load file modules (`mod foo;`) referenced by `items`, parsing the
/// sibling files and replacing each declaration with an inline module holding the
/// loaded items. `dir` is the directory that this set of items' file modules
/// resolve against.
fn load_file_modules(items: &mut Vec<Item>, dir: &Path, sources: &mut SourceMap) -> Result<(), ParseError> {
    for item in items.iter_mut() {
        if let ItemKind::Mod(m) = &mut item.kind {
            if m.inline {
                // Inline module: its own nested file mods resolve against
                // `dir/<modname>/` (matching the nested-directory convention).
                let sub = dir.join(&m.name);
                load_file_modules(&mut m.items, &sub, sources)?;
            } else {
                let (file, subdir) = module_file(dir, &m.name)?;
                let src = std::fs::read_to_string(&file).map_err(|e| ParseError {
                    msg: format!("cannot read module file `{}`: {}", file.display(), e),
                    span: m.span,
                })?;
                // Each module is its own source: register it + lex with its id so
                // its spans resolve against its own text, not the user file.
                let sid = add_source(sources, file.display().to_string(), src.clone());
                let toks = lex_with_source(&src, sid);
                crate::lexer::register_source_path(sid, &file);
                let toks = toks.map_err(|e| ParseError { msg: e.msg, span: e.span })?;
                let mut parsed = parse_module(&toks)?;
                // Recurse into this file's own `mod foo;` declarations.
                load_file_modules(&mut parsed.items, &subdir, sources)?;
                m.items = parsed.items;
                m.inline = true;
            }
        }
    }
    Ok(())
}

/// The top-level name an item declares (for dedup against the prelude).
fn item_name(item: &Item) -> Option<&str> {
    match &item.kind {
        ItemKind::Fn(f) => Some(&f.name),
        ItemKind::Struct(s) => Some(&s.name),
        ItemKind::Enum(e) => Some(&e.name),
        ItemKind::Trait(t) => Some(&t.name),
        ItemKind::TypeAlias(a) => Some(&a.name),
        ItemKind::Const(c) => Some(&c.name),
        ItemKind::Mod(m) => Some(&m.name),
        ItemKind::Impl(_) | ItemKind::Use(_) => None,
    }
}

/// Prepend the prelude to a user module's items (dropping prelude items the user
/// redeclares), producing the final module.
fn inject_prelude(user: Module, sources: &mut SourceMap) -> Module {
    let user_names: std::collections::HashSet<String> = user
        .items
        .iter()
        .filter_map(|i| item_name(i).map(|s| s.to_string()))
        .collect();

    // The prelude is its own source ("<std>"): register it + lex with its id so
    // prelude spans resolve against PRELUDE_SRC, never a fabricated user line.
    let sid = add_source(sources, "<std>".to_string(), PRELUDE_SRC.to_string());
    PRELUDE_SOURCE.with(|p| p.set(Some(sid)));
    let prelude_tokens = lex_with_source(PRELUDE_SRC, sid).expect("prelude must lex");
    let prelude = parse_module(&prelude_tokens).expect("prelude must parse");

    let mut items: Vec<Item> = prelude
        .items
        .into_iter()
        .filter(|i| match &i.kind {
            // A prelude impl belongs to the prelude's type and trait. When the
            // user declares their own type or trait of that name, the impl
            // would attach to the user's item instead, so drop it.
            ItemKind::Impl(imp) => {
                !user_names.contains(&crate::types::type_base_name(&imp.self_ty))
                    && imp.trait_ref.as_ref().is_none_or(|t| !user_names.contains(t.path.last()))
            }
            _ => match item_name(i) {
                Some(n) => !user_names.contains(n),
                None => true,
            },
        })
        .collect();
    items.extend(user.items);
    Module { items }
}

/// Parse a user program (a single source string, no file modules) and inject the
/// prelude. Used for tests and the in-memory REPL-style path.
pub fn parse_with_prelude(user_src: &str) -> Result<(Module, SourceMap), ParseError> {
    crate::parser::reset_exported_macros();
    let mut sources: SourceMap = Vec::new();
    add_source(&mut sources, "<input>".to_string(), user_src.to_string()); // user = id 0
    let user_tokens = lex(user_src).map_err(|e| ParseError { msg: e.msg, span: e.span })?;
    let user = parse_module(&user_tokens)?;
    let module = inject_prelude(user, &mut sources);
    Ok((module, sources))
}

/// Parse a user program from a file, recursively loading any `mod foo;` file
/// modules relative to the file's directory, then inject the prelude. This is the
/// driver entry point for multi-file projects.
pub fn parse_file_with_prelude(path: &Path) -> Result<(Module, SourceMap), ParseError> {
    crate::parser::reset_exported_macros();
    let src = std::fs::read_to_string(path).map_err(|e| ParseError {
        msg: format!("cannot read `{}`: {}", path.display(), e),
        span: crate::lexer::Span::new(0, 0),
    })?;
    let mut sources: SourceMap = Vec::new();
    add_source(&mut sources, path.display().to_string(), src.clone()); // user = id 0
    let tokens = lex(&src).map_err(|e| ParseError { msg: e.msg, span: e.span })?;
    crate::lexer::register_source_path(0, path);
    let mut user = parse_module(&tokens)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    load_file_modules(&mut user.items, dir, &mut sources)?; // mods get ids 1..
    let module = inject_prelude(user, &mut sources); // prelude gets the next id
    Ok((module, sources))
}

/// A `#[test]` function `gcr test` runs: its path and whether it returns a
/// `Result` (an `Err` fails it).
pub struct TestFn {
    pub path: String,
    pub returns_result: bool,
}

/// `gcr test`: the program at `path` plus the integration tests in
/// `tests_dir` (each `tests/<name>.gcr` a root module `<name>`, as in Cargo),
/// with `main` replaced by a runner that calls the `#[test]` function its
/// first argument numbers. Parse with `parser::set_test_mode(true)`.
pub fn parse_tests_with_prelude(path: &Path, tests_dir: Option<&Path>) -> Result<(Module, SourceMap, Vec<TestFn>), ParseError> {
    crate::parser::reset_exported_macros();
    let src = std::fs::read_to_string(path).map_err(|e| ParseError {
        msg: format!("cannot read `{}`: {}", path.display(), e),
        span: crate::lexer::Span::new(0, 0),
    })?;
    let mut sources: SourceMap = Vec::new();
    add_source(&mut sources, path.display().to_string(), src.clone());
    let tokens = lex(&src).map_err(|e| ParseError { msg: e.msg, span: e.span })?;
    crate::lexer::register_source_path(0, path);
    let mut user = parse_module(&tokens)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    load_file_modules(&mut user.items, dir, &mut sources)?;
    if let Some(tests_dir) = tests_dir {
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(tests_dir).into_iter().flatten().flatten()
            .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "gcr")).collect();
        files.sort();
        for file in files {
            let name = file.file_stem().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(&file).map_err(|e| ParseError {
                msg: format!("cannot read `{}`: {}", file.display(), e),
                span: crate::lexer::Span::new(0, 0),
            })?;
            let sid = add_source(&mut sources, file.display().to_string(), text.clone());
            let toks = lex_with_source(&text, sid).map_err(|e| ParseError { msg: e.msg, span: e.span })?;
            crate::lexer::register_source_path(sid, &file);
            let mut parsed = parse_module(&toks)?;
            load_file_modules(&mut parsed.items, tests_dir, &mut sources)?;
            user.items.push(Item {
                kind: ItemKind::Mod(ModDef { vis: true, name, items: parsed.items, inline: true, span: crate::lexer::Span::new(0, 0) }),
                span: crate::lexer::Span::new(0, 0),
            });
        }
    }
    // The runner calls each test, so tests and the modules holding them are
    // made visible to it.
    fn collect(items: &mut [Item], prefix: &str, out: &mut Vec<TestFn>) -> bool {
        let mut any = false;
        for item in items {
            match &mut item.kind {
                ItemKind::Fn(f) if f.hints.test => {
                    f.vis = true;
                    any = true;
                    out.push(TestFn {
                        path: if prefix.is_empty() { f.name.clone() } else { format!("{prefix}::{}", f.name) },
                        returns_result: f.ret.is_some(),
                    });
                }
                ItemKind::Mod(m) => {
                    let inner = if prefix.is_empty() { m.name.clone() } else { format!("{prefix}::{}", m.name) };
                    if collect(&mut m.items, &inner, out) { m.vis = true; any = true; }
                }
                _ => {}
            }
        }
        any
    }
    let mut tests = Vec::new();
    collect(&mut user.items, "", &mut tests);
    // The runner replaces the program's own `main`.
    user.items.retain(|i| !matches!(&i.kind, ItemKind::Fn(f) if f.name == "main"));
    let mut runner = String::from("fn main() -> i64 {\n    let args = std::env::args();\n    if args.len() < 2 { return 2; }\n    let which = args[1];\n");
    for (i, t) in tests.iter().enumerate() {
        let call = if t.returns_result {
            format!("match crate::{}() {{ Ok(_) => {{}} Err(e) => {{ eprintln!(\"Error: {{:?}}\", e); return 1; }} }}", t.path)
        } else {
            format!("crate::{}();", t.path)
        };
        runner.push_str(&format!("    if which == \"{i}\" {{ {call} return 0; }}\n"));
    }
    runner.push_str("    2\n}\n");
    let sid = add_source(&mut sources, "<test runner>".to_string(), runner.clone());
    let toks = lex_with_source(&runner, sid).map_err(|e| ParseError { msg: e.msg, span: e.span })?;
    user.items.extend(parse_module(&toks)?.items);
    let module = inject_prelude(user, &mut sources);
    Ok((module, sources, tests))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower::lower_program;
    use crate::resolve::resolve_module;
    use crate::codegen::jit_run_i64_gc;

    fn run(src: &str) -> i64 {
        let (m, _) = parse_with_prelude(src).unwrap();
        let r = resolve_module(m).unwrap();
        let prog = lower_program(&r.globals).unwrap();
        unsafe { jit_run_i64_gc(&prog, false) }.unwrap()
    }

    #[test]
    fn lexical_type_declarations_shadow_outer_trait_bounds() {
        for declaration in ["struct Bound {}", "enum Bound { Value }", "type Bound = i64;"] {
            let source = format!("trait Bound {{}} impl Bound for i64 {{}} mod inner {{ {declaration} type Alias<T: Bound> = Array<T>; }} fn main() -> i64 {{ 42 }}");
            let (module, _) = parse_with_prelude(&source).unwrap();
            let error = resolve_module(module).err().unwrap();
            assert!(error.msg.contains("`inner::Bound` is not a trait"), "{}", error.msg);
        }
        assert_eq!(run("mod contracts { pub trait Bound {} } impl contracts::Bound for i64 {} mod inner { struct Bound {} pub type Alias<T: contracts::Bound> = Array<T>; } fn main()->i64 { let values: inner::Alias<i64> = [42]; values[0] }"), 42);
    }

    #[test]
    fn generic_binders_shadow_trait_names_in_bounds() {
        for declaration in [
            "type Alias<Bound,T:Bound> = Array<T>;",
            "struct Record<Bound,T:Bound> { value:T }",
            "fn accept<Bound,T:Bound>(value:T)->T {value}",
            "struct Record<Bound> {value:Bound} impl<Bound> Record<Bound> { fn accept<T:Bound>(value:T)->T {value} }",
        ] {
            let source = format!("trait Bound {{}} {declaration} fn main()->i64 {{42}}");
            let (module, _) = parse_with_prelude(&source).unwrap();
            let error = resolve_module(module).err().unwrap();
            assert!(error.msg.contains("type parameter `Bound` is not a trait"), "{}", error.msg);
        }
        assert_eq!(run("mod contracts {pub trait Bound {}} impl contracts::Bound for i64 {} fn accept<Bound,T:contracts::Bound>(unused:Bound,value:T)->T {value} fn main()->i64 {accept(true,42)}"),42);
    }

    #[test]
    fn duplicate_member_declarations_fail_before_lowering() {
        for (declaration, diagnostic, repeated) in [
            ("struct Record {x:i64,x:i64}", "duplicate field `x`", "x:i64"),
            ("#[value] struct Record {x:i64,x:i64}", "duplicate field `x`", "x:i64"),
            ("enum Choice {Value{x:i64,x:i64}}", "duplicate field `x`", "x:i64"),
            ("enum Choice {Value,Value}", "duplicate variant `Value`", "Value"),
        ] {
            let source = format!("{declaration} fn main()->i64 {{42}}");
            let (module, _) = parse_with_prelude(&source).unwrap();
            let error = resolve_module(module).err().unwrap();
            assert!(error.msg.contains(diagnostic), "{}", error.msg);
            assert_eq!(error.span.start as usize, source.rfind(repeated).unwrap());
        }
        assert_eq!(run("struct Left {x:i64} struct Right {x:i64} enum Choice {One{x:i64},Two{x:i64}} fn main()->i64 {Left{x:40}.x+Right{x:2}.x}"),42);
    }

    #[test]
    fn parameter_binding_names_are_unique_in_each_callable_scope() {
        for source in [
            "fn value(x:i64,x:i64)->i64{x} fn main()->i64{42}",
            "extern \"C\" fn value(x:i64,x:i64)->i64; fn main()->i64{42}",
            "struct Record {} impl Record {fn value(x:i64,x:i64)->i64{x}} fn main()->i64{42}",
            "trait Contract {fn value(x:i64,x:i64)->i64;} fn main()->i64{42}",
            "trait Contract {fn value(x:i64,x:i64)->i64{x}} fn main()->i64{42}",
            "fn main()->i64{let value=|x:i64,x:i64| x; 42}",
        ] {
            let (module, _) = parse_with_prelude(source).unwrap();
            let error = resolve_module(module).err().unwrap();
            assert!(error.msg.contains("duplicate parameter `x`"), "{}", error.msg);
            assert_eq!(error.span.start as usize, source.rfind("x:i64").unwrap());
        }
        assert_eq!(run("fn value(_:i64,_:i64,x:i64)->i64{x} fn main()->i64 {let f=|_:i64,_:i64,x:i64| x; value(1,2,f(3,4,42))}"),42);
        assert_eq!(run("fn main()->i64 {let x=40; let f=|x:i64| {let x=x+1; x}; x+f(1)}"),42);
    }

    #[test]
    fn accepted_nested_programs_lower_on_two_megabyte_worker_stacks() {
        for (wrapped, depth) in [(false, 48usize), (true, 24)] {
            for invalid in [false, true] {
                std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(move || {
                    let mut expression = if invalid { "same(true,42)" } else { "42" }.to_string();
                    for _ in 0..depth {
                        expression = if wrapped { format!("same({{{expression}}},42)") }
                            else { format!("same({expression},42)") };
                    }
                    let source = format!("fn same<T>(left:T,right:T)->i64{{42}} fn main()->i64{{{expression}}}");
                    let (module, _) = parse_with_prelude(&source).unwrap();
                    let resolved = resolve_module(module).unwrap();
                    assert_eq!(crate::lower::check_program(&resolved.globals).is_err(), invalid);
                    assert_eq!(lower_program(&resolved.globals).is_err(), invalid);
                }).unwrap().join().unwrap();
            }
        }
    }

    #[test]
    fn immediate_function_values_infer_and_reject_invalid_calls() {
        for source in [
            "fn main()->i64{(||42)()}",
            "fn main()->i64{(|x|x+1)(41)}",
            "fn main()->i64{({|x|x+1})(41)}",
            "fn make()->fn(i64)->i64{|x:i64|x+1} fn main()->i64{make()(41)}",
            "fn main()->i64{(if true {|x:i64|x+1} else {|x:i64|x+2})(41)}",
            "enum Choice<T>{Empty,Value(T)} fn apply(Empty:fn()->i64)->i64{Empty()} fn main()->i64{apply(||42)}",
        ] { assert_eq!(run(source), 42, "{source}"); }
        for source in [
            "fn main()->i64{42()}",
            "fn value()->i64{42} fn main()->i64{let value=7;value()}",
            "fn main()->i64{(|x:i64|x)()}",
            "fn main()->i64{(|x:i64|x)(true)}",
            "fn main()->i64{(||true)()}",
        ] {
            let (module,_) = parse_with_prelude(source).unwrap();
            let resolved = resolve_module(module).unwrap();
            assert!(lower_program(&resolved.globals).is_err(), "{source}");
        }
    }

    #[test]
    fn ordinary_generic_calls_need_no_speculative_argument_lowering() {
        let mut expression = "value".to_string();
        for _ in 0..48 { expression = format!("same({expression},value)"); }
        let source = format!("fn same<T>(left:T,right:T)->i64{{42}} fn main()->i64{{let value=42;{expression}}}");
        let (module, _) = parse_with_prelude(&source).unwrap();
        let resolved = resolve_module(module).unwrap();
        crate::lower::reset_probe_work();
        lower_program(&resolved.globals).unwrap();
        assert_eq!(crate::lower::probe_work(), 0);
        assert!(crate::lower::expr_work() <= 4*48);
    }

    #[test]
    fn nested_generic_calls_have_bounded_argument_probe_work() {
        for wrapping in ["plain", "block", "arithmetic", "right", "closure"] {
            for invalid in [false, true] {
                for depth in if wrapping == "closure" { [2usize, 4, 8, 12] } else { [4usize, 8, 12, 24] } {
                    let mut value = if invalid { "same(true,42)" } else { "42" }.to_string();
                    for _ in 0..depth {
                        value = match wrapping {
                            "plain" => format!("same({value},42)"),
                            "block" => format!("same({{{value}}},42)"),
                            "arithmetic" => format!("same({value}+0,42)"),
                            "right" => format!("same(42,{value})"),
                            "closure" => format!("same({{let f = || {value}; f()}},42)"),
                            _ => unreachable!(),
                        };
                    }
                    let source = format!("fn same<T>(left:T,right:T)->i64 {{42}} fn main()->i64 {{{value}}}");
                    let (module, _) = parse_with_prelude(&source).unwrap();
                    let resolved = resolve_module(module).unwrap();
                    crate::lower::reset_probe_work();
                    let result = lower_program(&resolved.globals);
                    assert_eq!(result.is_err(), invalid, "{wrapping} invalid={invalid}: {result:?}");
                    let probes = crate::lower::probe_work();
                    let expressions = crate::lower::expr_work();
                    eprintln!("nested generic wrapping={wrapping} invalid={invalid} depth={depth} probe_lowerings={probes} expression_lowerings={expressions}");
                    assert!(probes <= 8*depth*depth, "repeated probes: {probes}");
                    assert!(expressions <= 64*depth*depth, "repeated expression lowering: {expressions}");
                }
            }
        }
    }

    #[test]
    fn earlier_function_arguments_infer_from_later_concrete_arguments() {
        for source in [
            "fn take<T>(values:Array<T>,sample:T)->i64 {array_len(values)+42} fn main()->i64 {take([],42)}",
            "fn empty<T>()->Array<T> {[]} fn take<T>(values:Array<T>,sample:T)->i64 {array_len(values)+42} fn main()->i64 {take(empty(),42)}",
            "trait Bound {} impl Bound for i64 {} type Words<T:Bound> = Array<T>; fn take<T>(values:Array<T>,sample:T)->i64 {array_len(values)+42} fn main()->i64 {take(Words::new(0),42)}",
            "struct Holder<T>{value:T} fn take<T>(holder:Holder<Array<T>>,sample:T)->i64 {array_len(holder.value)+42} fn main()->i64 {take(Holder{value:[]},42)}",
            "fn take<T>(values:Array<T>,sample:T)->i64 {array_len(values)+42} fn main()->i64 {let mut order=0; let result=take({let scratch=[1,2]; let action=||42; order=order+1;[]},{order=order*10+2;42}); if order==12 && result==42 {42} else {0}}",
        ] {assert_eq!(run(source),42);}
    }

    #[test]
    fn argument_type_probes_preserve_bounds_and_nominal_checks() {
        for (source, expected) in [
            ("trait Bound {} type Words<T:Bound> = Array<T>; fn take<T>(values:Array<T>,sample:T)->i64 {42} fn main()->i64 {take(Words::new(0),true)}", "not satisfied"),
            ("struct Wrong<T>{value:T} fn take<T>(values:Array<T>,sample:T)->i64 {42} fn main()->i64 {take(Wrong{value:true},true)}", "Wrong"),
        ] {
            let (module, _) = parse_with_prelude(source).unwrap();
            let resolved = resolve_module(module).unwrap();
            let error = lower_program(&resolved.globals).err().unwrap();
            assert!(error.msg.contains(expected), "{}", error.msg);
        }
    }

    #[test]
    fn associated_constructor_results_infer_from_later_uses() {
        for source in [
            "fn main() -> i64 { let mut values = Vec::new(); values.push(42); values[0] }",
            "trait Bound {} impl Bound for i64 {} type Values<T: Bound> = Vec<T>; fn main() -> i64 { let mut values = Values::new(); values.push(42); values[0] }",
            "struct Factory {} impl Factory { fn empty<T>() -> Array<T> { [] } } fn take(values: Array<i64>) -> i64 { array_len(values) + 42 } fn main() -> i64 { let values = Factory::empty(); take(values) }",
        ] { assert_eq!(run(source), 42); }
    }
    #[test]
    fn map_inference_collects_keys_and_values_before_proving_bounds() {
        for source in [
            "fn main() -> i64 { let mut values = HashMap::new(); if values.contains_key(\"answer\") { return 0; } values.insert(\"answer\", 42); values.get(\"answer\").unwrap_or(0) }",
            "fn main() -> i64 { let mut values = HashMap::new(); let before = values.len(); values.insert(\"answer\", 42); values.get(\"answer\").unwrap_or(0) + before }",
        ] { assert_eq!(run(source), 42); }
    }
    #[test]
    fn probe_deferral_preserves_relational_bound_checks() {
        let valid = "trait Rel<A> {} impl Rel<i64> for String {} struct Pair<K,V> { keys: Array<K>, values: Array<V> } impl<K: Rel<V>, V> Pair<K,V> { fn new() -> Self { Pair { keys: [], values: [] } } fn touch(self, key: K) -> i64 { 0 } fn set(self, key: K, value: V) -> V { value } } fn main() -> i64 { let pair = Pair::new(); pair.touch(\"x\"); pair.set(\"x\", 42) }";
        assert_eq!(run(valid), 42);
        let invalid = "trait Rel<A> {} struct Pair<K,V> { keys: Array<K>, values: Array<V> } impl<K: Rel<V>, V> Pair<K,V> { fn new() -> Self { Pair { keys: [], values: [] } } fn touch(self, key: K) -> i64 { 0 } fn set(self, key: K, value: V) -> V { value } } fn main() -> i64 { let pair = Pair::new(); pair.touch(\"x\"); pair.set(\"x\", 42) }";
        let (module, _) = parse_with_prelude(invalid).unwrap();
        let resolved = resolve_module(module).unwrap();
        let error = lower_program(&resolved.globals).err().unwrap();
        assert!(error.msg.contains("not satisfied"), "{}", error.msg);
    }
    #[test]
    fn inferred_associated_constructor_results_still_enforce_alias_bounds() {
        let source = "trait Bound {} type Values<T: Bound> = Vec<T>; fn main() -> i64 { let mut values = Values::new(); values.push(true); 42 }";
        let (module, _) = parse_with_prelude(source).unwrap();
        let resolved = resolve_module(module).unwrap();
        let error = lower_program(&resolved.globals).err().unwrap();
        assert!(error.msg.contains("not satisfied"), "{}", error.msg);
    }
    #[test]
    fn prelude_option_available_without_declaration() {
        // No `enum Option` declared by the user — it comes from the prelude.
        let src = "fn main() -> i64 { let none: Option<i64> = Option::None; Option::Some(42).unwrap_or(0) + none.unwrap_or(8) }";
        assert_eq!(run(src), 50);
    }

    #[test]
    fn prelude_numeric_helpers() {
        assert_eq!(run("fn main() -> i64 { 3.max(9) + 3.min(9) }"), 12);
        assert_eq!(run("fn main() -> i64 { 2.pow(10) }"), 1024);
        assert_eq!(run("fn main() -> i64 { (0 - 7).abs() }"), 7);
    }

    #[test]
    fn prelude_vec_grows() {
        // The prelude Vec<T> reallocates past its initial capacity of 4.
        let src = "fn main() -> i64 { \
                     let mut v: Vec<i64> = Vec::new(); \
                     let mut i = 0; \
                     while i < 30 { v.push(i * i); i = i + 1; } \
                     v[5] + v[29] + v.len() \
                   }";
        // 25 + 841 + 30 = 896
        assert_eq!(run(src), 896);
    }

    #[test]
    fn prelude_vec_with_capacity_and_set() {
        let src = "fn main() -> i64 { \
                     let mut v: Vec<i64> = Vec::with_capacity(8); \
                     let mut i = 0; \
                     while i < 8 { v.push(i); i = i + 1; } \
                     v[0] = 100; \
                     v[0] + v[7] + v.len() \
                   }";
        // 100 + 7 + 8 = 115
        assert_eq!(run(src), 115);
    }

    #[test]
    fn user_declaration_shadows_prelude() {
        // The user declares their own Option; it must win (no duplicate error).
        let src = "enum Option<T> { None, Some(T) } \
                   fn main() -> i64 { match Option::Some(5) { Option::Some(x) => x, Option::None => 0 } }";
        assert_eq!(run(src), 5);
    }

    // ---- string stdlib (written in gc-rust, over the runtime primitives) ----

    #[test]
    fn prelude_str_trim_and_prefix() {
        let src = "fn main() -> i64 { \
                     let t = \"  hi there  \".trim(); \
                     let p = if t.starts_with(\"hi\") { 1 } else { 0 }; \
                     let e = if t.ends_with(\"there\") { 1 } else { 0 }; \
                     t.len() * 10 + p + e \
                   }";
        // trimmed \"hi there\" len 8 -> 80 + 1 + 1 = 82
        assert_eq!(run(src), 82);
    }

    #[test]
    fn prelude_parse_int() {
        assert_eq!(run("fn main() -> i64 { (\"12345\".parse_i64()).unwrap_or(0) }"), 12345);
        assert_eq!(run("fn main() -> i64 { (\"-42\".parse_i64()).unwrap_or(0) }"), 0 - 42);
        // Malformed -> None -> the default.
        assert_eq!(run("fn main() -> i64 { (\"12x\".parse_i64()).unwrap_or(0 - 1) }"), 0 - 1);
        assert_eq!(run("fn main() -> i64 { (\"\".parse_i64()).unwrap_or(0 - 7) }"), 0 - 7);
    }

    #[test]
    fn prelude_split_and_join_roundtrip() {
        // Split on ',' then join with '|' — count pieces and total length.
        let src = "fn main() -> i64 { \
                     let parts = \"a,bb,ccc\".split(\",\"); \
                     let joined = parts.join(\"|\"); \
                     parts.len() * 100 + joined.len() \
                   }";
        // 3 pieces; \"a|bb|ccc\" len 8 -> 308
        assert_eq!(run(src), 308);
    }

    #[test]
    fn prelude_replace_byte() {
        let src = "fn main() -> i64 { (\"a-b-c\".replace(\"-\", \"__\")).len() }";
        // \"a__b__c\" -> length 7
        assert_eq!(run(src), 7);
    }

    // ---- integer overflow semantics (docs/overflow.md) --------------------

    #[test]
    fn overflow_wraps() {
        // i64::MAX + 1 wraps to i64::MIN — a defined behavior, not UB.
        let src = "fn main() -> i64 { let m = 9223372036854775807; m + 1 }";
        assert_eq!(run(src), i64::MIN);
    }

    #[test]
    fn checked_add_detects_overflow() {
        // checked_add returns None at the boundary, Some otherwise.
        let over = "fn main() -> i64 { if 9223372036854775807.checked_add(1).is_some() { 1 } else { 0 } }";
        assert_eq!(run(over), 0);
        let ok = "fn main() -> i64 { 100.checked_add(200).unwrap_or(0) }";
        assert_eq!(run(ok), 300);
    }

    #[test]
    fn checked_mul_detects_overflow() {
        let over = "fn main() -> i64 { if 9223372036854775807.checked_mul(2).is_some() { 1 } else { 0 } }";
        assert_eq!(run(over), 0);
        let ok = "fn main() -> i64 { 6.checked_mul(7).unwrap_or(0) }";
        assert_eq!(run(ok), 42);
    }

    // ---- Vec completion ----------------------------------------------------

    #[test]
    fn prelude_vec_sort_and_search() {
        let src = "fn main() -> i64 { \
                     let mut v: Vec<i64> = Vec::new(); \
                     v.push(5); v.push(3); v.push(8); v.push(1); \
                     let mut s = v.clone(); s.sort(); \
                     s[0] * 1000 + s[3] * 100 + v.position(|x: i64| x == 8).unwrap() * 10 + v.sum() \
                   }";
        // sorted[0]=1, sorted[3]=8, index_of(8)=2, sum=17 -> 1000+800+20+17 = 1837
        assert_eq!(run(src), 1837);
    }

    #[test]
    fn prelude_vec_pop_last_reverse() {
        let src = "fn main() -> i64 { \
                     let mut v: Vec<i64> = Vec::new(); \
                     v.push(1); v.push(2); v.push(3); \
                     let last = v.last().unwrap_or(0); \
                     let mut rev = v.clone(); rev.reverse(); \
                     let popped = v.pop().unwrap(); \
                     last * 1000 + rev[0] * 100 + popped * 10 + v.len() \
                   }";
        // last=3, rev[0]=3, popped=3, len after pop=2 -> 3000 + 300 + 30 + 2
        assert_eq!(run(src), 3332);
    }

    // ---- HashMap with String keys -------------------------------------------

    #[test]
    fn prelude_mapstr_basic() {
        let src = "fn main() -> i64 { \
                     let mut m: HashMap<String, i64> = HashMap::new(); \
                     m.insert(\"one\", 1); \
                     m.insert(\"two\", 2); \
                     m.insert(\"two\", 22); \
                     let g = m.get(\"two\").unwrap_or(0); \
                     let c = if m.contains_key(\"one\") { 1 } else { 0 }; \
                     let miss = if m.contains_key(\"x\") { 1 } else { 0 }; \
                     g * 100 + m.len() * 10 + c + miss \
                   }";
        // get(two)=22, len=2 (one+two), contains(one)=1, miss=0 -> 2200+20+1 = 2221
        assert_eq!(run(src), 2221);
    }

    #[test]
    fn prelude_iterator_combinators() {
        let src = "fn main() -> i64 { \
                     let mut v: Vec<i64> = Vec::new(); \
                     for i in 1..6 { v.push(i); } \
                     let doubled = v.map(|x: i64| x * 2); \
                     let evens = v.filter(|x: i64| x % 2 == 0); \
                     let total = v.fold(0, |acc: i64, x: i64| acc + x); \
                     doubled.sum() * 1000 + evens.sum() * 100 + total * 1 + v.filter(|x: i64| x > 3).len() \
                   }";
        // doubled sum=30, evens sum=6, fold total=15, count(>3)=2 -> 30000+600+15+2 = 30617
        assert_eq!(run(src), 30617);
    }

    #[test]
    fn prelude_generic_trait_ops() {
        // contains / sort / max via the Eq/Ord traits on i64.
        let src = "fn main() -> i64 { \
                     let mut v: Vec<i64> = Vec::new(); \
                     v.push(30); v.push(10); v.push(20); \
                     let mut s = v.clone(); s.sort(); \
                     let c = if v.contains(20) { 1 } else { 0 }; \
                     s[0] * 100 + s[2] + v.max().unwrap_or(0) + c \
                   }";
        // sorted[0]=10, sorted[2]=30, max=30, contains=1 -> 1000+30+30+1 = 1061
        assert_eq!(run(src), 1061);
    }

    #[test]
    fn prelude_user_type_implements_ord() {
        // A user struct implementing Ord is sorted by the generic `sort`.
        let src = "struct P { level: i64 } \
                   impl Ord for P { \
                     fn cmp(self, other: P) -> Ordering { self.level.cmp(other.level) } \
                   } \
                   fn main() -> i64 { \
                     let mut v: Vec<P> = Vec::new(); \
                     v.push(P { level: 3 }); v.push(P { level: 1 }); v.push(P { level: 2 }); \
                     v.sort(); \
                     v[0].level * 100 + v[1].level * 10 + v[2].level \
                   }";
        // sorted levels: 1, 2, 3 -> 123
        assert_eq!(run(src), 123);
    }

    #[test]
    fn prelude_iterator_any_all() {
        let src = "fn main() -> i64 { \
                     let mut v: Vec<i64> = Vec::new(); \
                     for i in 1..5 { v.push(i); } \
                     let a = if v.any(|x: i64| x > 3) { 1 } else { 0 }; \
                     let b = if v.all(|x: i64| x > 0) { 1 } else { 0 }; \
                     let c = if v.all(|x: i64| x > 2) { 1 } else { 0 }; \
                     a * 100 + b * 10 + c \
                   }";
        // any(>3)=1, all(>0)=1, all(>2)=0 -> 110
        assert_eq!(run(src), 110);
    }

    #[test]
    fn prelude_mapstr_grows_and_rehashes() {
        // Insert past the initial capacity (8) to force at least two rehashes;
        // every key must remain findable, including the earliest insert.
        let src = "fn main() -> i64 { \
                     let mut m: HashMap<String, i64> = HashMap::new(); \
                     m.insert(\"first\", 7); \
                     let mut i = 0; \
                     while i < 60 { m.insert(i.to_string(), i * 10); i = i + 1; } \
                     m.get(\"k\").unwrap_or(0) + \
                       m.get(\"first\").unwrap_or(0) * 1 + \
                       m.get(\"37\").unwrap_or(0) + \
                       m.len() \
                   }";
        // get(k)=0(absent), first=7, get(37)=370, len=61 -> 0+7+370+61 = 438
        assert_eq!(run(src), 438);
    }
}

/// Turn the layout and Core optimizations on or off for this thread: automatic
/// inline (value) layout of small structs and enums, inlining, and scalar
/// replacement. Off, every struct is a heap object as written, which heap
/// inspection and debug builds want.
pub fn set_optimize(on: bool) {
    crate::layout::set_inline_by_default(on);
    crate::inline::set_enabled(on);
}
