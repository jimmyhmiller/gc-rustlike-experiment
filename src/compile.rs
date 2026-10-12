//! The front-to-back compile pipeline with prelude injection.
//!
//! `parse_with_prelude` parses the embedded `prelude.gcr` and prepends every
//! prelude item whose name the user program does NOT already declare. This lets
//! programs use `Option`/`Result` and the prelude helpers without declaring
//! them, while existing programs that DO declare their own (e.g. the examples)
//! keep working — the user's declaration shadows the prelude's.

use crate::ast::{Item, ItemKind, Module};
use crate::core::SourceEntry;
use crate::lexer::{lex, lex_with_source, SourceId};
use crate::parser::{ParseError, parse_module};
use std::path::{Path, PathBuf};

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
                let toks = lex_with_source(&src, sid).map_err(|e| ParseError { msg: e.msg, span: e.span })?;
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
    let src = std::fs::read_to_string(path).map_err(|e| ParseError {
        msg: format!("cannot read `{}`: {}", path.display(), e),
        span: crate::lexer::Span::new(0, 0),
    })?;
    let mut sources: SourceMap = Vec::new();
    add_source(&mut sources, path.display().to_string(), src.clone()); // user = id 0
    let tokens = lex(&src).map_err(|e| ParseError { msg: e.msg, span: e.span })?;
    let mut user = parse_module(&tokens)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    load_file_modules(&mut user.items, dir, &mut sources)?; // mods get ids 1..
    let module = inject_prelude(user, &mut sources); // prelude gets the next id
    Ok((module, sources))
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
        jit_run_i64_gc(&prog, false).unwrap()
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
