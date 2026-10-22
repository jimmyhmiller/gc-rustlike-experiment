//! Recursive-descent parser: token stream → surface [`Module`].
//!
//! Grammar in `docs/language.md`. Pratt-style expression parsing with Rust
//! operator precedence. Errors are single-shot (no recovery in v0): the first
//! error aborts with a span.

use crate::ast::*;
use crate::lexer::{Kw, Span, TokKind, Token};

#[derive(Debug)]
pub struct ParseError {
    pub msg: String,
    pub span: Span,
}

pub fn parse_module(tokens: &[Token]) -> Result<Module, ParseError> {
    // Every module sees the standard macros (`println!`, `vec!`, ...).
    let macros = std::rc::Rc::new(std::cell::RefCell::new(prelude_macros()));
    let mut p = Parser::new(tokens.to_vec(), macros);
    let mut items = Vec::new();
    while !p.at(&TokKind::Eof) {
        items.extend(p.items()?);
    }
    Ok(Module { items })
}

struct Parser {
    /// Owned so `expect_gt` can split a `>>` token in place into a `>` (it
    /// rewrites the current token to `Gt` and leaves the cursor for the next
    /// generic close to consume).
    toks: Vec<Token>,
    pos: usize,
    /// >0 while parsing a context where a trailing `{` struct literal is
    /// forbidden (if/while/match/for scrutinee), so `if x {` reads `x` as the
    /// condition, not `x { … }` as a struct literal.
    no_struct: u32,
    /// Current expression-recursion depth, bounded by `MAX_EXPR_DEPTH` so deeply
    /// nested input (e.g. `((((((…))))))`) yields a clean error instead of a
    /// stack overflow. Every recursive-descent compiler needs this.
    depth: u32,
    /// Items generated alongside the one being parsed (`#[derive]` impls,
    /// the extra paths of a `use a::{b, c};` group).
    pending: Vec<Item>,
    /// While parsing a function's parameters: the anonymous type parameters
    /// their `impl Trait` types introduce.
    impl_params: Option<Vec<TypeParam>>,
    /// `macro_rules!` definitions seen so far (shared with the parsers that
    /// read their expansions), and the current expansion depth.
    macros: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, std::rc::Rc<crate::macros::MacroRules>>>>,
    macro_depth: u32,
    /// Numbers the bindings that range patterns introduce.
    pattern_counter: u32,
    /// The loops being parsed, innermost last (for labels and `break value`),
    /// and the flag checks to place after the statement that contains a loop
    /// some inner `break 'label` or `continue 'label` jumps out of.
    loops: Vec<LoopCtx>,
    pending_checks: Vec<Stmt>,
    loop_counter_next: u32,
    /// The inline modules enclosing the cursor (for `super::` and `self::`).
    module_path: Vec<String>,
    /// The turbofish type arguments the last expression path carried, and
    /// how many segments preceded them.
    last_turbofish: Option<(usize, Vec<Type>)>,
    /// The element bindings of each slice pattern (`[a, .., z]`), keyed by
    /// the `slice#..` name binding the whole value; arms built from the
    /// pattern run them before the guard and body.
    slice_lets: std::collections::HashMap<String, Vec<Stmt>>,
    /// Structs whose type parameters were closure-bounded
    /// (`struct Cache<F: Fn(i64) -> i64>`): those parameters are replaced by
    /// the closure type, and their positions dropped from later `Cache<..>`.
    closure_struct_params: std::collections::HashMap<String, Vec<usize>>,
    /// Destructuring parameters of the function being parsed
    /// (`fn f((a, b): (i64, i64))`): `let (a, b) = param#N;` opens its body.
    param_lets: Vec<Stmt>,
}

/// A loop being parsed. `break value` assigns `loop#N`; a `break 'label` or
/// `continue 'label` from a nested loop sets `brk#N` / `cnt#N` and breaks,
/// and each loop it crosses checks the flag after the inner loop ends.
struct LoopCtx {
    id: u32,
    label: Option<String>,
    /// A labeled block (`'a: { .. }`): only `break 'a` targets it.
    is_block: bool,
    value: bool,
    brk: bool,
    cnt: bool,
    /// (target loop id, is a `continue`) jumps that cross this loop.
    propagate: Vec<(u32, bool)>,
}

/// Expansions nested deeper than this are reported (a macro that expands
/// to itself forever).
const MAX_MACRO_DEPTH: u32 = 64;

/// The names a pattern binds, in order, with their mutability.
fn pattern_names(p: &Pattern, out: &mut Vec<(String, bool)>) {
    match &p.kind {
        PatternKind::Binding { is_mut, name } => out.push((name.clone(), *is_mut)),
        PatternKind::Variant { payload, .. } => for q in payload { pattern_names(q, out) },
        PatternKind::Struct { fields, .. } => for (_, q) in fields { pattern_names(q, out) },
        PatternKind::Tuple(ps) => for q in ps { pattern_names(q, out) },
        PatternKind::At { is_mut, name, pat } => { out.push((name.clone(), *is_mut)); pattern_names(pat, out) }
        PatternKind::Wildcard | PatternKind::Literal(_) => {}
    }
}

impl Parser {
/// `let PAT = e else { .. };` is
/// `let (x, y) = match e { PAT => (x, y), _ => { .. } };`.
fn let_else(&self, alts: Vec<(Pattern, Vec<Expr>)>, ty: Option<Type>, scrutinee: Expr, otherwise: Block, span: Span) -> Stmt {
    let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
    let mut names = Vec::new();
    pattern_names(&alts[0].0, &mut names);
    // A slice pattern's element bindings are the names it introduces.
    let slice_names: Vec<String> = names.iter().filter(|(n, _)| n.starts_with("slice#")).map(|(n, _)| n.clone()).collect();
    names.retain(|(n, _)| !n.starts_with("slice#"));
    for s in &slice_names {
        for st in self.slice_lets.get(s).into_iter().flatten() {
            if let Stmt::Let { pattern, .. } = st { pattern_names(pattern, &mut names); }
        }
    }
    let value = |names: &[(String, bool)]| match names {
        [(n, _)] => ex(ExprKind::Path(Path::single(n.clone(), span))),
        _ => ex(ExprKind::Tuple(names.iter().map(|(n, _)| ex(ExprKind::Path(Path::single(n.clone(), span)))).collect())),
    };
    let mut arms: Vec<MatchArm> = alts.into_iter()
        .map(|(pattern, guards)| self.arm(pattern, guards, None, value(&names), span)).collect();
    arms.push(MatchArm { pattern: Pattern { kind: PatternKind::Wildcard, span }, guard: None, body: ex(ExprKind::Block(otherwise)), span });
    let binding = |(n, m): &(String, bool)| Pattern { kind: PatternKind::Binding { is_mut: *m, name: n.clone() }, span };
    let pattern = match names.as_slice() {
        [] => Pattern { kind: PatternKind::Wildcard, span },
        [one] => binding(one),
        many => Pattern { kind: PatternKind::Tuple(many.iter().map(binding).collect()), span },
    };
    // An annotation names the matched value's type, not the bindings'.
    let scrutinee = match ty {
        Some(t) => ex(ExprKind::Block(Block {
            stmts: vec![Stmt::Let { pattern: Pattern { kind: PatternKind::Binding { is_mut: false, name: "let#else".into() }, span }, ty: Some(t), init: Some(scrutinee), span }],
            tail: Some(ex(ExprKind::Path(Path::single("let#else".into(), span)))),
            span,
        })),
        None => scrutinee,
    };
    Stmt::Let { pattern, ty: None, init: Some(ex(ExprKind::Match { scrutinee, arms })), span }
}
}

/// Slice arms that cover every length make a match exhaustive without a
/// `_` arm: `[] => .., [x] => .., [x, rest @ ..] => ..`. When unguarded,
/// test-free arms give every length below that of a `..` arm (also unguarded
/// and test-free), a final `_` arm that cannot be reached is added.
fn slice_coverage(arms: Vec<(MatchArm, bool)>) -> Vec<MatchArm> {
    let info = |a: &MatchArm, guarded: bool| -> Option<(usize, bool)> {
        if guarded { return None; }
        let PatternKind::Binding { name, .. } = &a.pattern.kind else { return None };
        let parts: Vec<&str> = name.split('#').collect();
        if parts.len() != 5 || parts[0] != "slice" || parts[4] != "0" { return None; }
        Some((parts[2].parse().ok()?, parts[3] == "1"))
    };
    let shapes: Vec<(usize, bool)> = arms.iter().filter_map(|(a, g)| info(a, *g)).collect();
    let exact: std::collections::HashSet<usize> = shapes.iter().filter(|(_, r)| !r).map(|(k, _)| *k).collect();
    let covered = shapes.iter().any(|(k, r)| *r && (0..*k).all(|n| exact.contains(&n)));
    let span = arms.last().map(|(a, _)| a.span);
    let mut out: Vec<MatchArm> = arms.into_iter().map(|(a, _)| a).collect();
    if let (true, Some(span)) = (covered, span) {
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let panic = ex(ExprKind::Call(ex(ExprKind::Path(Path::single("panic".into(), span))), vec![ex(ExprKind::Str("unreachable slice pattern".into()))]));
        out.push(MatchArm { pattern: Pattern { kind: PatternKind::Wildcard, span }, guard: None, body: panic, span });
    }
    out
}

/// `e` with explicit type arguments attached: `Cast(e, marker<tys..>)`.
/// Lowering reads `turbofish#` as a call's own type arguments and
/// `typeargs#` as the arguments of the type an associated function is on.
fn turbofish_marker(e: Expr, marker: &str, tys: Vec<Type>, span: Span) -> Expr {
    let ty = Type { kind: TypeKind::Path(Path::single(marker.to_string(), span), tys), span };
    Expr { kind: std::sync::Arc::new(ExprKind::Cast(e, ty)), span }
}

/// The conjunction of a pattern's range guards and the arm's own guard.
fn conjoin(guards: Vec<Expr>, user: Option<Expr>) -> Option<Expr> {
    guards.into_iter().chain(user).reduce(|a, b| {
        let span = a.span;
        Expr { kind: std::sync::Arc::new(ExprKind::Binary(BinOp::And, a, b)), span }
    })
}

/// Map a path into Rust's standard library onto the flat prelude:
/// `std::collections::HashMap` is `HashMap`, `fmt::Result` is `FmtResult`,
/// and `std::fmt::Display` (or `fmt::Display`) is `Display`.
fn std_path(mut segments: Vec<String>) -> Vec<String> {
    const MODULES: &[&str] = &[
        "collections", "fmt", "cmp", "ops", "string", "vec", "iter", "hash", "convert", "sync",
        "thread", "mem", "result", "option", "boxed", "rc", "default", "marker", "clone", "prelude",
        "v1", "rust_2021", "rust_2024", "hash_map", "borrow",
    ];
    if segments.len() > 1 && matches!(segments[0].as_str(), "std" | "core" | "alloc") {
        segments.remove(0);
        while segments.len() > 1 && MODULES.contains(&segments[0].as_str()) {
            let module = segments.remove(0);
            if module == "fmt" { segments.insert(0, module); break; }
        }
    }
    if segments.len() > 1 && segments[0] == "fmt"
        && matches!(segments[1].as_str(), "Display" | "Debug" | "Formatter" | "Result" | "Error" | "Write")
    {
        segments.remove(0);
        match segments[0].as_str() {
            "Result" => segments[0] = "FmtResult".into(),
            "Error" => segments[0] = "FmtError".into(),
            _ => {}
        }
    }
    if segments.len() == 2 && segments[0] == "cmp" && matches!(segments[1].as_str(), "Ordering" | "PartialEq" | "PartialOrd" | "Ord" | "Eq") {
        segments.remove(0);
    }
    // Rust's `PartialEq` is this language's `Eq` (the `==` trait).
    if segments.len() <= 2 && segments[0] == "PartialEq" {
        segments[0] = "Eq".into();
    }
    segments
}

fn is_fn_trait(name: &str) -> bool {
    matches!(name, "Fn" | "FnMut" | "FnOnce")
}

/// Remove the type parameters bounded by a closure trait (`F: Fn(A) -> R`,
/// inline or in a `where` clause), returning each one's closure type.
fn closure_params(g: &mut Generics) -> Vec<(String, Type)> {
    let closure = |bounds: &[TraitRef]| bounds.iter()
        .find(|b| b.path.last() == "Fn" && b.args.len() == 1 && matches!(b.args[0].kind, TypeKind::Fn(..)))
        .map(|b| b.args[0].clone());
    let mut out = Vec::new();
    for p in &g.params {
        if let Some(t) = closure(&p.bounds) { out.push((p.name.clone(), t)); }
    }
    for w in &g.where_clauses {
        if let TypeKind::Path(path, args) = &w.ty.kind {
            if path.is_single() && args.is_empty() && g.params.iter().any(|p| p.name == path.last()) {
                if let Some(t) = closure(&w.bounds) { out.push((path.last().to_string(), t)); }
            }
        }
    }
    g.params.retain(|p| !out.iter().any(|(n, _)| *n == p.name));
    g.where_clauses.retain(|w| !matches!(&w.ty.kind, TypeKind::Path(path, _) if path.is_single() && out.iter().any(|(n, _)| n == path.last())));
    out
}

/// Replace `Self` in `t` by `me`.
fn replace_self(t: &mut Type, me: &Type) {
    match &mut t.kind {
        TypeKind::SelfType => *t = me.clone(),
        TypeKind::Path(p, args) => {
            if p.is_single() && p.last() == "Self" && args.is_empty() { *t = me.clone(); return; }
            for a in args { replace_self(a, me); }
        }
        TypeKind::Tuple(ts) => for a in ts { replace_self(a, me); },
        TypeKind::Array(e, _) => replace_self(e, me),
        TypeKind::Fn(ps, r) | TypeKind::ExternFn(ps, r) => {
            for a in ps { replace_self(a, me); }
            if let Some(r) = r { replace_self(r, me); }
        }
    }
}

/// Replace each named type parameter in `t` by its type.
fn subst_type(t: &mut Type, map: &[(String, Type)]) {
    match &mut t.kind {
        TypeKind::Path(path, args) => {
            if path.is_single() && args.is_empty() {
                if let Some((_, r)) = map.iter().find(|(n, _)| n == path.last()) {
                    *t = r.clone();
                    return;
                }
            }
            for a in args { subst_type(a, map); }
        }
        TypeKind::Tuple(ts) => for a in ts { subst_type(a, map); },
        TypeKind::Array(e, _) => subst_type(e, map),
        TypeKind::Fn(ps, r) | TypeKind::ExternFn(ps, r) => {
            for a in ps { subst_type(a, map); }
            if let Some(r) = r { subst_type(r, map); }
        }
        TypeKind::SelfType => {}
    }
}

/// One part of a let chain.
enum ChainPart {
    Let(Vec<(Pattern, Vec<Expr>)>, Expr),
    Cond(Expr),
}

/// Tokens rendered as source text, spaced the way `stringify!` does.
fn tokens_text(toks: &[Token]) -> String {
    let mut out = String::new();
    let mut prev: Option<&TokKind> = None;
    for t in toks {
        let text = match &t.kind {
            TokKind::Int(n, s) => format!("{}{}", n, suffix_text(*s)),
            TokKind::Float(f, s) => format!("{:?}{}", f, suffix_text(*s)),
            TokKind::Str(x) => format!("{:?}", x),
            TokKind::Char(c) => format!("{:?}", c),
            TokKind::ByteStr(b) => format!("b{:?}", String::from_utf8_lossy(b)),
            TokKind::Ident(x) => x.clone(),
            TokKind::Lifetime(l) => format!("'{}", l),
            TokKind::Keyword(k) => format!("{:?}", k).to_lowercase().replace("selfvalue", "self").replace("selftype", "Self"),
            TokKind::GroupOpen | TokKind::GroupClose => continue,
            other => punct_text(other).to_string(),
        };
        let tight_before = matches!(t.kind, TokKind::Comma | TokKind::Semi | TokKind::Dot | TokKind::RParen | TokKind::RBracket | TokKind::Question | TokKind::ColonColon)
            || (matches!(t.kind, TokKind::LParen | TokKind::LBracket) && matches!(prev, Some(TokKind::Ident(_)) | Some(TokKind::Not)));
        let tight_after = matches!(prev, Some(TokKind::LParen | TokKind::LBracket | TokKind::Dot | TokKind::ColonColon | TokKind::Not | TokKind::Hash));
        if !out.is_empty() && !tight_before && !tight_after { out.push(' '); }
        out.push_str(&text);
        prev = Some(&t.kind);
    }
    out
}

fn suffix_text(s: crate::lexer::NumSuffix) -> &'static str {
    use crate::lexer::NumSuffix as S;
    match s {
        S::None => "", S::I8 => "i8", S::I16 => "i16", S::I32 => "i32", S::I64 => "i64",
        S::U8 => "u8", S::U16 => "u16", S::U32 => "u32", S::U64 => "u64", S::F32 => "f32", S::F64 => "f64",
    }
}

fn punct_text(k: &TokKind) -> &'static str {
    match k {
        TokKind::LParen => "(", TokKind::RParen => ")", TokKind::LBrace => "{", TokKind::RBrace => "}",
        TokKind::LBracket => "[", TokKind::RBracket => "]", TokKind::Comma => ",", TokKind::Semi => ";",
        TokKind::Colon => ":", TokKind::ColonColon => "::", TokKind::Arrow => "->", TokKind::FatArrow => "=>",
        TokKind::Dot => ".", TokKind::DotDot => "..", TokKind::DotDotEq => "..=", TokKind::Question => "?",
        TokKind::At => "@", TokKind::Hash => "#", TokKind::Dollar => "$", TokKind::Plus => "+", TokKind::Minus => "-",
        TokKind::Star => "*", TokKind::Slash => "/", TokKind::Percent => "%", TokKind::Eq => "=", TokKind::EqEq => "==",
        TokKind::Ne => "!=", TokKind::Lt => "<", TokKind::Le => "<=", TokKind::Gt => ">", TokKind::Ge => ">=",
        TokKind::AndAnd => "&&", TokKind::OrOr => "||", TokKind::Not => "!", TokKind::Amp => "&", TokKind::Pipe => "|",
        TokKind::Caret => "^", TokKind::Shl => "<<", TokKind::Shr => ">>", TokKind::PlusEq => "+=", TokKind::MinusEq => "-=",
        TokKind::StarEq => "*=", TokKind::SlashEq => "/=", TokKind::PercentEq => "%=",
        _ => "",
    }
}

/// The source id the prelude macros are lexed with; tokens from their
/// bodies take the invocation's span.
pub(crate) const PRELUDE_MACRO_SOURCE: crate::lexer::SourceId = 0xFFF0;

thread_local! {
    static PRELUDE_MACRO_TABLE: std::cell::RefCell<Option<std::collections::HashMap<String, std::rc::Rc<crate::macros::MacroRules>>>> = const { std::cell::RefCell::new(None) };
}

/// The standard `macro_rules!` macros (`prelude_macros.gcr`), parsed once.
fn prelude_macros() -> std::collections::HashMap<String, std::rc::Rc<crate::macros::MacroRules>> {
    if let Some(t) = PRELUDE_MACRO_TABLE.with(|t| t.borrow().clone()) { return t; }
    let toks = crate::lexer::lex_with_source(include_str!("prelude_macros.gcr"), PRELUDE_MACRO_SOURCE).expect("prelude macros lex");
    let mut p = Parser::new(toks, Default::default());
    while !p.at(&TokKind::Eof) {
        p.items().expect("prelude macros parse");
    }
    let table = p.macros.borrow().clone();
    PRELUDE_MACRO_TABLE.with(|t| *t.borrow_mut() = Some(table.clone()));
    table
}

/// A parsed `#[name]` or `#[name(arg, ...)]` attribute.
struct Attr {
    name: String,
    args: Vec<(String, Span)>,
    span: Span,
}

/// Maximum expression/block nesting depth before the parser bails with an error.
/// Real programs nest a few dozen deep at most; 128 is well above that yet low
/// enough that the recursive-descent frames stay within a small (2 MB) thread
/// stack, so deeply nested input errors cleanly instead of overflowing.
const MAX_EXPR_DEPTH: u32 = 128;

type PResult<T> = Result<T, ParseError>;

impl Parser {
    fn new(toks: Vec<Token>, macros: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, std::rc::Rc<crate::macros::MacroRules>>>>) -> Parser {
        Parser { toks, pos: 0, no_struct: 0, depth: 0, pending: Vec::new(), impl_params: None, macros, macro_depth: 0, pattern_counter: 0, loops: Vec::new(), pending_checks: Vec::new(), loop_counter_next: 0, module_path: Vec::new(), last_turbofish: None, slice_lets: Default::default(), closure_struct_params: Default::default(), param_lets: Vec::new() }
    }

    // ---- token cursor ------------------------------------------------------
    fn peek(&self) -> &TokKind {
        &self.toks[self.pos].kind
    }
    fn peek_at(&self, n: usize) -> &TokKind {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].kind
    }
    fn span(&self) -> Span {
        self.toks[self.pos].span
    }
    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1)].span
    }
    fn at(&self, k: &TokKind) -> bool {
        self.peek() == k
    }
    fn at_kw(&self, kw: Kw) -> bool {
        matches!(self.peek(), TokKind::Keyword(k) if *k == kw)
    }
    fn bump(&mut self) -> &Token {
        let t = &self.toks[self.pos];
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn eat(&mut self, k: &TokKind) -> bool {
        if self.at(k) { self.bump(); true } else { false }
    }
    fn eat_kw(&mut self, kw: Kw) -> bool {
        if self.at_kw(kw) { self.bump(); true } else { false }
    }
    fn expect(&mut self, k: &TokKind) -> PResult<Span> {
        if self.at(k) {
            let s = self.span();
            self.bump();
            Ok(s)
        } else {
            Err(self.error(format!("expected {:?}, found {:?}", k, self.peek())))
        }
    }
    fn expect_kw(&mut self, kw: Kw) -> PResult<Span> {
        if self.at_kw(kw) {
            let s = self.span();
            self.bump();
            Ok(s)
        } else {
            Err(self.error(format!("expected `{:?}`, found {:?}", kw, self.peek())))
        }
    }
    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            TokKind::Ident(s) => { self.bump(); Ok(s) }
            // `self`/`Self` are sometimes used as idents in paths.
            other => Err(self.error(format!("expected identifier, found {:?}", other))),
        }
    }
    fn error(&self, msg: impl Into<String>) -> ParseError {
        ParseError { msg: msg.into(), span: self.span() }
    }

    // ---- items -------------------------------------------------------------
    /// One source item and anything it generates. A `use` of the standard
    /// library (`use std::fmt;`) generates nothing: the prelude is flat.
    fn items(&mut self) -> PResult<Vec<Item>> {
        let item = self.item()?;
        let mut out: Vec<Item> = item.into_iter().collect();
        out.append(&mut self.pending);
        Ok(out)
    }

    fn item(&mut self) -> PResult<Option<Item>> {
        let start = self.span();
        let attrs = self.attributes()?;
        // `macro_rules! name { ... }` and item-position invocations `name!(..);`.
        if let (TokKind::Ident(name), TokKind::Not) = (self.peek().clone(), self.peek_at(1).clone()) {
            if name == "macro_rules" {
                self.macro_rules()?;
                return Ok(None);
            }
            if self.macros.borrow().contains_key(&name) {
                let items = self.macro_items()?;
                self.pending.extend(items);
                return Ok(None);
            }
        }
        // `#[value]` selects the inline value-type representation on a struct/enum.
        let is_value = attrs.iter().any(|a| a.name == "value");
        let skip = attrs.iter().any(|a| a.name == "test" || (a.name == "cfg" && a.args.iter().any(|(n, _)| n == "off")));
        let derives: Vec<(String, Span)> = attrs.iter().filter(|a| a.name == "derive").flat_map(|a| a.args.clone()).collect();
        let vis = self.visibility()?;
        if is_value && !matches!(self.peek(), TokKind::Keyword(Kw::Struct) | TokKind::Keyword(Kw::Enum)) {
            let span = attrs.iter().find(|a| a.name == "value").unwrap().span;
            return Err(ParseError { msg: "`#[value]` applies only to a struct or enum".into(), span });
        }
        let kind = match self.peek() {
            TokKind::Keyword(Kw::Extern) => ItemKind::Fn(self.extern_fn(vis)?),
            TokKind::Keyword(Kw::Fn) => ItemKind::Fn(self.fn_def(vis)?),
            // `const fn` and `unsafe fn` (and `unsafe impl` / `unsafe trait`,
            // whose `unsafe` only asserts what the programmer checked).
            TokKind::Keyword(Kw::Const) if matches!(self.peek_at(1), TokKind::Keyword(Kw::Fn | Kw::Unsafe)) => ItemKind::Fn(self.qualified_fn(vis)?),
            TokKind::Keyword(Kw::Unsafe) if matches!(self.peek_at(1), TokKind::Keyword(Kw::Fn | Kw::Const)) => ItemKind::Fn(self.qualified_fn(vis)?),
            TokKind::Keyword(Kw::Unsafe) if matches!(self.peek_at(1), TokKind::Keyword(Kw::Impl)) => { self.bump(); ItemKind::Impl(self.impl_block()?) }
            TokKind::Keyword(Kw::Unsafe) if matches!(self.peek_at(1), TokKind::Keyword(Kw::Trait)) => { self.bump(); ItemKind::Trait(self.trait_def(vis)?) }
            TokKind::Keyword(Kw::Struct) => ItemKind::Struct(self.struct_def(vis, is_value)?),
            TokKind::Keyword(Kw::Enum) => ItemKind::Enum(self.enum_def(vis, is_value)?),
            TokKind::Keyword(Kw::Trait) => ItemKind::Trait(self.trait_def(vis)?),
            TokKind::Keyword(Kw::Impl) => {
                let imp = self.impl_block()?;
                // Rust's marker `impl Eq for T {}` (beside its `PartialEq`
                // impl, which is this language's `Eq`) adds nothing.
                if imp.items.is_empty() && imp.trait_ref.as_ref().is_some_and(|t| t.path.last() == "Eq") {
                    return Ok(None);
                }
                ItemKind::Impl(imp)
            }
            TokKind::Keyword(Kw::Type) => ItemKind::TypeAlias(self.type_alias(vis)?),
            TokKind::Keyword(Kw::Const) | TokKind::Keyword(Kw::Static) => {
                ItemKind::Const(self.const_def(vis)?)
            }
            TokKind::Keyword(Kw::Mod) => ItemKind::Mod(self.mod_def(vis)?),
            TokKind::Keyword(Kw::Use) => {
                let mut paths = self.use_decl()?;
                if paths.is_empty() { return Ok(None); }
                let first = paths.remove(0);
                let span = start.to(self.prev_span());
                self.pending.extend(paths.into_iter().map(|p| Item { kind: ItemKind::Use(p), span }));
                ItemKind::Use(first)
            }
            other => return Err(self.error(format!("expected item, found {:?}", other))),
        };
        let span = start.to(self.prev_span());
        if skip {
            self.pending.clear();
            return Ok(None);
        }
        if !derives.is_empty() {
            let names: Vec<String> = derives.iter().map(|(n, _)| n.clone()).collect();
            let sources = crate::derive::derive_impls(&kind, &names)
                .map_err(|msg| ParseError { msg, span: derives[0].1 })?;
            for (trait_name, source) in sources {
                // Generated code reports at the derive name that asked for it.
                // Each derived trait has its own span, so each generated
                // method keeps a distinct source identity.
                let at = derives.iter().find(|(n, _)| n == trait_name || (trait_name == "Eq" && n == "PartialEq"))
                    .map(|(_, s)| *s).unwrap_or(derives[0].1);
                let mut tokens = crate::lexer::lex_with_source(&source, at.source)
                    .map_err(|e| ParseError { msg: format!("derive: {}", e.msg), span: at })?;
                for t in &mut tokens { t.span = at; }
                let mut p = Parser { toks: tokens, pos: 0, no_struct: 0, depth: 0, pending: Vec::new(), impl_params: None, macros: self.macros.clone(), macro_depth: self.macro_depth, pattern_counter: 0, loops: Vec::new(), pending_checks: Vec::new(), loop_counter_next: 0, module_path: Vec::new(), last_turbofish: None, slice_lets: Default::default(), closure_struct_params: Default::default(), param_lets: Vec::new() };
                while !p.at(&TokKind::Eof) {
                    let items = p.items().map_err(|e| ParseError { msg: format!("in derived `{}`: {}", trait_name, e.msg), span: at })?;
                    self.pending.extend(items);
                }
            }
        }
        Ok(Some(Item { kind, span }))
    }

    /// Parse zero or more leading attributes `#[ name ]` / `#[name(args)]`.
    ///
    /// `#[value]` selects the inline representation; `#[derive(..)]` generates
    /// impls; `#[test]` and `#[cfg(test)]` items are skipped (there is no test
    /// harness). Lint and codegen hints (`allow`, `inline`, `must_use`, ...)
    /// have no effect. Any other attribute is an error.
    fn attributes(&mut self) -> PResult<Vec<Attr>> {
        let mut attrs = Vec::new();
        while self.at(&TokKind::Hash) {
            let start = self.span();
            self.bump(); // `#`
            self.expect(&TokKind::LBracket)?;
            let name_span = self.span();
            let name = self.ident()?;
            let mut args = Vec::new();
            match name.as_str() {
                "value" | "test" => {
                    if self.at(&TokKind::LParen) {
                        return Err(self.error(format!("`#[{}]` takes no arguments", name)));
                    }
                }
                // `#[cfg(pred)]`: the item is kept only when `pred` holds.
                "cfg" => {
                    let s = self.span();
                    self.expect(&TokKind::LParen)?;
                    let keep = self.cfg_predicate()?;
                    self.expect(&TokKind::RParen)?;
                    if !keep { args.push(("off".to_string(), s)); }
                }
                "derive" => {
                    self.expect(&TokKind::LParen)?;
                    while !self.at(&TokKind::RParen) {
                        let s = self.span();
                        // `derive(serde::Serialize)` style paths keep their last segment.
                        let mut n = self.ident()?;
                        while self.eat(&TokKind::ColonColon) { n = self.ident()?; }
                        args.push((n, s));
                        if !self.eat(&TokKind::Comma) { break; }
                    }
                    self.expect(&TokKind::RParen)?;
                }
                "allow" | "warn" | "deny" | "forbid" | "expect" | "inline" | "cold" | "must_use" | "doc"
                | "track_caller" | "non_exhaustive" | "rustfmt" | "clippy" | "deprecated"
                // Layout and linking hints with no meaning here, and test
                // harness markers (the items they mark are tests, skipped).
                | "repr" | "macro_export" | "macro_use" | "automatically_derived" | "no_mangle"
                | "should_panic" | "ignore" | "cfg_attr" | "allow_internal_unstable" | "export_name" | "link" => {
                    self.skip_attr_args()?;
                }
                _ => {
                    return Err(ParseError {
                        msg: format!("unknown attribute `#[{}]`", name),
                        span: name_span,
                    });
                }
            }
            self.expect(&TokKind::RBracket)?;
            attrs.push(Attr { name, args, span: start.to(self.prev_span()) });
        }
        Ok(attrs)
    }

    /// A `cfg` predicate: `test` and `feature = ".."` are off; `unix`,
    /// `target_os = ".."`, `target_pointer_width`, and `debug_assertions`
    /// follow the compiling host; `not`, `all`, and `any` combine.
    fn cfg_predicate(&mut self) -> PResult<bool> {
        let name = self.ident()?;
        if self.eat(&TokKind::LParen) {
            let mut values = Vec::new();
            while !self.at(&TokKind::RParen) {
                values.push(self.cfg_predicate()?);
                if !self.eat(&TokKind::Comma) { break; }
            }
            self.expect(&TokKind::RParen)?;
            return match name.as_str() {
                "not" if values.len() == 1 => Ok(!values[0]),
                "all" => Ok(values.iter().all(|v| *v)),
                "any" => Ok(values.iter().any(|v| *v)),
                _ => Err(self.error(format!("unknown `cfg` operator `{name}`"))),
            };
        }
        if self.eat(&TokKind::Eq) {
            let TokKind::Str(value) = self.peek().clone() else { return Err(self.error("expected a string in `cfg`")) };
            self.bump();
            return Ok(match name.as_str() {
                "target_os" => value == std::env::consts::OS,
                "target_family" => value == std::env::consts::FAMILY,
                "target_arch" => value == std::env::consts::ARCH,
                "target_pointer_width" => value == "64",
                "target_endian" => value == if cfg!(target_endian = "little") { "little" } else { "big" },
                _ => false, // features and anything else are not enabled
            });
        }
        Ok(match name.as_str() {
            "unix" => cfg!(unix),
            "windows" => cfg!(windows),
            "debug_assertions" => true,
            _ => false, // `test`, `doc`, `miri`, ...
        })
    }

    /// At `#`: do these attributes mark an item (`#[derive] struct ..`)?
    fn is_item_attr_ahead(&self) -> bool {
        let mut i = 0;
        while matches!(self.peek_at(i), TokKind::Hash) {
            // Skip `#[ .. ]`.
            let Some(end) = crate::macros::tree_end(&self.toks, self.pos + i + 1) else { return false };
            i = end - self.pos;
        }
        matches!(self.peek_at(i), TokKind::Keyword(Kw::Fn | Kw::Struct | Kw::Enum | Kw::Trait | Kw::Impl | Kw::Type | Kw::Const | Kw::Static | Kw::Mod | Kw::Use | Kw::Pub | Kw::Unsafe))
            || matches!(self.peek_at(i), TokKind::Ident(n) if n == "macro_rules")
    }

    /// Outer attributes on a statement, field, variant, parameter, or arm:
    /// returns whether the thing they mark is kept (`#[cfg(..)]`).
    fn inner_attributes(&mut self) -> PResult<bool> {
        if !self.at(&TokKind::Hash) { return Ok(true); }
        let attrs = self.attributes()?;
        Ok(!attrs.iter().any(|a| a.name == "cfg" && a.args.iter().any(|(n, _)| n == "off")))
    }

    /// Skip an ignored attribute's arguments up to (not including) its `]`.
    fn skip_attr_args(&mut self) -> PResult<()> {
        let mut depth = 0i32;
        loop {
            match self.peek() {
                TokKind::Eof => return Err(self.error("unterminated attribute")),
                TokKind::RBracket if depth == 0 => return Ok(()),
                TokKind::LParen | TokKind::LBracket | TokKind::LBrace => depth += 1,
                TokKind::RParen | TokKind::RBracket | TokKind::RBrace => depth -= 1,
                _ => {}
            }
            self.bump();
        }
    }

    /// `const`, `unsafe`, and `async` before `fn`; returns whether `unsafe`.
    /// A `const fn` is an ordinary function here.
    fn fn_qualifiers(&mut self) -> bool {
        let mut is_unsafe = false;
        loop {
            match self.peek() {
                TokKind::Keyword(Kw::Const) if matches!(self.peek_at(1), TokKind::Keyword(Kw::Fn | Kw::Unsafe)) => { self.bump(); }
                TokKind::Keyword(Kw::Unsafe) if matches!(self.peek_at(1), TokKind::Keyword(Kw::Fn | Kw::Const)) => { self.bump(); is_unsafe = true; }
                _ => return is_unsafe,
            }
        }
    }

    fn qualified_fn(&mut self, vis: bool) -> PResult<FnDef> {
        let is_unsafe = self.fn_qualifiers();
        let mut f = self.fn_def(vis)?;
        f.is_unsafe = is_unsafe;
        Ok(f)
    }

    fn fn_def(&mut self, vis: bool) -> PResult<FnDef> {
        let start = self.expect_kw(Kw::Fn)?;
        let name = self.ident()?;
        let (generics, params, has_self, self_is_mut, ret) = self.fn_signature()?;
        let lets = std::mem::take(&mut self.param_lets);
        let mut body = self.block()?;
        if !lets.is_empty() { body.stmts.splice(0..0, lets); }
        crate::capture::share_mutated_captures(&mut body);
        Ok(FnDef { vis, name, generics, params, has_self, self_is_mut, ret, body, is_extern: false, is_unsafe: false, span: start.to(self.prev_span()) })
    }

    /// `<generics>(params) -> ret where ...`. An argument's `impl Trait`
    /// becomes a type parameter, and a parameter bounded by a closure trait
    /// (`F: Fn(i64) -> i64`) is replaced by that closure type.
    fn fn_signature(&mut self) -> PResult<(Generics, Vec<Param>, bool, bool, Option<Type>)> {
        let mut generics = self.generics()?;
        self.expect(&TokKind::LParen)?;
        let outer = self.impl_params.replace(Vec::new());
        let parsed = self.params();
        let impl_params = std::mem::replace(&mut self.impl_params, outer).unwrap_or_default();
        let (mut params, has_self, self_is_mut) = parsed?;
        generics.params.extend(impl_params);
        self.expect(&TokKind::RParen)?;
        let mut ret = if self.eat(&TokKind::Arrow) { Some(self.ty()?) } else { None };
        let mut generics = self.maybe_where(generics)?;
        let closures = closure_params(&mut generics);
        if !closures.is_empty() {
            for p in &mut params { subst_type(&mut p.ty, &closures); }
            if let Some(r) = &mut ret { subst_type(r, &closures); }
        }
        Ok((generics, params, has_self, self_is_mut, ret))
    }

    /// Parse an `extern "C" fn name(params) -> ret;` foreign declaration. The
    /// ABI string must be `"C"`. There is no body — the trailing `;` is required.
    /// See `docs/ffi.md`.
    fn extern_fn(&mut self, vis: bool) -> PResult<FnDef> {
        let start = self.expect_kw(Kw::Extern)?;
        let abi = match self.peek() {
            TokKind::Str(s) => { let s = s.clone(); self.bump(); s }
            other => return Err(self.error(format!("expected ABI string after `extern`, found {:?}", other))),
        };
        if abi != "C" {
            return Err(self.error(format!("unsupported extern ABI {:?} (only \"C\" is supported)", abi)));
        }
        self.expect_kw(Kw::Fn)?;
        let name = self.ident()?;
        let generics = self.generics()?;
        if !generics.params.is_empty() {
            return Err(self.error("extern functions cannot be generic"));
        }
        self.expect(&TokKind::LParen)?;
        let (params, has_self, self_is_mut) = self.params()?;
        self.expect(&TokKind::RParen)?;
        if has_self {
            return Err(self.error("extern functions cannot take `self`"));
        }
        let ret = if self.eat(&TokKind::Arrow) { Some(self.ty()?) } else { None };
        self.expect(&TokKind::Semi)?;
        let body = Block { stmts: Vec::new(), tail: None, span: start.to(self.prev_span()) };
        Ok(FnDef {
            vis, name, generics, params, has_self, self_is_mut, ret, body,
            is_extern: true, is_unsafe: false, span: start.to(self.prev_span()),
        })
    }

    fn params(&mut self) -> PResult<(Vec<Param>, bool, bool)> {
        let mut params = Vec::new();
        let mut has_self = false;
        let mut self_is_mut = false;
        // A receiver may be written `self` or `mut self`.
        if self.at_kw(Kw::Mut) && matches!(self.peek_at(1), TokKind::Keyword(Kw::SelfValue)) {
            self.bump(); // `mut`
            self.bump(); // `self`
            has_self = true;
            self_is_mut = true;
            if !self.at(&TokKind::RParen) {
                self.expect(&TokKind::Comma)?;
            }
        } else if self.at_kw(Kw::SelfValue) {
            self.bump();
            has_self = true;
            if !self.at(&TokKind::RParen) {
                self.expect(&TokKind::Comma)?;
            }
        }
        while !self.at(&TokKind::RParen) {
            if !self.inner_attributes()? {
                // A `#[cfg]`-disabled parameter.
                while !self.at(&TokKind::Comma) && !self.at(&TokKind::RParen) { self.bump(); }
                if !self.eat(&TokKind::Comma) { break; }
                continue;
            }
            let pstart = self.span();
            let simple = matches!(self.peek(), TokKind::Ident(_)) && matches!(self.peek_at(1), TokKind::Colon)
                || matches!(self.peek(), TokKind::Keyword(Kw::Mut)) && matches!(self.peek_at(2), TokKind::Colon);
            if simple {
                let is_mut = self.eat_kw(Kw::Mut);
                let name = self.ident()?;
                self.expect(&TokKind::Colon)?;
                let ty = self.ty()?;
                params.push(Param { is_mut, name, ty, span: pstart.to(self.prev_span()) });
            } else {
                // A pattern parameter: a fresh name, destructured in the body.
                let pattern = self.pattern()?;
                self.expect(&TokKind::Colon)?;
                let ty = self.ty()?;
                let span = pstart.to(self.prev_span());
                self.pattern_counter += 1;
                let name = format!("param#{}", self.pattern_counter);
                let init = Expr { kind: std::sync::Arc::new(ExprKind::Path(Path::single(name.clone(), span))), span };
                self.param_lets.push(Stmt::Let { pattern, ty: None, init: Some(init), span });
                params.push(Param { is_mut: false, name, ty, span });
            }
            if !self.eat(&TokKind::Comma) {
                break;
            }
        }
        Ok((params, has_self, self_is_mut))
    }

    fn struct_def(&mut self, vis: bool, is_value: bool) -> PResult<StructDef> {
        let start = self.span();
        self.expect_kw(Kw::Struct)?;
        let name = self.ident()?;
        let generics = self.generics()?;
        let mut generics = self.maybe_where(generics)?;
        let before: Vec<String> = generics.params.iter().map(|p| p.name.clone()).collect();
        let closures = closure_params(&mut generics);
        if !closures.is_empty() {
            let positions = before.iter().enumerate().filter(|(_, n)| closures.iter().any(|(c, _)| c == *n)).map(|(i, _)| i).collect();
            self.closure_struct_params.insert(name.clone(), positions);
        }
        let body = if self.at(&TokKind::LBrace) {
            self.bump();
            let mut fields = Vec::new();
            while !self.at(&TokKind::RBrace) {
                let keep = self.inner_attributes()?;
                let f = self.field_def()?;
                if keep { fields.push(f); }
                if !self.eat(&TokKind::Comma) {
                    break;
                }
            }
            self.expect(&TokKind::RBrace)?;
            StructBody::Named(fields)
        } else if self.at(&TokKind::LParen) {
            self.bump();
            let mut tys = Vec::new();
            while !self.at(&TokKind::RParen) {
                self.inner_attributes()?;
                self.visibility()?;
                tys.push(self.ty()?);
                if !self.eat(&TokKind::Comma) {
                    break;
                }
            }
            self.expect(&TokKind::RParen)?;
            self.expect(&TokKind::Semi)?;
            StructBody::Tuple(tys)
        } else {
            self.expect(&TokKind::Semi)?;
            StructBody::Unit
        };
        let mut body = body;
        // `Self` in a field type is the struct itself (`Option<Box<Self>>`).
        let me = Type {
            kind: TypeKind::Path(Path::single(name.clone(), start), generics.params.iter()
                .map(|p| Type { kind: TypeKind::Path(Path::single(p.name.clone(), start), vec![]), span: start }).collect()),
            span: start,
        };
        match &mut body {
            StructBody::Named(fields) => for f in fields { replace_self(&mut f.ty, &me); },
            StructBody::Tuple(tys) => for t in tys { replace_self(t, &me); },
            StructBody::Unit => {}
        }
        if !closures.is_empty() {
            match &mut body {
                StructBody::Named(fields) => for f in fields { subst_type(&mut f.ty, &closures); },
                StructBody::Tuple(tys) => for t in tys { subst_type(t, &closures); },
                StructBody::Unit => {}
            }
        }
        Ok(StructDef { vis, is_value, name, generics, body, span: start.to(self.prev_span()) })
    }

    fn field_def(&mut self) -> PResult<FieldDef> {
        let start = self.span();
        let vis = self.visibility()?;
        let name = self.ident()?;
        self.expect(&TokKind::Colon)?;
        let ty = self.ty()?;
        Ok(FieldDef { vis, name, ty, span: start.to(self.prev_span()) })
    }

    fn enum_def(&mut self, vis: bool, is_value: bool) -> PResult<EnumDef> {
        let start = self.span();
        self.expect_kw(Kw::Enum)?;
        let name = self.ident()?;
        let generics = self.generics()?;
        let generics = self.maybe_where(generics)?;
        self.expect(&TokKind::LBrace)?;
        let mut variants = Vec::new();
        while !self.at(&TokKind::RBrace) {
            self.inner_attributes()?;
            let vstart = self.span();
            let vname = self.ident()?;
            let payload = if self.at(&TokKind::LParen) {
                self.bump();
                let mut tys = Vec::new();
                while !self.at(&TokKind::RParen) {
                    tys.push(self.ty()?);
                    if !self.eat(&TokKind::Comma) { break; }
                }
                self.expect(&TokKind::RParen)?;
                VariantPayload::Tuple(tys)
            } else if self.at(&TokKind::LBrace) {
                self.bump();
                let mut fields = Vec::new();
                while !self.at(&TokKind::RBrace) {
                    fields.push(self.field_def()?);
                    if !self.eat(&TokKind::Comma) { break; }
                }
                self.expect(&TokKind::RBrace)?;
                VariantPayload::Named(fields)
            } else {
                VariantPayload::None
            };
            let discriminant = if self.eat(&TokKind::Eq) {
                let negative = self.eat(&TokKind::Minus);
                let TokKind::Int(n, _) = self.peek().clone() else {
                    return Err(self.error("an enum discriminant must be an integer literal"));
                };
                self.bump();
                Some(if negative { (n as i64).wrapping_neg() } else { n as i64 })
            } else { None };
            variants.push(VariantDef { name: vname, payload, discriminant, span: vstart.to(self.prev_span()) });
            if !self.eat(&TokKind::Comma) { break; }
        }
        self.expect(&TokKind::RBrace)?;
        // `Self` in a payload is the enum itself (`Cons(i64, Box<Self>)`).
        let me = Type {
            kind: TypeKind::Path(Path::single(name.clone(), start), generics.params.iter()
                .map(|p| Type { kind: TypeKind::Path(Path::single(p.name.clone(), start), vec![]), span: start }).collect()),
            span: start,
        };
        for v in &mut variants {
            match &mut v.payload {
                VariantPayload::Tuple(tys) => for t in tys { replace_self(t, &me); },
                VariantPayload::Named(fields) => for f in fields { replace_self(&mut f.ty, &me); },
                VariantPayload::None => {}
            }
        }
        Ok(EnumDef { vis, is_value, name, generics, variants, span: start.to(self.prev_span()) })
    }

    fn trait_def(&mut self, vis: bool) -> PResult<TraitDef> {
        let start = self.expect_kw(Kw::Trait)?;
        let name = self.ident()?;
        let generics = self.generics()?;
        let mut supertraits = Vec::new();
        if self.eat(&TokKind::Colon) {
            loop {
                supertraits.push(self.trait_ref()?);
                if !self.eat(&TokKind::Plus) { break; }
            }
        }
        let generics = self.maybe_where(generics)?;
        self.expect(&TokKind::LBrace)?;
        let mut items = Vec::new();
        let mut assoc_defaults = Vec::new();
        while !self.at(&TokKind::RBrace) {
            // `type Output = Self;`: an associated type with a default.
            if self.at_kw(Kw::Type) && matches!(self.peek_at(1), TokKind::Ident(_)) && matches!(self.peek_at(2), TokKind::Eq) {
                self.bump();
                let n = self.ident()?;
                self.expect(&TokKind::Eq)?;
                let t = self.ty()?;
                self.expect(&TokKind::Semi)?;
                assoc_defaults.push((n.clone(), t));
                items.push(TraitItem::AssocType(n));
                continue;
            }
            items.push(self.trait_item()?);
        }
        self.expect(&TokKind::RBrace)?;
        Ok(TraitDef { vis, name, generics, supertraits, items, assoc_defaults, span: start.to(self.prev_span()) })
    }

    fn trait_item(&mut self) -> PResult<TraitItem> {
        // `const ID: i64;` (required) or `const ID: i64 = 0;` (a default).
        if self.at_kw(Kw::Const) {
            let start = self.span();
            self.bump();
            let name = self.ident()?;
            self.expect(&TokKind::Colon)?;
            let ty = self.ty()?;
            let value = if self.eat(&TokKind::Eq) { self.expr()? } else {
                Expr { kind: std::sync::Arc::new(ExprKind::Path(Path::single("const#required".into(), start))), span: start }
            };
            self.expect(&TokKind::Semi)?;
            return Ok(TraitItem::Const(ConstDef { vis: true, name, ty, value, span: start.to(self.prev_span()) }));
        }
        if self.eat_kw(Kw::Type) {
            let n = self.ident()?;
            // Bounds on an associated type (`type Item: Display;`) are not checked.
            if self.eat(&TokKind::Colon) { self.bound_list()?; }
            self.expect(&TokKind::Semi)?;
            return Ok(TraitItem::AssocType(n));
        }
        // A method: parse the signature, then either `;` (required) or a body.
        let is_unsafe = self.fn_qualifiers();
        let start = self.expect_kw(Kw::Fn)?;
        let name = self.ident()?;
        let (generics, params, has_self, self_is_mut, ret) = self.fn_signature()?;
        let lets = std::mem::take(&mut self.param_lets);
        if self.eat(&TokKind::Semi) {
            Ok(TraitItem::Required(FnSig {
                name, generics, params, has_self, self_is_mut, ret, span: start.to(self.prev_span()),
            }))
        } else {
            let mut body = self.block()?;
            if !lets.is_empty() { body.stmts.splice(0..0, lets); }
            crate::capture::share_mutated_captures(&mut body);
            Ok(TraitItem::Provided(FnDef {
                vis: false, name, generics, params, has_self, self_is_mut, ret, body,
                is_extern: false, is_unsafe,
                span: start.to(self.prev_span()),
            }))
        }
    }

    fn impl_block(&mut self) -> PResult<ImplBlock> {
        let start = self.expect_kw(Kw::Impl)?;
        let generics = self.generics()?;
        // `impl Trait for Type` vs `impl Type`. Parse a type; if `for` follows,
        // the parsed thing was the trait ref.
        let first = self.ty()?;
        let (trait_ref, self_ty) = if self.eat_kw(Kw::For) {
            let tr = type_to_trait_ref(first)
                .ok_or_else(|| self.error("expected a trait name before `for`"))?;
            (Some(tr), self.ty()?)
        } else {
            (None, first)
        };
        let mut generics = self.maybe_where(generics)?;
        // `impl<F: Fn(i64) -> i64> Cache<F>`: `F` is the closure type.
        let closures = closure_params(&mut generics);
        self.expect(&TokKind::LBrace)?;
        let mut items = Vec::new();
        let mut assoc_types = Vec::new();
        let mut consts = Vec::new();
        while !self.at(&TokKind::RBrace) {
            let vis = self.visibility()?;
            if self.at_kw(Kw::Const) {
                consts.push(self.const_def(vis)?);
                continue;
            }
            if self.eat_kw(Kw::Type) {
                let name = self.ident()?;
                self.expect(&TokKind::Eq)?;
                let ty = self.ty()?;
                self.expect(&TokKind::Semi)?;
                assoc_types.push((name, ty));
                continue;
            }
            items.push(self.qualified_fn(vis)?);
        }
        self.expect(&TokKind::RBrace)?;
        let mut self_ty = self_ty;
        let mut trait_ref = trait_ref;
        if !closures.is_empty() {
            for f in &mut items {
                for p in &mut f.params { subst_type(&mut p.ty, &closures); }
                if let Some(r) = &mut f.ret { subst_type(r, &closures); }
            }
            // `impl<F: Fn(i64) -> i64> Op for F`: an impl for that closure type.
            subst_type(&mut self_ty, &closures);
            if let Some(tr) = &mut trait_ref { for a in &mut tr.args { subst_type(a, &closures); } }
        }
        Ok(ImplBlock { generics, trait_ref, self_ty, items, assoc_types, consts, span: start.to(self.prev_span()) })
    }

    fn type_alias(&mut self, vis: bool) -> PResult<TypeAlias> {
        let start = self.expect_kw(Kw::Type)?;
        let name = self.ident()?;
        let generics = self.generics()?;
        self.expect(&TokKind::Eq)?;
        let ty = self.ty()?;
        self.expect(&TokKind::Semi)?;
        Ok(TypeAlias { vis, name, generics, ty, span: start.to(self.prev_span()) })
    }

    fn const_def(&mut self, vis: bool) -> PResult<ConstDef> {
        let start = self.span();
        // `const` or `static`.
        self.bump();
        let name = self.ident()?;
        self.expect(&TokKind::Colon)?;
        let ty = self.ty()?;
        self.expect(&TokKind::Eq)?;
        let value = self.expr()?;
        self.expect(&TokKind::Semi)?;
        Ok(ConstDef { vis, name, ty, value, span: start.to(self.prev_span()) })
    }

    fn mod_def(&mut self, vis: bool) -> PResult<ModDef> {
        let start = self.expect_kw(Kw::Mod)?;
        let name = self.ident()?;
        // `mod foo;` — a file module, loaded from disk by the pipeline. Or
        // `mod foo { ... }` — an inline module.
        if self.eat(&TokKind::Semi) {
            return Ok(ModDef { vis, name, items: vec![], inline: false, span: start.to(self.prev_span()) });
        }
        self.expect(&TokKind::LBrace)?;
        let mut items = Vec::new();
        self.module_path.push(name.clone());
        while !self.at(&TokKind::RBrace) {
            match self.items() {
                Ok(more) => items.extend(more),
                Err(e) => { self.module_path.pop(); return Err(e); }
            }
        }
        self.module_path.pop();
        self.expect(&TokKind::RBrace)?;
        Ok(ModDef { vis, name, items, inline: true, span: start.to(self.prev_span()) })
    }

    /// `use a::b;`, `use a::*;`, and groups `use a::{b, c::d, self};`. Paths
    /// into the standard library (`std::`, `core::`, `alloc::`) name prelude
    /// items, which are always in scope, so they produce nothing.
    fn use_decl(&mut self) -> PResult<Vec<UsePath>> {
        let start = self.expect_kw(Kw::Use)?;
        self.eat(&TokKind::ColonColon);
        let mut out = Vec::new();
        self.use_tree(Vec::new(), &mut out)?;
        self.expect(&TokKind::Semi)?;
        let span = start.to(self.prev_span());
        // `crate::`, `super::`, and `self::` prefixes become absolute paths.
        let module_path = self.module_path.clone();
        let out = out.into_iter().map(|mut segments: Vec<String>| {
            match segments[0].as_str() {
                "crate" if segments.len() > 1 => { segments.remove(0); }
                "super" if !module_path.is_empty() && segments.len() > 1 => {
                    let mut base = module_path[..module_path.len() - 1].to_vec();
                    base.extend(segments.drain(1..));
                    segments = base;
                }
                _ => {}
            }
            segments
        });
        Ok(out
            .filter(|segments: &Vec<String>| !matches!(segments[0].as_str(), "std" | "core" | "alloc"))
            .map(|segments| UsePath { segments, span })
            .collect())
    }

    fn use_tree(&mut self, prefix: Vec<String>, out: &mut Vec<Vec<String>>) -> PResult<()> {
        let mut segments = prefix;
        loop {
            if self.eat(&TokKind::LBrace) {
                while !self.at(&TokKind::RBrace) {
                    self.use_tree(segments.clone(), out)?;
                    if !self.eat(&TokKind::Comma) { break; }
                }
                self.expect(&TokKind::RBrace)?;
                return Ok(());
            }
            // A trailing `::*` is a glob import; record it as a `*` segment.
            if self.eat(&TokKind::Star) {
                segments.push("*".to_string());
                break;
            }
            if self.eat_kw(Kw::SelfValue) {
                // `use a::{self}` names `a` itself; `use self::x` is relative.
                if segments.is_empty() && self.eat(&TokKind::ColonColon) {
                    segments.extend(self.module_path.iter().cloned());
                    continue;
                }
                break;
            }
            segments.push(self.ident()?);
            if !self.eat(&TokKind::ColonColon) { break; }
        }
        if !segments.is_empty() { out.push(segments); }
        Ok(())
    }

    // ---- generics ----------------------------------------------------------
    fn generics(&mut self) -> PResult<Generics> {
        let mut g = Generics::default();
        if !self.at(&TokKind::Lt) {
            return Ok(g);
        }
        self.bump(); // <
        while !self.at(&TokKind::Gt) {
            // Lifetime parameters (`'a`, `'a: 'b`) mean nothing without
            // references.
            if let TokKind::Lifetime(_) = self.peek() {
                self.bump();
                if self.eat(&TokKind::Colon) {
                    while matches!(self.peek(), TokKind::Lifetime(_)) {
                        self.bump();
                        if !self.eat(&TokKind::Plus) { break; }
                    }
                }
                if !self.eat(&TokKind::Comma) { break; }
                continue;
            }
            let pstart = self.span();
            // `const N: usize`: a const parameter, instantiated with a
            // type-level number.
            if self.eat_kw(Kw::Const) {
                let name = self.ident()?;
                self.expect(&TokKind::Colon)?;
                self.ty()?;
                g.params.push(TypeParam { name, bounds: vec![], default: None, span: pstart.to(self.prev_span()) });
                if !self.eat(&TokKind::Comma) { break; }
                continue;
            }
            let name = self.ident()?;
            let mut bounds = Vec::new();
            if self.eat(&TokKind::Colon) {
                loop {
                    if let Some(b) = self.bound()? { bounds.push(b); }
                    if !self.eat(&TokKind::Plus) { break; }
                }
            }
            let default = if self.eat(&TokKind::Eq) { Some(self.ty()?) } else { None };
            g.params.push(TypeParam { name, bounds, default, span: pstart.to(self.prev_span()) });
            if !self.eat(&TokKind::Comma) { break; }
        }
        self.expect_gt()?;
        Ok(g)
    }

    fn maybe_where(&mut self, mut g: Generics) -> PResult<Generics> {
        if self.eat_kw(Kw::Where) {
            loop {
                let ty = self.ty()?;
                self.expect(&TokKind::Colon)?;
                let mut bounds = Vec::new();
                loop {
                    if let Some(b) = self.bound()? { bounds.push(b); }
                    if !self.eat(&TokKind::Plus) { break; }
                }
                if !bounds.is_empty() { g.where_clauses.push(WherePredicate { ty, bounds }); }
                if !self.eat(&TokKind::Comma) { break; }
                // A `{` ends the where clause.
                if self.at(&TokKind::LBrace) { break; }
            }
        }
        Ok(g)
    }

    fn trait_ref(&mut self) -> PResult<TraitRef> {
        let start = self.span();
        let path = self.path()?;
        // `Fn(A, B) -> R` (also `FnMut`, `FnOnce`): a closure bound, kept as
        // a `Fn` trait ref whose one argument is the `fn(A, B) -> R` type.
        if path.is_single() && is_fn_trait(path.last()) && self.at(&TokKind::LParen) {
            let sig = self.fn_trait_sig(start)?;
            return Ok(TraitRef { path: Path::single("Fn".into(), path.span), args: vec![sig], bindings: vec![], span: start.to(self.prev_span()) });
        }
        // `<A, B, Item = T>`: type arguments, then associated type bindings.
        let mut args = Vec::new();
        let mut bindings = Vec::new();
        if self.eat(&TokKind::Lt) {
            while !self.at(&TokKind::Gt) && !self.at(&TokKind::Shr) {
                if let TokKind::Lifetime(_) = self.peek() {
                    self.bump();
                    if !self.eat(&TokKind::Comma) { break; }
                    continue;
                }
                if let (TokKind::Ident(name), TokKind::Eq) = (self.peek().clone(), self.peek_at(1).clone()) {
                    self.bump();
                    self.bump();
                    bindings.push((name, self.ty()?));
                } else {
                    args.push(self.ty()?);
                }
                if !self.eat(&TokKind::Comma) { break; }
            }
            self.expect_gt()?;
        }
        Ok(TraitRef { path, args, bindings, span: start.to(self.prev_span()) })
    }

    /// The `(A, B) -> R` after `Fn`, as the type `fn(A, B) -> R`.
    fn fn_trait_sig(&mut self, start: Span) -> PResult<Type> {
        self.expect(&TokKind::LParen)?;
        let mut params = Vec::new();
        while !self.at(&TokKind::RParen) {
            params.push(self.ty()?);
            if !self.eat(&TokKind::Comma) { break; }
        }
        self.expect(&TokKind::RParen)?;
        let ret = if self.eat(&TokKind::Arrow) { Some(Box::new(self.ty()?)) } else { None };
        Ok(Type { kind: TypeKind::Fn(params, ret), span: start.to(self.prev_span()) })
    }

    /// `impl A + B` / `dyn A + B` bounds after the keyword.
    /// One `+`-separated bound. Lifetimes, `Sized`, and `?Sized` constrain
    /// nothing here and are dropped.
    fn bound(&mut self) -> PResult<Option<TraitRef>> {
        if let TokKind::Lifetime(_) = self.peek() {
            self.bump();
            return Ok(None);
        }
        let relaxed = self.eat(&TokKind::Question);
        let b = self.trait_ref()?;
        Ok((!relaxed && b.path.last() != "Sized").then_some(b))
    }

    fn bound_list(&mut self) -> PResult<Vec<TraitRef>> {
        let mut bounds = Vec::new();
        loop {
            if let Some(b) = self.bound()? { bounds.push(b); }
            if !self.eat(&TokKind::Plus) { break; }
        }
        Ok(bounds)
    }

    // ---- types -------------------------------------------------------------
    fn ty(&mut self) -> PResult<Type> {
        let start = self.span();
        let kind = match self.peek() {
            TokKind::LParen => {
                self.bump();
                let mut tys = Vec::new();
                while !self.at(&TokKind::RParen) {
                    tys.push(self.ty()?);
                    if !self.eat(&TokKind::Comma) { break; }
                }
                self.expect(&TokKind::RParen)?;
                // `(T)` is just T; `()` is unit; `(A, B)` is a tuple.
                if tys.len() == 1 {
                    return Ok(tys.into_iter().next().unwrap());
                }
                TypeKind::Tuple(tys)
            }
            TokKind::LBracket => {
                self.bump();
                let elem = Box::new(self.ty()?);
                self.expect(&TokKind::Semi)?;
                let count = Box::new(self.expr()?);
                self.expect(&TokKind::RBracket)?;
                TypeKind::Array(elem, count)
            }
            TokKind::Keyword(Kw::Fn) => {
                self.bump();
                self.expect(&TokKind::LParen)?;
                let mut params = Vec::new();
                while !self.at(&TokKind::RParen) {
                    params.push(self.ty()?);
                    if !self.eat(&TokKind::Comma) { break; }
                }
                self.expect(&TokKind::RParen)?;
                let ret = if self.eat(&TokKind::Arrow) { Some(Box::new(self.ty()?)) } else { None };
                TypeKind::Fn(params, ret)
            }
            // `extern fn(A, B) -> R` — a C callback (function-pointer) type.
            TokKind::Keyword(Kw::Extern) => {
                self.bump();
                self.expect_kw(Kw::Fn)?;
                self.expect(&TokKind::LParen)?;
                let mut params = Vec::new();
                while !self.at(&TokKind::RParen) {
                    params.push(self.ty()?);
                    if !self.eat(&TokKind::Comma) { break; }
                }
                self.expect(&TokKind::RParen)?;
                let ret = if self.eat(&TokKind::Arrow) { Some(Box::new(self.ty()?)) } else { None };
                TypeKind::ExternFn(params, ret)
            }
            // The never type: `fn fail() -> !`.
            TokKind::Not => {
                self.bump();
                TypeKind::Path(Path::single("!".into(), start), vec![])
            }
            // A const generic argument: `Matrix<2, 3>`.
            TokKind::Int(n, _) => {
                let n = *n;
                self.bump();
                TypeKind::Path(Path::single(format!("const#{n}"), start), vec![])
            }
            TokKind::Keyword(Kw::SelfType) if !matches!(self.peek_at(1), TokKind::ColonColon) => {
                self.bump();
                TypeKind::SelfType
            }
            TokKind::Keyword(Kw::SelfType) => TypeKind::Path(self.path()?, vec![]),
            // `<T as Trait>::Name`: the associated type `Name` of `T`.
            TokKind::Lt => {
                self.bump();
                let base = self.ty()?;
                self.expect_kw(Kw::As)?;
                let tr = self.trait_ref()?;
                self.expect_gt()?;
                self.expect(&TokKind::ColonColon)?;
                let name = self.ident()?;
                let first = match &base.kind {
                    TypeKind::Path(p, a) if p.is_single() && a.is_empty() => p.last().to_string(),
                    TypeKind::SelfType => "Self".to_string(),
                    _ => return Err(ParseError { msg: "`<T as Trait>::Name` needs a type parameter or `Self` for `T`".into(), span: start.to(self.prev_span()) }),
                };
                // The trait rides along as a marker argument: `trait#Mul<f64>`.
                let span = start.to(self.prev_span());
                let marker = Type { kind: TypeKind::Path(Path::single(format!("trait#{}", tr.path.segments.join("::")), span), tr.args), span };
                TypeKind::Path(Path { segments: vec![first, name], span }, vec![marker])
            }
            // `impl Fn(A) -> R` and `dyn Fn(A) -> R` are closure types. An
            // argument's `impl Trait` is an anonymous type parameter.
            TokKind::Keyword(Kw::Impl) => {
                self.bump();
                let bounds = self.bound_list()?;
                if let Some(f) = bounds.iter().find(|b| b.path.last() == "Fn" && b.args.len() == 1 && matches!(b.args[0].kind, TypeKind::Fn(..))) {
                    return Ok(f.args[0].clone());
                }
                let span = start.to(self.prev_span());
                let Some(sink) = self.impl_params.as_mut() else {
                    // A result's `impl Trait` is the concrete type the body
                    // returns, found when the function is instantiated. It is
                    // written like a `dyn` type with an `impl#` marker.
                    let mut objects = bounds.into_iter().filter(|b| !matches!(b.path.last(), "Send" | "Sync" | "Unpin" | "Sized"));
                    let Some(object) = objects.next() else {
                        return Err(ParseError { msg: "`impl` needs a trait".into(), span });
                    };
                    let mut path = object.path;
                    let last = path.segments.pop().unwrap();
                    path.segments.push(format!("impl#{}", last));
                    let mut bindings = object.bindings;
                    bindings.sort_by(|a, b| a.0.cmp(&b.0));
                    let mut args = object.args;
                    args.extend(bindings.into_iter().map(|(_, t)| t));
                    return Ok(Type { kind: TypeKind::Path(path, args), span });
                };
                let name = format!("impl#{}", sink.len());
                sink.push(TypeParam { name: name.clone(), bounds, default: None, span });
                TypeKind::Path(Path::single(name, start), vec![])
            }
            TokKind::Ident(w) if w == "dyn" && matches!(self.peek_at(1), TokKind::Ident(_)) => {
                self.bump();
                let bounds = self.bound_list()?;
                if let Some(f) = bounds.iter().find(|b| b.path.last() == "Fn" && b.args.len() == 1 && matches!(b.args[0].kind, TypeKind::Fn(..))) {
                    return Ok(f.args[0].clone());
                }
                // `dyn Trait` is the trait's object type, `dyn#Trait` (see
                // resolve::synthesize_trait_objects). Marker bounds such as
                // `Send` and `Sync` add nothing: every value may be shared.
                let mut objects = bounds.into_iter().filter(|b| !matches!(b.path.last(), "Send" | "Sync" | "Unpin"));
                let (Some(object), None) = (objects.next(), objects.next()) else {
                    return Err(ParseError { msg: "`dyn` needs exactly one trait (plus `Send`/`Sync`)".into(), span: start.to(self.prev_span()) });
                };
                let mut path = object.path;
                let last = path.segments.pop().unwrap();
                path.segments.push(format!("dyn#{}", last));
                // Associated type bindings follow the trait's arguments, in
                // name order (the object type's parameter order).
                let mut bindings = object.bindings;
                bindings.sort_by(|a, b| a.0.cmp(&b.0));
                let mut args = object.args;
                args.extend(bindings.into_iter().map(|(_, t)| t));
                TypeKind::Path(path, args)
            }
            TokKind::Ident(_) => {
                let path = self.path()?;
                let mut args = self.opt_type_args()?;
                if let Some(positions) = self.closure_struct_params.get(path.last()) {
                    for &i in positions.iter().rev() { if i < args.len() { args.remove(i); } }
                }
                TypeKind::Path(path, args)
            }
            other => return Err(self.error(format!("expected type, found {:?}", other))),
        };
        Ok(Type { kind, span: start.to(self.prev_span()) })
    }

    fn opt_type_args(&mut self) -> PResult<Vec<Type>> {
        if !self.at(&TokKind::Lt) {
            return Ok(Vec::new());
        }
        self.bump();
        let mut args = Vec::new();
        while !self.at(&TokKind::Gt) && !self.at(&TokKind::Shr) {
            if let TokKind::Lifetime(_) = self.peek() {
                self.bump();
                if !self.eat(&TokKind::Comma) { break; }
                continue;
            }
            args.push(self.ty()?);
            if !self.eat(&TokKind::Comma) { break; }
        }
        self.expect_gt()?;
        Ok(args)
    }

    /// Consume a `>`, splitting a `>>` token into a single `>` in place when
    /// closing nested generics (`Vec<Vec<T>>`).
    fn expect_gt(&mut self) -> PResult<()> {
        match self.peek() {
            TokKind::Gt => { self.bump(); Ok(()) }
            TokKind::Shr => {
                // Rewrite `>>` to `>` in the owned token stream and leave the
                // cursor so the enclosing generic's close consumes it.
                let sp = self.span();
                self.toks[self.pos].kind = TokKind::Gt;
                let _ = sp;
                Ok(())
            }
            other => Err(self.error(format!("expected `>`, found {:?}", other))),
        }
    }

    // ---- paths -------------------------------------------------------------
    fn path(&mut self) -> PResult<Path> {
        let start = self.span();
        let mut segments = vec![self.path_seg()?];
        loop {
            if self.at(&TokKind::ColonColon)
                && matches!(self.peek_at(1), TokKind::Ident(_) | TokKind::Keyword(Kw::SelfType))
            {
                self.bump();
                segments.push(self.path_seg()?);
            } else if self.at(&TokKind::ColonColon) && matches!(self.peek_at(1), TokKind::Lt) {
                // A turbofish in an expression path: `f::<i64>()` gives the
                // function's type arguments, `Vec::<i64>::new()` the type's.
                self.bump();
                let tys = self.opt_type_args()?;
                self.last_turbofish = Some((segments.len(), tys));
            } else {
                break;
            }
        }
        // `crate::x` is the root module's `x`; `super::x` and `self::x` are
        // relative to the inline module being parsed.
        if segments.len() > 1 {
            match segments[0].as_str() {
                "crate" => { segments.remove(0); }
                "super" if !self.module_path.is_empty() => {
                    let mut base = self.module_path[..self.module_path.len() - 1].to_vec();
                    let mut rest = segments.split_off(1);
                    while rest.len() > 1 && rest[0] == "super" && !base.is_empty() { base.pop(); rest.remove(0); }
                    base.extend(rest);
                    segments = base;
                }
                _ => {}
            }
        }
        Ok(Path { segments: std_path(segments), span: start.to(self.prev_span()) })
    }
    /// `pub`, `pub(crate)`, `pub(super)`, `pub(in path)`: all are `pub`.
    fn visibility(&mut self) -> PResult<bool> {
        if !self.eat_kw(Kw::Pub) { return Ok(false); }
        if self.at(&TokKind::LParen) && matches!(self.peek_at(1), TokKind::Ident(_) | TokKind::Keyword(Kw::SelfValue | Kw::In)) {
            let end = crate::macros::tree_end(&self.toks, self.pos).ok_or_else(|| self.error("unclosed `pub(`"))?;
            self.pos = end;
        }
        Ok(true)
    }

    fn path_seg(&mut self) -> PResult<String> {
        match self.peek().clone() {
            TokKind::Ident(s) => { self.bump(); Ok(s) }
            TokKind::Keyword(Kw::SelfType) => { self.bump(); Ok("Self".to_string()) }
            other => Err(self.error(format!("expected path segment, found {:?}", other))),
        }
    }

    // ---- statements + blocks ----------------------------------------------
    fn block(&mut self) -> PResult<Block> {
        // Blocks recurse (a `{ { ... } }` block-expression chain), so bound them
        // with the same depth budget as expressions to prevent stack overflow.
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err(ParseError {
                msg: format!("block nested too deeply (limit {})", MAX_EXPR_DEPTH),
                span: self.span(),
            });
        }
        let saved_no_struct = std::mem::take(&mut self.no_struct);
        let r = self.block_inner();
        self.no_struct = saved_no_struct;
        self.depth -= 1;
        r
    }

    fn block_inner(&mut self) -> PResult<Block> {
        let start = self.expect(&TokKind::LBrace)?;
        let mut stmts = Vec::new();
        let mut tail = None;
        while !self.at(&TokKind::RBrace) {
            // Flag checks for labeled jumps out of a loop in the last statement.
            stmts.append(&mut self.pending_checks);
            // `#[allow(..)] let x = ..;` (a `#[cfg]`-disabled statement is
            // parsed and dropped).
            if self.at(&TokKind::Hash) && !self.is_item_attr_ahead() {
                let keep = self.inner_attributes()?;
                if !keep {
                    if self.at_kw(Kw::Let) { self.let_stmt()?; } else { self.expr()?; self.eat(&TokKind::Semi); }
                    continue;
                }
            }
            if self.at_kw(Kw::Let) {
                stmts.push(self.let_stmt()?);
                continue;
            }
            // A nested item (`fn helper() {}` inside a function) is hoisted
            // beside the enclosing item; like Rust's, it captures nothing.
            if self.is_item_start() {
                let items = self.items()?;
                self.pending.extend(items);
                continue;
            }
            // A statement that STARTS with a block-like keyword (`if`/`while`/
            // `loop`/`match`/`for`/`{`) is parsed as a self-contained statement
            // — a following `(`/`[`/`.`/`-` begins a NEW statement, it does not
            // postfix onto the block (Rust's statement-vs-expression rule). This
            // is what makes `while c {} (x)` two statements, not a call.
            if self.is_block_stmt_start() {
                let e = self.block_like_stmt()?;
                // It may still be the block's tail value if `}` follows.
                if self.at(&TokKind::RBrace) {
                    tail = Some(e);
                    break;
                }
                self.eat(&TokKind::Semi); // optional trailing `;`
                stmts.push(Stmt::Expr(e));
                continue;
            }
            let e = self.expr()?;
            if self.eat(&TokKind::Semi) {
                stmts.push(Stmt::Expr(e));
            } else if self.at(&TokKind::RBrace) {
                tail = Some(e);
                break;
            } else if expr_is_block_like(&e) {
                // Block-like exprs used as statements need no `;`.
                stmts.push(Stmt::Expr(e));
            } else {
                return Err(self.error("expected `;` or `}` after expression"));
            }
        }
        if !self.pending_checks.is_empty() {
            if let Some(t) = tail.take() { stmts.push(Stmt::Expr(t)); }
            stmts.append(&mut self.pending_checks);
        }
        self.expect(&TokKind::RBrace)?;
        Ok(Block { stmts, tail, span: start.to(self.prev_span()) })
    }

    fn is_block_stmt_start(&self) -> bool {
        matches!(
            self.peek(),
            TokKind::Keyword(Kw::If | Kw::While | Kw::Loop | Kw::Match | Kw::For | Kw::Unsafe) | TokKind::LBrace
        ) || matches!((self.peek(), self.peek_at(1)), (TokKind::Lifetime(_), TokKind::Colon))
    }

    /// Parse a single block-like expression in statement position WITHOUT
    /// postfix continuation (no trailing call/index/field/cast). Used so a
    /// `(`/`[` after the block starts a new statement.
    fn block_like_stmt(&mut self) -> PResult<Expr> {
        if let TokKind::Lifetime(label) = self.peek().clone() {
            let start = self.span();
            self.bump();
            self.expect(&TokKind::Colon)?;
            let _ = start;
            return self.labeled_loop(Some(label));
        }
        match self.peek() {
            TokKind::Keyword(Kw::If) => self.if_expr(),
            TokKind::Keyword(Kw::While) => self.labeled_loop(None),
            TokKind::Keyword(Kw::Loop) => self.labeled_loop(None),
            TokKind::Keyword(Kw::Match) => self.match_expr(),
            TokKind::Keyword(Kw::For) => self.labeled_loop(None),
            TokKind::Keyword(Kw::Unsafe) => self.unsafe_expr(),
            TokKind::LBrace => self.block_expr(),
            _ => unreachable!("block_like_stmt called on non-block-like start"),
        }
    }

    fn is_item_start(&self) -> bool {
        (self.at(&TokKind::Hash) && self.is_item_attr_ahead()) || matches!(
            self.peek(),
            TokKind::Keyword(
                Kw::Fn | Kw::Struct | Kw::Enum | Kw::Trait | Kw::Impl
                    | Kw::Type | Kw::Const | Kw::Static | Kw::Mod | Kw::Use
            )
        ) || matches!((self.peek(), self.peek_at(1)), (TokKind::Ident(n), TokKind::Not) if n == "macro_rules")
          || matches!((self.peek(), self.peek_at(1)), (TokKind::Keyword(Kw::Unsafe), TokKind::Keyword(Kw::Fn | Kw::Impl | Kw::Trait | Kw::Const)))
    }

    fn let_stmt(&mut self) -> PResult<Stmt> {
        let start = self.expect_kw(Kw::Let)?;
        let pstart = self.span();
        let mut alts = self.pattern_alts()?;
        let ty = if self.eat(&TokKind::Colon) { Some(self.ty()?) } else { None };
        let init = if self.eat(&TokKind::Eq) { Some(self.expr()?) } else { None };
        // `let PAT = EXPR else { diverge };`
        if let (Some(scrutinee), true) = (init.clone(), self.at_kw(Kw::Else)) {
            self.bump();
            let otherwise = self.block()?;
            self.expect(&TokKind::Semi)?;
            let span = start.to(self.prev_span());
            return Ok(self.let_else(alts, ty, scrutinee, otherwise, span));
        }
        if alts.len() != 1 || !alts[0].1.is_empty() {
            return Err(ParseError {
                msg: "or-patterns and range patterns are supported in `match`, `if let`, `while let`, and `let ... else`".into(),
                span: pstart.to(self.prev_span()),
            });
        }
        let pattern = alts.pop().unwrap().0;
        self.expect(&TokKind::Semi)?;
        Ok(Stmt::Let { pattern, ty, init, span: start.to(self.prev_span()) })
    }

    // ---- expressions (Pratt) ----------------------------------------------
    fn expr(&mut self) -> PResult<Expr> {
        self.expr_bp(0)
    }

    // Delimiters remove the ambiguity between an outer condition and its body.
    // A no-struct restriction must not leak into call arguments or nested groups.
    fn delimited_expr(&mut self) -> PResult<Expr> {
        let saved = std::mem::take(&mut self.no_struct);
        let result = self.expr();
        self.no_struct = saved;
        result
    }

    fn expr_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        // Bound recursion depth: deeply nested input must error cleanly, never
        // overflow the stack.
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err(ParseError {
                msg: format!("expression nested too deeply (limit {})", MAX_EXPR_DEPTH),
                span: self.span(),
            });
        }
        let r = self.expr_bp_inner(min_bp);
        self.depth -= 1;
        r
    }

    fn expr_bp_inner(&mut self, min_bp: u8) -> PResult<Expr> {
        let start = self.span();
        // A range with no start: `..hi`, `..=hi`, or `..`.
        if matches!(self.peek(), TokKind::DotDot | TokKind::DotDotEq) && min_bp <= 2 {
            let inclusive = matches!(self.peek(), TokKind::DotDotEq);
            self.bump();
            let hi = if self.starts_range_end() { Some(self.expr_bp(3)?) } else { None };
            let span = start.to(self.prev_span());
            return Ok(Expr { kind: std::sync::Arc::new(ExprKind::Range { lo: None, hi, inclusive }), span });
        }
        let mut lhs = self.unary()?;

        loop {
            // assignment (right-assoc, lowest)
            if let Some(op) = self.assign_op() {
                if min_bp > 1 { break; }
                self.bump();
                let value = self.expr_bp(1)?;
                let span = start.to(self.prev_span());
                lhs = Expr { kind: std::sync::Arc::new(ExprKind::Assign { target: lhs, op, value }), span };
                continue;
            }
            // range
            if matches!(self.peek(), TokKind::DotDot | TokKind::DotDotEq) && min_bp <= 2 {
                let inclusive = matches!(self.peek(), TokKind::DotDotEq);
                self.bump();
                let hi = if self.starts_range_end() { Some(self.expr_bp(3)?) } else { None };
                let span = start.to(self.prev_span());
                lhs = Expr {
                    kind: std::sync::Arc::new(ExprKind::Range { lo: Some(lhs), hi, inclusive }),
                    span,
                };
                continue;
            }

            let Some((op, l_bp, r_bp)) = self.bin_op() else { break };
            if l_bp < min_bp { break; }
            self.bump();
            let rhs = self.expr_bp(r_bp)?;
            let span = start.to(self.prev_span());
            lhs = Expr { kind: std::sync::Arc::new(ExprKind::Binary(op, lhs, rhs)), span };
        }
        Ok(lhs)
    }

    fn assign_op(&self) -> Option<Option<BinOp>> {
        Some(match self.peek() {
            TokKind::Eq => None,
            TokKind::PlusEq => Some(BinOp::Add),
            TokKind::MinusEq => Some(BinOp::Sub),
            TokKind::StarEq => Some(BinOp::Mul),
            TokKind::SlashEq => Some(BinOp::Div),
            TokKind::PercentEq => Some(BinOp::Rem),
            _ => return None,
        })
    }

    fn bin_op(&self) -> Option<(BinOp, u8, u8)> {
        // (op, left bp, right bp). Higher binds tighter.
        Some(match self.peek() {
            TokKind::OrOr => (BinOp::Or, 4, 5),
            TokKind::AndAnd => (BinOp::And, 6, 7),
            TokKind::EqEq => (BinOp::Eq, 8, 9),
            TokKind::Ne => (BinOp::Ne, 8, 9),
            TokKind::Lt => (BinOp::Lt, 8, 9),
            TokKind::Le => (BinOp::Le, 8, 9),
            TokKind::Gt => (BinOp::Gt, 8, 9),
            TokKind::Ge => (BinOp::Ge, 8, 9),
            TokKind::Pipe => (BinOp::BitOr, 10, 11),
            TokKind::Caret => (BinOp::BitXor, 12, 13),
            TokKind::Amp => (BinOp::BitAnd, 14, 15),
            TokKind::Shl => (BinOp::Shl, 16, 17),
            TokKind::Shr => (BinOp::Shr, 16, 17),
            TokKind::Plus => (BinOp::Add, 18, 19),
            TokKind::Minus => (BinOp::Sub, 18, 19),
            TokKind::Star => (BinOp::Mul, 20, 21),
            TokKind::Slash => (BinOp::Div, 20, 21),
            TokKind::Percent => (BinOp::Rem, 20, 21),
            _ => return None,
        })
    }

    fn unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let op = match self.peek() {
            TokKind::Minus => Some(UnOp::Neg),
            TokKind::Not => Some(UnOp::Not),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let e = self.unary()?;
            let span = start.to(self.prev_span());
            return Ok(Expr { kind: std::sync::Arc::new(ExprKind::Unary(op, e)), span });
        }
        self.postfix()
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let start = self.span();
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                TokKind::LParen => {
                    self.bump();
                    let args = self.call_args()?;
                    let span = start.to(self.prev_span());
                    e = Expr { kind: std::sync::Arc::new(ExprKind::Call(e, args)), span };
                }
                TokKind::Dot => {
                    self.bump();
                    match self.peek().clone() {
                        TokKind::Ident(name) => {
                            self.bump();
                            // A turbofish: `.collect::<Vec<_>>()` names the
                            // result type, which becomes `collect`'s target.
                            // Other methods infer their type arguments.
                            let mut turbofish = None;
                            if self.at(&TokKind::ColonColon) && matches!(self.peek_at(1), TokKind::Lt) {
                                self.bump();
                                turbofish = Some(self.opt_type_args()?);
                            }
                            if self.at(&TokKind::LParen) {
                                self.bump();
                                let args = self.call_args()?;
                                let span = start.to(self.prev_span());
                                let collect_target = match &turbofish {
                                    Some(tys) if name == "collect" && tys.len() == 1 => Some(tys[0].clone()),
                                    _ => None,
                                };
                                e = Expr {
                                    kind: std::sync::Arc::new(ExprKind::MethodCall {
                                        recv: e, method: name, args, span,
                                    }),
                                    span,
                                };
                                if let Some(target) = collect_target {
                                    e = Expr { kind: std::sync::Arc::new(ExprKind::Cast(e, target)), span };
                                } else if let Some(tys) = turbofish {
                                    e = turbofish_marker(e, "turbofish#", tys, span);
                                }
                            } else {
                                let span = start.to(self.prev_span());
                                e = Expr {
                                    kind: std::sync::Arc::new(ExprKind::Field {
                                        base: e, field: FieldAccess::Named(name),
                                    }),
                                    span,
                                };
                            }
                        }
                        TokKind::Int(n, suffix) => {
                            if suffix != crate::lexer::NumSuffix::None {
                                return Err(self.error("a tuple field index cannot have a numeric suffix"));
                            }
                            let index = u32::try_from(n).map_err(|_| self.error("tuple field index is out of range"))?;
                            self.bump();
                            let span = start.to(self.prev_span());
                            e = Expr {
                                kind: std::sync::Arc::new(ExprKind::Field {
                                    base: e, field: FieldAccess::Tuple(index),
                                }),
                                span,
                            };
                        }
                        other => return Err(self.error(format!("expected field, found {:?}", other))),
                    }
                }
                TokKind::LBracket => {
                    self.bump();
                    let index = self.delimited_expr()?;
                    self.expect(&TokKind::RBracket)?;
                    let span = start.to(self.prev_span());
                    e = Expr { kind: std::sync::Arc::new(ExprKind::Index { base: e, index }), span };
                }
                TokKind::Question => {
                    self.bump();
                    let span = start.to(self.prev_span());
                    e = Expr { kind: std::sync::Arc::new(ExprKind::Try(e)), span };
                }
                TokKind::Keyword(Kw::As) => {
                    self.bump();
                    let t = self.ty()?;
                    let span = start.to(self.prev_span());
                    e = Expr { kind: std::sync::Arc::new(ExprKind::Cast(e, t)), span };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn call_args(&mut self) -> PResult<Vec<Expr>> {
        let mut args = Vec::new();
        while !self.at(&TokKind::RParen) {
            args.push(self.delimited_expr()?);
            if !self.eat(&TokKind::Comma) { break; }
        }
        self.expect(&TokKind::RParen)?;
        Ok(args)
    }

    /// A path in expression position: a variable, constant, function, unit
    /// variant, or struct literal. `Err(e)` is a finished expression (one
    /// carrying turbofish type arguments). Kept out of `primary` so deep
    /// nesting stays within the stack budget.
    #[inline(never)]
    fn path_expr(&mut self, start: Span) -> PResult<Result<ExprKind, Expr>> {
        self.last_turbofish = None;
        let path = self.path_or_self()?;
        if let Some((after, tys)) = self.last_turbofish.take() {
            let span = start.to(self.prev_span());
            let e = Expr { kind: std::sync::Arc::new(ExprKind::Path(path.clone())), span };
            // On the last segment: the function's (or variant's) type
            // arguments. On the one before: the type's.
            let marker = if after == path.segments.len() { "turbofish#" } else { "typeargs#" };
            return Ok(Err(turbofish_marker(e, marker, tys, span)));
        }
        // Struct literal: `Path { ... }` — but only when a struct lit is
        // allowed here (it always is in primary position in v0).
        if self.at(&TokKind::LBrace) && self.looks_like_struct_lit() {
            self.bump();
            let mut fields = Vec::new();
            while !self.at(&TokKind::RBrace) {
                let fstart = self.span();
                // `..base`: the remaining fields come from `base`.
                if self.eat(&TokKind::DotDot) {
                    let base = self.expr()?;
                    fields.push(FieldInit { name: "..".into(), value: Some(base), span: fstart.to(self.prev_span()) });
                    break;
                }
                // `P { 0: a, 1: b }` names a tuple struct's fields.
                let name = match self.peek().clone() {
                    TokKind::Int(n, _) => { self.bump(); n.to_string() }
                    _ => self.ident()?,
                };
                let value = if self.eat(&TokKind::Colon) { Some(self.expr()?) } else { None };
                fields.push(FieldInit { name, value, span: fstart.to(self.prev_span()) });
                if !self.eat(&TokKind::Comma) { break; }
            }
            self.expect(&TokKind::RBrace)?;
            let span = start.to(self.prev_span());
            Ok(Ok(ExprKind::StructLit { path, fields, span }))
        } else {
            Ok(Ok(ExprKind::Path(path)))
        }
    }

    /// `<T as Trait>::name` and `<T>::name` in an expression: the item
    /// `name` of `T` (the trait only names where it comes from).
    #[inline(never)]
    fn qualified_path_expr(&mut self, start: Span) -> PResult<Expr> {
        self.expect(&TokKind::Lt)?;
        let ty = self.ty()?;
        if self.eat_kw(Kw::As) { self.trait_ref()?; }
        self.expect_gt()?;
        let mut rest = Vec::new();
        while self.eat(&TokKind::ColonColon) { rest.push(self.path_seg()?); }
        if rest.is_empty() { return Err(self.error("expected `::name` after `<..>`")); }
        let span = start.to(self.prev_span());
        let (mut segments, args) = match ty.kind {
            TypeKind::Path(p, args) => (p.segments, args),
            TypeKind::SelfType => (vec!["Self".to_string()], vec![]),
            _ => return Err(ParseError { msg: "`<T>::name` needs a named type".into(), span }),
        };
        segments.extend(rest);
        let e = Expr { kind: std::sync::Arc::new(ExprKind::Path(Path { segments, span })), span };
        Ok(if args.is_empty() { e } else { turbofish_marker(e, "typeargs#", args, span) })
    }

    fn primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let kind = match self.peek().clone() {
            TokKind::Int(n, sfx) => { self.bump(); ExprKind::Int(n, sfx) }
            TokKind::Float(f, sfx) => { self.bump(); ExprKind::Float(f, sfx) }
            TokKind::Str(s) => { self.bump(); ExprKind::Str(s) }
            // `b"ab"` is an array of its bytes.
            TokKind::ByteStr(bytes) => {
                self.bump();
                ExprKind::Array(ArrayLit::Elems(bytes.into_iter().map(|b| Expr {
                    kind: std::sync::Arc::new(ExprKind::Int(b as u64, crate::lexer::NumSuffix::U8)), span: start,
                }).collect()))
            }
            TokKind::Char(c) => { self.bump(); ExprKind::Char(c) }
            TokKind::Keyword(Kw::True) => { self.bump(); ExprKind::Bool(true) }
            TokKind::Keyword(Kw::False) => { self.bump(); ExprKind::Bool(false) }
            // `<T as Trait>::f`, `<T>::f`: an item of type `T`.
            TokKind::Lt => return self.qualified_path_expr(start),
            TokKind::Keyword(Kw::If) => return self.if_expr(),
            TokKind::Keyword(Kw::Match) => return self.match_expr(),
            TokKind::Keyword(Kw::While | Kw::Loop | Kw::For) => return self.labeled_loop(None),
            TokKind::Keyword(Kw::Unsafe) => return self.unsafe_expr(),
            TokKind::LBrace => return Ok(self.block_expr()?),
            TokKind::Keyword(Kw::Return) => {
                self.bump();
                let v = if self.starts_expr() { Some(self.expr()?) } else { None };
                ExprKind::Return(v)
            }
            TokKind::Keyword(Kw::Break) => {
                self.bump();
                let label = if let TokKind::Lifetime(l) = self.peek().clone() { self.bump(); Some(l) } else { None };
                let v = if self.starts_expr() { Some(self.expr()?) } else { None };
                return self.jump(label, v, false, start);
            }
            TokKind::Keyword(Kw::Continue) => {
                self.bump();
                let label = if let TokKind::Lifetime(l) = self.peek().clone() { self.bump(); Some(l) } else { None };
                return self.jump(label, None, true, start);
            }
            // `'label: loop/while/for`
            TokKind::Lifetime(label) if matches!(self.peek_at(1), TokKind::Colon) => {
                self.bump();
                self.bump();
                return match self.peek() {
                    TokKind::Keyword(Kw::Loop | Kw::While | Kw::For) | TokKind::LBrace => self.labeled_loop(Some(label)),
                    _ => Err(self.error("a label must be followed by `loop`, `while`, `for`, or a block")),
                };
            }
            TokKind::Pipe | TokKind::OrOr => return self.closure(),
            // `move` closures: captures are always shared GC values or copies.
            TokKind::Ident(w) if w == "move" && matches!(self.peek_at(1), TokKind::Pipe | TokKind::OrOr) => {
                self.bump();
                return self.closure();
            }
            // An expression a macro substituted, in invisible delimiters.
            TokKind::GroupOpen => {
                self.bump();
                let saved = std::mem::take(&mut self.no_struct);
                let inner = self.expr();
                self.no_struct = saved;
                let inner = inner?;
                self.expect(&TokKind::GroupClose)?;
                return Ok(inner);
            }
            TokKind::LParen => {
                self.bump();
                if self.at(&TokKind::RParen) {
                    self.bump();
                    ExprKind::Unit
                } else {
                    let mut elems = vec![self.delimited_expr()?];
                    let mut is_tuple = false;
                    while self.eat(&TokKind::Comma) {
                        is_tuple = true;
                        if self.at(&TokKind::RParen) { break; }
                        elems.push(self.delimited_expr()?);
                    }
                    self.expect(&TokKind::RParen)?;
                    if is_tuple { ExprKind::Tuple(elems) }
                    else { return Ok(elems.into_iter().next().unwrap()); }
                }
            }
            TokKind::LBracket => {
                self.bump();
                if self.at(&TokKind::RBracket) {
                    self.bump();
                    ExprKind::Array(ArrayLit::Elems(Vec::new()))
                } else {
                    let first = self.delimited_expr()?;
                    if self.eat(&TokKind::Semi) {
                        let count = self.delimited_expr()?;
                        self.expect(&TokKind::RBracket)?;
                        ExprKind::Array(ArrayLit::Repeat(Box::new(first), Box::new(count)))
                    } else {
                        let mut elems = vec![first];
                        while self.eat(&TokKind::Comma) {
                            if self.at(&TokKind::RBracket) { break; }
                            elems.push(self.delimited_expr()?);
                        }
                        self.expect(&TokKind::RBracket)?;
                        ExprKind::Array(ArrayLit::Elems(elems))
                    }
                }
            }
            TokKind::Ident(name)
                if matches!(self.peek_at(1), TokKind::Not)
                    && (matches!(self.peek_at(2), TokKind::LParen | TokKind::LBracket)
                        || (matches!(self.peek_at(2), TokKind::LBrace) && self.macros.borrow().contains_key(name.as_str()))) =>
            {
                return self.macro_call(start);
            }
            TokKind::Ident(_) | TokKind::Keyword(Kw::SelfValue) | TokKind::Keyword(Kw::SelfType) => match self.path_expr(start)? {
                Ok(kind) => kind,
                Err(done) => return Ok(done),
            },
            other => return Err(self.error(format!("expected expression, found {:?}", other))),
        };
        Ok(Expr { kind: std::sync::Arc::new(kind), span: start.to(self.prev_span()) })
    }

    // ---- built-in macros ----------------------------------------------------
    // `name!(...)` expands here into ordinary expressions. Temporaries use `#`
    // in their names, which no source identifier can contain.

    fn macro_call(&mut self, start: Span) -> PResult<Expr> {
        let name = self.ident()?;
        self.expect(&TokKind::Not)?;
        // A user `macro_rules!` macro (which may shadow a built-in one).
        if self.macros.borrow().contains_key(&name) {
            let tokens = self.macro_invocation_tokens(&name, start)?;
            return self.parse_expansion_expr(tokens, start);
        }
        let close = if self.eat(&TokKind::LParen) {
            TokKind::RParen
        } else {
            self.expect(&TokKind::LBracket)?;
            TokKind::RBracket
        };
        self.builtin_macro(&name, start, &close)
    }

    /// The compiler's built-in macros: the ones Rust itself cannot express
    /// with `macro_rules!`. The standard macros (`println!`, `vec!`, ...)
    /// are `macro_rules!` in `prelude_macros.gcr`, built on these.
    fn builtin_macro(&mut self, name: &str, start: Span, close: &TokKind) -> PResult<Expr> {
        let lit = |kind: ExprKind, span: Span| Expr { kind: std::sync::Arc::new(kind), span };
        match name {
            // `format_args!("{} {x:>4}", a)`: the formatted `String`.
            "format_args" => {
                let e = self.format_args(close, name)?;
                self.eat(&TokKind::Comma);
                self.expect(close)?;
                Ok(e)
            }
            // `stringify!(a + b)`: the tokens as written ("a + b").
            "stringify" => {
                let begin = self.pos;
                let mut depth = 0i32;
                while !(depth == 0 && self.at(close)) {
                    match self.peek() {
                        TokKind::Eof => return Err(self.error("unclosed `stringify!`")),
                        TokKind::LParen | TokKind::LBracket | TokKind::LBrace | TokKind::GroupOpen => depth += 1,
                        TokKind::RParen | TokKind::RBracket | TokKind::RBrace | TokKind::GroupClose => depth -= 1,
                        _ => {}
                    }
                    self.bump();
                }
                let text = tokens_text(&self.toks[begin..self.pos]);
                self.expect(close)?;
                Ok(lit(ExprKind::Str(text), start.to(self.prev_span())))
            }
            // `concat!("a", 1, true)`: literals joined into one string.
            "concat" => {
                let mut out = String::new();
                while !self.at(close) {
                    let negative = self.eat(&TokKind::Minus);
                    if negative { out.push('-'); }
                    match self.peek().clone() {
                        TokKind::Str(t) => out.push_str(&t),
                        TokKind::Char(c) => out.push(c),
                        TokKind::Int(n, _) => out.push_str(&n.to_string()),
                        TokKind::Float(f, _) => out.push_str(&f.to_string()),
                        TokKind::Keyword(Kw::True) => out.push_str("true"),
                        TokKind::Keyword(Kw::False) => out.push_str("false"),
                        _ => return Err(self.error("`concat!` takes literals")),
                    }
                    self.bump();
                    if !self.eat(&TokKind::Comma) { break; }
                }
                self.expect(close)?;
                Ok(lit(ExprKind::Str(out), start.to(self.prev_span())))
            }
            "line" | "column" => {
                self.expect(close)?;
                let (line, column) = crate::lexer::source_position(start);
                let n = if name == "line" { line } else { column };
                Ok(lit(ExprKind::Int(n as u64, crate::lexer::NumSuffix::U32), start.to(self.prev_span())))
            }
            "file" => {
                self.expect(close)?;
                let path = crate::lexer::source_path(start.source).map(|p| p.display().to_string()).unwrap_or_else(|| "<input>".into());
                Ok(lit(ExprKind::Str(path), start.to(self.prev_span())))
            }
            "module_path" => {
                self.expect(close)?;
                let mut path = vec!["crate".to_string()];
                path.extend(self.module_path.iter().cloned());
                Ok(lit(ExprKind::Str(path.join("::")), start.to(self.prev_span())))
            }
            // `include_str!("file")`: the file's text, relative to this source.
            "include_str" => {
                let TokKind::Str(rel) = self.peek().clone() else { return Err(self.error("`include_str!` takes a string literal path")) };
                self.bump();
                self.expect(close)?;
                let span = start.to(self.prev_span());
                let base = crate::lexer::source_path(start.source).and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| ".".into());
                let path = base.join(&rel);
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| ParseError { msg: format!("`include_str!` cannot read `{}`: {}", path.display(), e), span })?;
                Ok(lit(ExprKind::Str(text), span))
            }
            "compile_error" => {
                let TokKind::Str(msg) = self.peek().clone() else { return Err(self.error("`compile_error!` takes a string literal")) };
                Err(ParseError { msg, span: start })
            }
            _ => Err(ParseError { msg: format!("unknown macro `{}!`", name), span: start }),
        }
    }

    /// `macro_rules! name { (matcher) => { body }; ... }`.
    fn macro_rules(&mut self) -> PResult<()> {
        self.bump(); // macro_rules
        self.expect(&TokKind::Not)?;
        let name = self.ident()?;
        let open = self.span();
        let end = crate::macros::tree_end(&self.toks, self.pos).filter(|_| crate::macros::close_of(self.peek()).is_some())
            .ok_or_else(|| self.error("expected `{` after `macro_rules! name`"))?;
        let body: Vec<Token> = self.toks[self.pos + 1..end - 1].to_vec();
        let brace = matches!(self.peek(), TokKind::LBrace);
        self.pos = end;
        if !brace { self.expect(&TokKind::Semi)?; }
        let mut rules = Vec::new();
        let mut i = 0;
        let fail = |(msg, span): (String, Span)| ParseError { msg, span };
        while i < body.len() {
            let mend = crate::macros::tree_end(&body, i).filter(|_| crate::macros::close_of(&body[i].kind).is_some())
                .ok_or_else(|| ParseError { msg: "expected a `(...)` macro pattern".into(), span: body[i].span })?;
            let matcher = crate::macros::parse_matcher(&body[i + 1..mend - 1]).map_err(fail)?;
            if body.get(mend).map(|t| &t.kind) != Some(&TokKind::FatArrow) {
                return Err(ParseError { msg: "expected `=>` after a macro pattern".into(), span: body.get(mend).map(|t| t.span).unwrap_or(open) });
            }
            let bstart = mend + 1;
            let bend = crate::macros::tree_end(&body, bstart).filter(|_| body.get(bstart).is_some_and(|t| crate::macros::close_of(&t.kind).is_some()))
                .ok_or_else(|| ParseError { msg: "expected a `{...}` macro body".into(), span: body.get(bstart).map(|t| t.span).unwrap_or(open) })?;
            let trans = crate::macros::parse_transcriber(&body[bstart + 1..bend - 1]).map_err(fail)?;
            rules.push((matcher, trans));
            i = bend;
            if body.get(i).map(|t| &t.kind) == Some(&TokKind::Semi) { i += 1; }
        }
        self.macros.borrow_mut().insert(name, std::rc::Rc::new(crate::macros::MacroRules { rules }));
        Ok(())
    }

    /// Expand the invocation at the cursor (`name!` then a delimited group,
    /// the cursor on the group) into tokens.
    fn macro_invocation_tokens(&mut self, name: &str, at: Span) -> PResult<Vec<Token>> {
        if self.macro_depth >= MAX_MACRO_DEPTH {
            return Err(ParseError { msg: format!("macro `{name}!` expands too deeply (limit {MAX_MACRO_DEPTH})"), span: at });
        }
        let end = crate::macros::tree_end(&self.toks, self.pos).filter(|_| crate::macros::close_of(self.peek()).is_some())
            .ok_or_else(|| self.error(format!("expected `(`, `[`, or `{{` after `{name}!`")))?;
        let input: Vec<Token> = self.toks[self.pos + 1..end - 1].to_vec();
        self.pos = end;
        let rules = self.macros.borrow().get(name).cloned().unwrap();
        let macros = self.macros.clone();
        let depth = self.macro_depth;
        let measure = move |kind: &str, toks: &[Token]| -> Option<usize> {
            let mut all = toks.to_vec();
            let eof_span = toks.last().map(|t| t.span).unwrap_or(at);
            all.push(Token { kind: TokKind::Eof, span: eof_span });
            let mut p = Parser { toks: all, pos: 0, no_struct: 0, depth: 0, pending: Vec::new(), impl_params: None, macros: macros.clone(), macro_depth: depth + 1, pattern_counter: 0, loops: Vec::new(), pending_checks: Vec::new(), loop_counter_next: 0, module_path: Vec::new(), last_turbofish: None, slice_lets: Default::default(), closure_struct_params: Default::default(), param_lets: Vec::new() };
            let ok = match kind {
                "expr" => p.delimited_expr().is_ok(),
                "ty" => p.ty().is_ok(),
                // A `pat` fragment may have `|` alternatives (Rust 2021).
                "pat" => p.pattern_alts().is_ok(),
                "pat_param" => p.pattern_one().is_ok(),
                "path" => p.path().is_ok(),
                "block" => matches!(p.peek(), TokKind::LBrace) && p.block().is_ok(),
                "stmt" => p.let_stmt().is_ok() || { p.pos = 0; p.delimited_expr().is_ok() },
                "item" => p.item().is_ok(),
                _ => false,
            };
            (ok && p.pos > 0).then_some(p.pos.min(toks.len()))
        };
        let mut out = crate::macros::expand(name, &rules, &input, &measure, at).map_err(|(msg, span)| ParseError { msg, span })?;
        // A standard macro's own tokens report at its use, as rustc does.
        for t in &mut out { if t.span.source == PRELUDE_MACRO_SOURCE { t.span = at; } }
        Ok(out)
    }

    fn sub_parser(&self, mut tokens: Vec<Token>, at: Span) -> Parser {
        let eof_span = tokens.last().map(|t| t.span).unwrap_or(at);
        tokens.push(Token { kind: TokKind::Eof, span: eof_span });
        // Fresh names from an expansion must not meet the enclosing code's.
        let mut p = Parser::new(tokens, self.macros.clone());
        p.macro_depth = self.macro_depth + 1;
        p.pattern_counter = self.pattern_counter + 1_000_000 * p.macro_depth;
        p.loop_counter_next = self.loop_counter_next + 1_000_000 * p.macro_depth;
        p.module_path = self.module_path.clone();
        p.closure_struct_params = self.closure_struct_params.clone();
        p
    }

    /// An expansion in expression position: its tokens as the body of a block,
    /// so a macro can expand to statements followed by a value.
    fn parse_expansion_expr(&mut self, tokens: Vec<Token>, at: Span) -> PResult<Expr> {
        let mut wrapped = vec![Token { kind: TokKind::LBrace, span: at }];
        wrapped.extend(tokens);
        wrapped.push(Token { kind: TokKind::RBrace, span: at });
        let mut p = self.sub_parser(wrapped, at);
        let block = p.block()?;
        self.pending.append(&mut p.pending);
        if !matches!(p.peek(), TokKind::Eof) {
            return Err(p.error("unexpected tokens after the macro expansion"));
        }
        // A single-expression expansion is that expression.
        if block.stmts.is_empty() {
            if let Some(tail) = block.tail.clone() { return Ok(tail); }
        }
        Ok(Expr { kind: std::sync::Arc::new(ExprKind::Block(block)), span: at })
    }

    /// An invocation in item position: `name!(...);` expanding to items.
    fn macro_items(&mut self) -> PResult<Vec<Item>> {
        let start = self.span();
        let name = self.ident()?;
        self.expect(&TokKind::Not)?;
        let brace = matches!(self.peek(), TokKind::LBrace);
        let tokens = self.macro_invocation_tokens(&name, start)?;
        if !brace { self.expect(&TokKind::Semi)?; }
        let mut p = self.sub_parser(tokens, start);
        let mut items = Vec::new();
        while !p.at(&TokKind::Eof) { items.extend(p.items()?); }
        Ok(items)
    }

    /// Comma-separated macro arguments up to (not including) `close`.
    fn macro_args(&mut self, close: &TokKind) -> PResult<Vec<Expr>> {
        let mut args = Vec::new();
        while !self.at(close) {
            args.push(self.delimited_expr()?);
            if !self.eat(&TokKind::Comma) { break; }
        }
        Ok(args)
    }

    /// `"format string", args...` (the part inside `format_args!(...)`).
    fn format_args(&mut self, close: &TokKind, macro_name: &str) -> PResult<Expr> {
        let fmt_span = self.span();
        let TokKind::Str(fmt) = self.peek().clone() else {
            return Err(self.error(format!("`{}!` needs a string literal format", macro_name)));
        };
        self.bump();
        let args = if self.eat(&TokKind::Comma) { self.macro_args(close)? } else { vec![] };
        expand_format(&fmt, args, fmt_span)
    }



    fn path_or_self(&mut self) -> PResult<Path> {
        // `self::x`: the current inline module's `x`.
        if self.at_kw(Kw::SelfValue) && matches!(self.peek_at(1), TokKind::ColonColon) {
            self.bump();
            self.bump();
            let mut path = self.path()?;
            let mut segments = self.module_path.clone();
            segments.append(&mut path.segments);
            path.segments = segments;
            return Ok(path);
        }
        if self.at_kw(Kw::SelfValue) {
            let s = self.span();
            self.bump();
            return Ok(Path::single("self".to_string(), s));
        }
        self.path()
    }

    /// Heuristic: after a path, `{` begins a struct literal unless we're in a
    /// position where `{` starts a block (handled by callers gating struct lits
    /// out of `if`/`while`/`match` scrutinee via `no_struct` — v0 keeps it
    /// simple and always allows struct lits in primary expressions).
    fn looks_like_struct_lit(&self) -> bool {
        if self.no_struct > 0 {
            return false;
        }
        // `Ident {` or `Ident }` ... we accept `{ ident :` , `{ ident ,`,
        // `{ }`. This distinguishes from a following block in statement
        // position, which the block parser handles before reaching here.
        match self.peek_at(1) {
            TokKind::RBrace | TokKind::DotDot => true,
            TokKind::Ident(_) | TokKind::Int(..) => matches!(self.peek_at(2), TokKind::Colon | TokKind::Comma | TokKind::RBrace),
            _ => false,
        }
    }

    fn closure(&mut self) -> PResult<Expr> {
        let start = self.span();
        let mut params = Vec::new();
        let mut closure_lets = Vec::new();
        if self.eat(&TokKind::OrOr) {
            // empty params `||`
        } else {
            self.expect(&TokKind::Pipe)?;
            while !self.at(&TokKind::Pipe) {
                let pstart = self.span();
                // `|x|`, or a pattern (`|(a, b)|`, `|mut acc|`, `|P { x, .. }|`)
                // bound from a fresh name at the start of the body.
                let simple = matches!(self.peek(), TokKind::Ident(n) if n != "_")
                    && matches!(self.peek_at(1), TokKind::Colon | TokKind::Comma | TokKind::Pipe);
                if simple {
                    let name = self.ident()?;
                    let ty = if self.eat(&TokKind::Colon) { Some(self.ty()?) } else { None };
                    params.push(ClosureParam { name, ty, span: pstart.to(self.prev_span()) });
                } else {
                    // No `|` alternatives here: `|` closes the parameters.
                    let alts = self.pattern_one()?;
                    let pattern = match <[_; 1]>::try_from(alts) {
                        Ok([(p, g)]) if g.is_empty() => p,
                        _ => return Err(ParseError { msg: "a closure parameter cannot use a range pattern".into(), span: pstart.to(self.prev_span()) }),
                    };
                    let ty = if self.eat(&TokKind::Colon) { Some(self.ty()?) } else { None };
                    let span = pstart.to(self.prev_span());
                    self.pattern_counter += 1;
                    let name = format!("param#{}", self.pattern_counter);
                    let init = Expr { kind: std::sync::Arc::new(ExprKind::Path(Path::single(name.clone(), span))), span };
                    closure_lets.push(Stmt::Let { pattern, ty: None, init: Some(init), span });
                    params.push(ClosureParam { name, ty, span });
                }
                if !self.eat(&TokKind::Comma) { break; }
            }
            self.expect(&TokKind::Pipe)?;
        }
        let ret = if self.eat(&TokKind::Arrow) { Some(self.ty()?) } else { None };
        let body = self.expr()?;
        let span = start.to(self.prev_span());
        let body = if closure_lets.is_empty() { body } else {
            let bspan = body.span;
            Expr { kind: std::sync::Arc::new(ExprKind::Block(Block { stmts: closure_lets, tail: Some(body), span: bspan })), span: bspan }
        };
        Ok(Expr { kind: std::sync::Arc::new(ExprKind::Closure { params, ret, body }), span })
    }

    fn block_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        let b = self.block()?;
        Ok(Expr { kind: std::sync::Arc::new(ExprKind::Block(b)), span: start.to(self.prev_span()) })
    }

    fn unsafe_expr(&mut self) -> PResult<Expr> {
        let start = self.expect_kw(Kw::Unsafe)?;
        let body = self.block()?;
        Ok(Expr {
            kind: std::sync::Arc::new(ExprKind::Unsafe(body)),
            span: start.to(self.prev_span()),
        })
    }

    /// A let chain: `let P = e && cond && let Q = f` (Rust 2024), as its
    /// parts. `None` (with the cursor restored) for a condition without a
    /// `let`, or a single `let` (handled as `if let`).
    fn let_chain(&mut self) -> PResult<Option<Vec<ChainPart>>> {
        // Look ahead (without parsing: parsing may split `>>` in place) for
        // `&& let` before the condition's block.
        let mut depth = 0i32;
        let mut i = 0;
        let mut chained_let = false;
        loop {
            match self.peek_at(i) {
                TokKind::Eof => break,
                TokKind::LBrace if depth == 0 => break,
                TokKind::LParen | TokKind::LBracket | TokKind::LBrace | TokKind::GroupOpen => depth += 1,
                TokKind::RParen | TokKind::RBracket | TokKind::RBrace | TokKind::GroupClose => depth -= 1,
                TokKind::AndAnd if depth == 0 => {
                    if matches!(self.peek_at(i + 1), TokKind::Keyword(Kw::Let)) { chained_let = true; }
                }
                _ => {}
            }
            if self.pos + i >= self.toks.len() { break; }
            i += 1;
        }
        let starts_let = self.at_kw(Kw::Let);
        if !chained_let && !(starts_let && self.chain_has_and()) { return Ok(None); }
        let save = self.pos;
        let mut parts = Vec::new();
        let mut lets = 0;
        loop {
            self.no_struct += 1;
            let part = if self.eat_kw(Kw::Let) {
                lets += 1;
                let alts = self.pattern_alts();
                let alts = match alts { Ok(a) => a, Err(e) => { self.no_struct -= 1; return Err(e); } };
                if let Err(e) = self.expect(&TokKind::Eq) { self.no_struct -= 1; return Err(e); }
                // Above `&&`, so the chain's next part is not swallowed.
                let e = self.expr_bp(7);
                self.no_struct -= 1;
                ChainPart::Let(alts, e?)
            } else {
                let e = self.expr_bp(7);
                self.no_struct -= 1;
                ChainPart::Cond(e?)
            };
            parts.push(part);
            if !self.eat(&TokKind::AndAnd) { break; }
        }
        if lets == 0 || (lets == 1 && parts.len() == 1) || !self.at(&TokKind::LBrace) {
            self.pos = save;
            return Ok(None);
        }
        Ok(Some(parts))
    }

    /// After `if let`: does an `&&` follow at the top level before the block
    /// (`if let Some(x) = o && x > 0`)?
    fn chain_has_and(&self) -> bool {
        let mut depth = 0i32;
        let mut i = 0;
        loop {
            match self.peek_at(i) {
                TokKind::Eof => return false,
                TokKind::LBrace if depth == 0 => return false,
                TokKind::LParen | TokKind::LBracket | TokKind::LBrace | TokKind::GroupOpen => depth += 1,
                TokKind::RParen | TokKind::RBracket | TokKind::RBrace | TokKind::GroupClose => depth -= 1,
                TokKind::AndAnd if depth == 0 => return true,
                _ => {}
            }
            if self.pos + i >= self.toks.len() { return false; }
            i += 1;
        }
    }

    /// `if`/`while` over a let chain: nested matches and ifs that fall to
    /// `otherwise` at the first failing part.
    fn chain_expr(&self, parts: Vec<ChainPart>, then: Expr, otherwise: Expr, span: Span) -> Expr {
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let mut acc = then;
        for part in parts.into_iter().rev() {
            acc = match part {
                ChainPart::Cond(c) => ex(ExprKind::If {
                    cond: c,
                    then_branch: Block { stmts: vec![], tail: Some(acc), span },
                    else_branch: Some(otherwise.clone()),
                }),
                ChainPart::Let(alts, scrutinee) => {
                    let mut arms: Vec<MatchArm> = alts.into_iter().map(|(p, g)| self.arm(p, g, None, acc.clone(), span)).collect();
                    arms.push(MatchArm { pattern: Pattern { kind: PatternKind::Wildcard, span }, guard: None, body: otherwise.clone(), span });
                    ex(ExprKind::Match { scrutinee, arms })
                }
            };
        }
        acc
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let start = self.expect_kw(Kw::If)?;
        if let Some(parts) = self.let_chain()? {
            let then_branch = self.block()?;
            let else_branch = self.else_branch()?;
            let span = start.to(self.prev_span());
            let then = Expr { kind: std::sync::Arc::new(ExprKind::Block(then_branch)), span };
            let otherwise = else_branch.unwrap_or(Expr { kind: std::sync::Arc::new(ExprKind::Unit), span });
            return Ok(self.chain_expr(parts, then, otherwise, span));
        }
        if self.eat_kw(Kw::Let) {
            return self.if_let(start);
        }
        let cond = self.expr_no_struct()?;
        let then_branch = self.block()?;
        let else_branch = self.else_branch()?;
        let span = start.to(self.prev_span());
        Ok(Expr { kind: std::sync::Arc::new(ExprKind::If { cond, then_branch, else_branch }), span })
    }

    fn else_branch(&mut self) -> PResult<Option<Expr>> {
        if !self.eat_kw(Kw::Else) {
            return Ok(None);
        }
        if self.at_kw(Kw::If) { Ok(Some(self.if_expr()?)) } else { Ok(Some(self.block_expr()?)) }
    }

    /// `if let PAT = EXPR { then } [else ...]` is
    /// `match EXPR { PAT => { then }, _ => else-or-() }`.
    fn if_let(&mut self, start: Span) -> PResult<Expr> {
        let alts = self.pattern_alts()?;
        self.expect(&TokKind::Eq)?;
        let scrutinee = self.expr_no_struct()?;
        let then_branch = self.block()?;
        let else_branch = self.else_branch()?;
        let span = start.to(self.prev_span());
        let then_span = then_branch.span;
        let otherwise = else_branch.unwrap_or(Expr { kind: std::sync::Arc::new(ExprKind::Unit), span });
        let then = Expr { kind: std::sync::Arc::new(ExprKind::Block(then_branch)), span: then_span };
        let mut arms: Vec<MatchArm> = alts.into_iter()
            .map(|(pattern, guards)| self.arm(pattern, guards, None, then.clone(), span)).collect();
        arms.push(MatchArm { pattern: Pattern { kind: PatternKind::Wildcard, span }, guard: None, body: otherwise, span });
        Ok(Expr { kind: std::sync::Arc::new(ExprKind::Match { scrutinee, arms }), span })
    }

    fn match_expr(&mut self) -> PResult<Expr> {
        let start = self.expect_kw(Kw::Match)?;
        let scrutinee = self.expr_no_struct()?;
        self.expect(&TokKind::LBrace)?;
        let mut arms = Vec::new();
        while !self.at(&TokKind::RBrace) {
            self.inner_attributes()?;
            let astart = self.span();
            let alts = self.pattern_alts()?;
            let guard = if self.eat_kw(Kw::If) { Some(self.expr()?) } else { None };
            self.expect(&TokKind::FatArrow)?;
            let body = self.expr()?;
            self.eat(&TokKind::Comma);
            // Each alternative is its own arm with the same body.
            for (pattern, guards) in alts {
                let a = self.arm(pattern, guards, guard.clone(), body.clone(), astart.to(self.prev_span()));
                arms.push((a, guard.is_some()));
            }
        }
        self.expect(&TokKind::RBrace)?;
        let span = start.to(self.prev_span());
        Ok(Expr { kind: std::sync::Arc::new(ExprKind::Match { scrutinee, arms: slice_coverage(arms) }), span })
    }

    fn while_expr(&mut self) -> PResult<Expr> {
        let start = self.expect_kw(Kw::While)?;
        if let Some(parts) = self.let_chain()? {
            let body = self.block()?;
            let span = start.to(self.prev_span());
            let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
            let chained = self.chain_expr(parts, ex(ExprKind::Block(body)), ex(ExprKind::Break(None)), span);
            return Ok(ex(ExprKind::Loop { body: Block { stmts: vec![], tail: Some(chained), span } }));
        }
        if self.eat_kw(Kw::Let) {
            // `while let PAT = EXPR { body }` is
            // `loop { match EXPR { PAT => { body }, _ => break } }`.
            let alts = self.pattern_alts()?;
            self.expect(&TokKind::Eq)?;
            let scrutinee = self.expr_no_struct()?;
            let body = self.block()?;
            let span = start.to(self.prev_span());
            let body_span = body.span;
            let body = Expr { kind: std::sync::Arc::new(ExprKind::Block(body)), span: body_span };
            let mut arms: Vec<MatchArm> = alts.into_iter()
                .map(|(pattern, guards)| self.arm(pattern, guards, None, body.clone(), span)).collect();
            arms.extend([
                MatchArm {
                    pattern: Pattern { kind: PatternKind::Wildcard, span },
                    guard: None,
                    body: Expr { kind: std::sync::Arc::new(ExprKind::Break(None)), span },
                    span,
                },
            ]);
            let matched = Expr { kind: std::sync::Arc::new(ExprKind::Match { scrutinee, arms }), span };
            let body = Block { stmts: vec![], tail: Some(matched), span };
            return Ok(Expr { kind: std::sync::Arc::new(ExprKind::Loop { body }), span });
        }
        let cond = self.expr_no_struct()?;
        let body = self.block()?;
        let span = start.to(self.prev_span());
        Ok(Expr { kind: std::sync::Arc::new(ExprKind::While { cond, body }), span })
    }

    /// A `loop`, `while`, or `for`, with its label. Labeled jumps and
    /// `break value` are rewritten here into flags and a result variable.
    fn labeled_loop(&mut self, label: Option<String>) -> PResult<Expr> {
        let start = self.span();
        self.loop_counter_next += 1;
        let id = self.loop_counter_next;
        let is_block = matches!(self.peek(), TokKind::LBrace);
        self.loops.push(LoopCtx { id, label, is_block, value: false, brk: false, cnt: false, propagate: Vec::new() });
        let parsed = match self.peek() {
            TokKind::Keyword(Kw::Loop) => self.loop_expr(),
            TokKind::Keyword(Kw::While) => self.while_expr(),
            // `'a: { body }` is `'a: loop { break 'a { body } }`.
            TokKind::LBrace => self.block_expr().map(|b| {
                let span = b.span;
                let brk = Expr { kind: std::sync::Arc::new(ExprKind::Break(Some(b))), span };
                Expr { kind: std::sync::Arc::new(ExprKind::Loop { body: Block { stmts: vec![], tail: Some(brk), span } }), span }
            }),
            _ => self.for_expr(),
        };
        let ctx = self.loops.pop().unwrap();
        let lp = parsed?;
        let span = start.to(self.prev_span());
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let var = |n: String| ex(ExprKind::Path(Path::single(n, span)));
        // Checks the enclosing statement needs for jumps that cross this loop.
        let mut propagate = ctx.propagate.clone();
        propagate.sort();
        propagate.dedup();
        for (target, is_continue) in propagate {
            let flag = if is_continue { format!("cnt#{target}") } else { format!("brk#{target}") };
            let here = self.loops.last().is_some_and(|l| l.id == target);
            let action = if here && is_continue {
                Block { stmts: vec![Stmt::Expr(ex(ExprKind::Assign { target: var(flag.clone()), op: None, value: ex(ExprKind::Bool(false)) })), Stmt::Expr(ex(ExprKind::Continue))], tail: None, span }
            } else {
                Block { stmts: vec![Stmt::Expr(ex(ExprKind::Break(None)))], tail: None, span }
            };
            if !here {
                if let Some(outer) = self.loops.last_mut() { outer.propagate.push((target, is_continue)); }
            }
            self.pending_checks.push(Stmt::Expr(ex(ExprKind::If { cond: var(flag), then_branch: action, else_branch: None })));
        }
        if !ctx.value && !ctx.brk && !ctx.cnt { return Ok(lp); }
        let let_ = |name: String, init: Option<Expr>| Stmt::Let {
            pattern: Pattern { kind: PatternKind::Binding { is_mut: true, name }, span },
            ty: None,
            init,
            span,
        };
        let mut stmts = Vec::new();
        // A labeled `break 'l value` from an inner loop stores the value
        // here; its type comes from that assignment.
        if ctx.value { stmts.push(let_(format!("loop#{id}"), None)); }
        if ctx.brk { stmts.push(let_(format!("brk#{id}"), Some(ex(ExprKind::Bool(false))))); }
        if ctx.cnt { stmts.push(let_(format!("cnt#{id}"), Some(ex(ExprKind::Bool(false))))); }
        stmts.push(Stmt::Expr(lp));
        let tail = ctx.value.then(|| var(format!("loop#{id}")));
        Ok(ex(ExprKind::Block(Block { stmts, tail, span })))
    }

    /// `break ['label] [value]` / `continue ['label]`.
    fn jump(&mut self, label: Option<String>, value: Option<Expr>, is_continue: bool, start: Span) -> PResult<Expr> {
        let span = start.to(self.prev_span());
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let var = |n: String| ex(ExprKind::Path(Path::single(n, span)));
        let target = match &label {
            Some(l) => match self.loops.iter().rposition(|c| c.label.as_deref() == Some(l.as_str())) {
                Some(i) => i,
                None => return Err(ParseError { msg: format!("no enclosing loop is labeled `'{l}`"), span }),
            },
            // An unlabeled jump targets the innermost loop, not a labeled block.
            None => match self.loops.iter().rposition(|c| !c.is_block) {
                Some(i) => i,
                // Reported by lowering ("break outside loop").
                None => return Ok(ex(if is_continue { ExprKind::Continue } else { ExprKind::Break(value) })),
            },
        };
        let innermost = target == self.loops.len() - 1;
        let tid = self.loops[target].id;
        // The innermost loop takes `break value` directly.
        if innermost {
            return Ok(ex(if is_continue { ExprKind::Continue } else { ExprKind::Break(value) }));
        }
        let mut stmts = Vec::new();
        if let Some(v) = value {
            self.loops[target].value = true;
            stmts.push(Stmt::Expr(ex(ExprKind::Assign { target: var(format!("loop#{tid}")), op: None, value: v })));
        }
        {
            let flag = if is_continue { self.loops[target].cnt = true; format!("cnt#{tid}") } else { self.loops[target].brk = true; format!("brk#{tid}") };
            stmts.push(Stmt::Expr(ex(ExprKind::Assign { target: var(flag), op: None, value: ex(ExprKind::Bool(true)) })));
            stmts.push(Stmt::Expr(ex(ExprKind::Break(None))));
            self.loops.last_mut().unwrap().propagate.push((tid, is_continue));
        }
        if stmts.len() == 1 {
            if let Stmt::Expr(e) = stmts.pop().unwrap() { return Ok(e); }
            unreachable!()
        }
        Ok(ex(ExprKind::Block(Block { stmts, tail: None, span })))
    }

    fn loop_expr(&mut self) -> PResult<Expr> {
        let start = self.expect_kw(Kw::Loop)?;
        let body = self.block()?;
        let span = start.to(self.prev_span());
        Ok(Expr { kind: std::sync::Arc::new(ExprKind::Loop { body }), span })
    }

    fn for_expr(&mut self) -> PResult<Expr> {
        let start = self.expect_kw(Kw::For)?;
        let pat = self.pattern()?;
        self.expect_kw(Kw::In)?;
        let iter = self.expr_no_struct()?;
        let body = self.block()?;
        let span = start.to(self.prev_span());
        Ok(Expr { kind: std::sync::Arc::new(ExprKind::For { pat, iter, body }), span })
    }

    /// Parse an expression but forbid a trailing struct literal at the top
    /// level (so `if x { }` parses `x` as the cond, not `x { }` as a struct
    /// lit). Implemented with a flag.
    fn expr_no_struct(&mut self) -> PResult<Expr> {
        self.no_struct += 1;
        let r = self.expr();
        self.no_struct -= 1;
        r
    }

    /// After `..`: does an upper bound follow? Not a `{` where a struct
    /// literal is not allowed (`for i in 5.. { body }`).
    fn starts_range_end(&self) -> bool {
        self.starts_expr() && !(self.no_struct > 0 && self.at(&TokKind::LBrace))
    }

    fn starts_expr(&self) -> bool {
        matches!(
            self.peek(),
            TokKind::Int(..) | TokKind::Float(..) | TokKind::Str(_) | TokKind::Char(_) | TokKind::ByteStr(_) | TokKind::GroupOpen
                | TokKind::Ident(_) | TokKind::LParen | TokKind::LBracket | TokKind::LBrace
                | TokKind::Minus | TokKind::Not | TokKind::Pipe | TokKind::OrOr
                | TokKind::Keyword(
                    Kw::True | Kw::False | Kw::If | Kw::Match | Kw::While | Kw::Loop
                        | Kw::For | Kw::Return | Kw::Break | Kw::Continue | Kw::Unsafe
                        | Kw::SelfValue | Kw::SelfType
                )
        )
    }

    // ---- patterns ----------------------------------------------------------
    /// A pattern with no alternatives or ranges (`let`, `for`, parameters).
    fn pattern(&mut self) -> PResult<Pattern> {
        let start = self.span();
        let mut alts = self.pattern_alts()?;
        if alts.len() != 1 || !alts[0].1.is_empty() {
            return Err(ParseError {
                msg: "or-patterns and range patterns are supported in `match`, `if let`, and `while let`".into(),
                span: start.to(self.prev_span()),
            });
        }
        Ok(alts.pop().unwrap().0)
    }

    /// A pattern that may contain `|` alternatives, ranges (`1..=9`,
    /// `'a'..='z'`), negative literals, and `name @ range`. It expands to one
    /// plain pattern per alternative, each with the guard conditions its
    /// ranges need (tested against fresh `pat#N` bindings).
    fn pattern_alts(&mut self) -> PResult<Vec<(Pattern, Vec<Expr>)>> {
        self.eat(&TokKind::Pipe);
        let mut out = self.pattern_one()?;
        while self.eat(&TokKind::Pipe) {
            out.extend(self.pattern_one()?);
        }
        Ok(out)
    }

    /// The literal at the cursor as an expression, if it starts one
    /// (`3`, `-3`, `'a'`).
    fn literal_pattern_expr(&mut self) -> PResult<Option<Expr>> {
        let start = self.span();
        let ex = |kind: ExprKind, span: Span| Expr { kind: std::sync::Arc::new(kind), span };
        Ok(Some(match self.peek().clone() {
            TokKind::Minus if matches!(self.peek_at(1), TokKind::Int(..) | TokKind::Float(..)) => {
                self.bump();
                let lit = match self.peek().clone() {
                    TokKind::Int(n, sfx) => ExprKind::Int(n, sfx),
                    TokKind::Float(f, sfx) => ExprKind::Float(f, sfx),
                    _ => unreachable!(),
                };
                self.bump();
                let span = start.to(self.prev_span());
                ex(ExprKind::Unary(UnOp::Neg, ex(lit, span)), span)
            }
            TokKind::Int(n, sfx) => { self.bump(); ex(ExprKind::Int(n, sfx), start) }
            TokKind::Char(c) => { self.bump(); ex(ExprKind::Char(c), start) }
            // A constant path ending a range: `0..=i64::MAX`.
            TokKind::Ident(_) if matches!(self.peek_at(1), TokKind::ColonColon) => {
                let path = self.path()?;
                ex(ExprKind::Path(path), start.to(self.prev_span()))
            }
            _ => return Ok(None),
        }))
    }

    /// Range and negative-literal patterns: a fresh binding plus its guard.
    fn guarded_literal_pattern(&mut self, name: Option<String>) -> PResult<Option<(Pattern, Vec<Expr>)>> {
        let start = self.span();
        let save = self.pos;
        let negative = matches!(self.peek(), TokKind::Minus);
        let lo = match self.literal_pattern_expr()? {
            Some(lo) => lo,
            // A constant path starting a range: `i64::MIN..=-1`.
            None if matches!(self.peek(), TokKind::Ident(_)) => {
                let path = self.path()?;
                if !matches!(self.peek(), TokKind::DotDotEq | TokKind::DotDot) {
                    self.pos = save;
                    return Ok(None);
                }
                Expr { kind: std::sync::Arc::new(ExprKind::Path(path)), span: start.to(self.prev_span()) }
            }
            None => return Ok(None),
        };
        let inclusive = self.at(&TokKind::DotDotEq);
        let is_range = inclusive || self.at(&TokKind::DotDot);
        // A path that does not start a range is an ordinary pattern
        // (`Msg::Move(..)`, a unit variant), even after `name @`.
        let from_path = matches!(&*lo.kind, ExprKind::Path(_));
        if !is_range && from_path {
            self.pos = save;
            return Ok(None);
        }
        if !is_range && !negative && name.is_none() {
            // An ordinary literal pattern.
            self.pos = save;
            return Ok(None);
        }
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span: start };
        let binding = name.unwrap_or_else(|| {
            self.pattern_counter += 1;
            format!("pat#{}", self.pattern_counter)
        });
        let var = || ex(ExprKind::Path(Path::single(binding.clone(), start)));
        let guard = if is_range {
            self.bump();
            let hi = self.literal_pattern_expr()?.ok_or_else(|| self.error("expected the end of the range pattern"))?;
            let upper = if inclusive { BinOp::Le } else { BinOp::Lt };
            ex(ExprKind::Binary(BinOp::And, ex(ExprKind::Binary(BinOp::Ge, var(), lo)), ex(ExprKind::Binary(upper, var(), hi))))
        } else {
            ex(ExprKind::Binary(BinOp::Eq, var(), lo))
        };
        let pat = Pattern { kind: PatternKind::Binding { is_mut: false, name: binding }, span: start.to(self.prev_span()) };
        Ok(Some((pat, vec![guard])))
    }

    /// Combine the alternatives of each sub-pattern (their cartesian product).
    fn product(parts: Vec<Vec<(Pattern, Vec<Expr>)>>) -> Vec<(Vec<Pattern>, Vec<Expr>)> {
        let mut acc: Vec<(Vec<Pattern>, Vec<Expr>)> = vec![(Vec::new(), Vec::new())];
        for alts in parts {
            let mut next = Vec::new();
            for (pats, guards) in &acc {
                for (p, g) in &alts {
                    let mut pats = pats.clone();
                    pats.push(p.clone());
                    let mut guards = guards.clone();
                    guards.extend(g.iter().cloned());
                    next.push((pats, guards));
                }
            }
            acc = next;
        }
        acc
    }

    fn pattern_list(&mut self, close: &TokKind) -> PResult<Vec<Vec<(Pattern, Vec<Expr>)>>> {
        let mut parts = Vec::new();
        while !self.at(close) {
            // `..`: the remaining elements (see `ast::expand_rest`).
            if self.at(&TokKind::DotDot) {
                let span = self.span();
                self.bump();
                parts.push(vec![(Pattern { kind: PatternKind::Binding { is_mut: false, name: REST_PATTERN.into() }, span }, Vec::new())]);
                if !self.eat(&TokKind::Comma) { break; }
                continue;
            }
            parts.push(self.pattern_alts()?);
            if !self.eat(&TokKind::Comma) { break; }
        }
        Ok(parts)
    }

    /// A match arm for one pattern alternative: its range/slice guards and
    /// the user's guard, with any slice element bindings in scope for the
    /// user's guard and the body.
    fn arm(&self, pattern: Pattern, guards: Vec<Expr>, user_guard: Option<Expr>, body: Expr, span: Span) -> MatchArm {
        let mut names = Vec::new();
        pattern_names(&pattern, &mut names);
        let prelude: Vec<Stmt> = names.iter().filter(|(n, _)| n.starts_with("slice#"))
            .flat_map(|(n, _)| self.slice_lets.get(n).cloned().unwrap_or_default()).collect();
        let wrap = |e: Expr| if prelude.is_empty() { e } else {
            let span = e.span;
            Expr { kind: std::sync::Arc::new(ExprKind::Block(Block { stmts: prelude.clone(), tail: Some(e), span })), span }
        };
        MatchArm { pattern, guard: conjoin(guards, user_guard.map(wrap)), body: wrap(body), span }
    }

    /// `[a, b, .., z]`, `[x, rest @ ..]`, `[]`: the value is bound to a
    /// `slice#..` name, its length and literal/range elements become guards,
    /// and named elements become bindings (see `arm`). Elements may be names,
    /// `_`, literals, and ranges; `..` or `name @ ..` takes the rest.
    fn slice_pattern(&mut self) -> PResult<Vec<(Pattern, Vec<Expr>)>> {
        enum Elem { Bind(String, bool), Wild, Eq(Expr), Range(Expr, Expr, bool), Pat(Pattern) }
        let start = self.expect(&TokKind::LBracket)?;
        let mut before: Vec<Elem> = Vec::new();
        let mut after: Vec<Elem> = Vec::new();
        let mut rest: Option<Option<String>> = None;
        while !self.at(&TokKind::RBracket) {
            if self.eat(&TokKind::DotDot) {
                if rest.is_some() { return Err(self.error("a slice pattern has at most one `..`")); }
                rest = Some(None);
            } else if let (TokKind::Ident(n), TokKind::At, TokKind::DotDot) = (self.peek().clone(), self.peek_at(1).clone(), self.peek_at(2).clone()) {
                self.bump(); self.bump(); self.bump();
                if rest.is_some() { return Err(self.error("a slice pattern has at most one `..`")); }
                rest = Some(Some(n));
            } else {
                let estart = self.span();
                let elem = match self.literal_pattern_expr()? {
                    Some(lo) => {
                        if self.at(&TokKind::DotDotEq) || self.at(&TokKind::DotDot) {
                            let inclusive = self.at(&TokKind::DotDotEq);
                            self.bump();
                            let hi = self.literal_pattern_expr()?.ok_or_else(|| self.error("expected the end of the range pattern"))?;
                            Elem::Range(lo, hi, inclusive)
                        } else { Elem::Eq(lo) }
                    }
                    None => match self.peek().clone() {
                        TokKind::Ident(n) if n == "_" => { self.bump(); Elem::Wild }
                        TokKind::Keyword(Kw::Mut) => { self.bump(); Elem::Bind(self.ident()?, true) }
                        TokKind::Ident(n) if !n.chars().next().is_some_and(|c| c.is_uppercase()) && !matches!(self.peek_at(1), TokKind::ColonColon | TokKind::LParen | TokKind::LBrace) => {
                            self.bump();
                            Elem::Bind(n, false)
                        }
                        TokKind::Str(t) => { self.bump(); Elem::Eq(Expr { kind: std::sync::Arc::new(ExprKind::Str(t)), span: estart }) }
                        TokKind::Keyword(Kw::True) => { self.bump(); Elem::Eq(Expr { kind: std::sync::Arc::new(ExprKind::Bool(true)), span: estart }) }
                        TokKind::Keyword(Kw::False) => { self.bump(); Elem::Eq(Expr { kind: std::sync::Arc::new(ExprKind::Bool(false)), span: estart }) }
                        // A destructuring element (`(x, y)`, `P { x, .. }`)
                        // binds through a `let`, so it must be irrefutable.
                        _ => {
                            let mut alts = self.pattern_alts()?;
                            if alts.len() != 1 || !alts[0].1.is_empty() {
                                return Err(ParseError { msg: "a slice pattern element cannot use `|` or ranges inside it".into(), span: estart.to(self.prev_span()) });
                            }
                            Elem::Pat(alts.pop().unwrap().0)
                        }
                    },
                };
                if rest.is_some() { after.push(elem) } else { before.push(elem) }
            }
            if !self.eat(&TokKind::Comma) { break; }
        }
        self.expect(&TokKind::RBracket)?;
        let span = start.to(self.prev_span());
        self.pattern_counter += 1;
        let tests = before.iter().chain(&after).any(|e| matches!(e, Elem::Eq(_) | Elem::Range(..)));
        let k = before.len() + after.len();
        let name = format!("slice#{}#{}#{}#{}", self.pattern_counter, k, rest.is_some() as u8, tests as u8);
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let var = || ex(ExprKind::Path(Path::single(name.clone(), span)));
        let int = |n: usize| ex(ExprKind::Int(n as u64, crate::lexer::NumSuffix::None));
        let len = || ex(ExprKind::MethodCall { recv: var(), method: "len".into(), args: vec![], span });
        let mut guards = vec![ex(ExprKind::Binary(if rest.is_some() { BinOp::Ge } else { BinOp::Eq }, len(), int(k)))];
        let mut lets = Vec::new();
        let n_after = after.len();
        let positions = before.into_iter().enumerate().map(|(i, e)| (int(i), e))
            .chain(after.into_iter().enumerate().map(|(j, e)| (ex(ExprKind::Binary(BinOp::Sub, len(), int(n_after - j))), e)));
        for (index, e) in positions {
            let at = ex(ExprKind::Index { base: var(), index });
            match e {
                Elem::Wild => {}
                Elem::Bind(n, m) => lets.push(Stmt::Let { pattern: Pattern { kind: PatternKind::Binding { is_mut: m, name: n }, span }, ty: None, init: Some(at), span }),
                Elem::Pat(p) => lets.push(Stmt::Let { pattern: p, ty: None, init: Some(at), span }),
                Elem::Eq(v) => guards.push(ex(ExprKind::Binary(BinOp::Eq, at, v))),
                Elem::Range(lo, hi, inclusive) => guards.push(ex(ExprKind::Binary(BinOp::And,
                    ex(ExprKind::Binary(BinOp::Ge, at.clone(), lo)),
                    ex(ExprKind::Binary(if inclusive { BinOp::Le } else { BinOp::Lt }, at, hi))))),
            }
        }
        if let Some(Some(r)) = rest {
            let lo = int(k - n_after);
            let hi = ex(ExprKind::Binary(BinOp::Sub, len(), int(n_after)));
            let range = ex(ExprKind::Range { lo: Some(lo), hi: Some(hi), inclusive: false });
            lets.push(Stmt::Let { pattern: Pattern { kind: PatternKind::Binding { is_mut: false, name: r }, span }, ty: None, init: Some(ex(ExprKind::Index { base: var(), index: range })), span });
        }
        self.slice_lets.insert(name.clone(), lets);
        Ok(vec![(Pattern { kind: PatternKind::Binding { is_mut: false, name }, span }, guards)])
    }

    fn pattern_one(&mut self) -> PResult<Vec<(Pattern, Vec<Expr>)>> {
        let start = self.span();
        if self.at(&TokKind::LBracket) { return self.slice_pattern(); }
        if let Some(alt) = self.guarded_literal_pattern(None)? {
            return Ok(vec![alt]);
        }
        // `ref x` / `ref mut x`: every binding already shares the value.
        if matches!(self.peek(), TokKind::Ident(r) if r == "ref") && matches!(self.peek_at(1), TokKind::Ident(_) | TokKind::Keyword(Kw::Mut)) {
            self.bump();
        }
        // `name @ 1..=9` (a guard), or `name @ PATTERN`.
        let at_mut = matches!(self.peek(), TokKind::Keyword(Kw::Mut)) && matches!(self.peek_at(2), TokKind::At);
        if at_mut { self.bump(); }
        if let (TokKind::Ident(name), TokKind::At) = (self.peek().clone(), self.peek_at(1).clone()) {
            self.bump();
            self.bump();
            if let Some(alt) = self.guarded_literal_pattern(Some(name.clone()))? {
                return Ok(vec![alt]);
            }
            let inner = self.pattern_one()?;
            let span = start.to(self.prev_span());
            return Ok(inner.into_iter().map(|(p, g)| (Pattern { kind: PatternKind::At { is_mut: at_mut, name: name.clone(), pat: Box::new(p) }, span }, g)).collect());
        }
        let single = |kind: PatternKind, this: &Self| vec![(Pattern { kind, span: start.to(this.prev_span()) }, Vec::new())];
        let kind = match self.peek().clone() {
            TokKind::Ident(name) if name == "_" => { self.bump(); PatternKind::Wildcard }
            TokKind::Keyword(Kw::Mut) => {
                self.bump();
                let name = self.ident()?;
                PatternKind::Binding { is_mut: true, name }
            }
            TokKind::Int(n, _) => { self.bump(); PatternKind::Literal(LitPattern::Int(n)) }
            TokKind::Keyword(Kw::True) => { self.bump(); PatternKind::Literal(LitPattern::Bool(true)) }
            TokKind::Keyword(Kw::False) => { self.bump(); PatternKind::Literal(LitPattern::Bool(false)) }
            TokKind::Char(c) => { self.bump(); PatternKind::Literal(LitPattern::Char(c)) }
            TokKind::Str(s) => { self.bump(); PatternKind::Literal(LitPattern::Str(s)) }
            TokKind::LParen => {
                self.bump();
                let parts = self.pattern_list(&TokKind::RParen)?;
                self.expect(&TokKind::RParen)?;
                // `(p)` is just `p`.
                if parts.len() == 1 && !matches!(self.toks[self.pos - 2].kind, TokKind::Comma) {
                    return Ok(parts.into_iter().next().unwrap());
                }
                let span = start.to(self.prev_span());
                return Ok(Self::product(parts).into_iter()
                    .map(|(pats, g)| (Pattern { kind: PatternKind::Tuple(pats), span }, g)).collect());
            }
            TokKind::Ident(_) | TokKind::Keyword(Kw::SelfType) => {
                let path = self.path()?;
                if self.at(&TokKind::LParen) {
                    self.bump();
                    let parts = self.pattern_list(&TokKind::RParen)?;
                    self.expect(&TokKind::RParen)?;
                    let span = start.to(self.prev_span());
                    return Ok(Self::product(parts).into_iter()
                        .map(|(payload, g)| (Pattern { kind: PatternKind::Variant { path: path.clone(), payload }, span }, g)).collect());
                } else if self.at(&TokKind::LBrace) {
                    self.bump();
                    let mut names = Vec::new();
                    let mut parts = Vec::new();
                    let mut rest = false;
                    while !self.at(&TokKind::RBrace) {
                        if self.eat(&TokKind::DotDot) { rest = true; break; }
                        let fname = self.ident()?;
                        let fpat = if self.eat(&TokKind::Colon) {
                            self.pattern_alts()?
                        } else {
                            vec![(Pattern {
                                kind: PatternKind::Binding { is_mut: false, name: fname.clone() },
                                span: self.prev_span(),
                            }, Vec::new())]
                        };
                        names.push(fname);
                        parts.push(fpat);
                        if !self.eat(&TokKind::Comma) { break; }
                    }
                    self.expect(&TokKind::RBrace)?;
                    let span = start.to(self.prev_span());
                    return Ok(Self::product(parts).into_iter()
                        .map(|(pats, g)| (Pattern {
                            kind: PatternKind::Struct { path: path.clone(), fields: names.iter().cloned().zip(pats).collect(), rest },
                            span,
                        }, g)).collect());
                } else if path.is_single() {
                    // A bare lowercase-ish single ident is a binding; an
                    // upper-case one *could* be a unit variant. Resolution
                    // disambiguates; we record it as a Binding-or-variant by
                    // name and let resolve decide. We choose Variant if the
                    // name starts uppercase, else Binding.
                    let name = path.segments[0].clone();
                    if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                        PatternKind::Variant { path, payload: Vec::new() }
                    } else {
                        PatternKind::Binding { is_mut: false, name }
                    }
                } else {
                    PatternKind::Variant { path, payload: Vec::new() }
                }
            }
            other => return Err(self.error(format!("expected pattern, found {:?}", other))),
        };
        Ok(single(kind, self))
    }

}

fn expr_is_block_like(e: &Expr) -> bool {
    matches!(
        &*e.kind,
        ExprKind::Block(_)
            | ExprKind::Unsafe(_)
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::While { .. }
            | ExprKind::Loop { .. }
            | ExprKind::For { .. }
    )
}

fn type_to_trait_ref(t: Type) -> Option<TraitRef> {
    match t.kind {
        TypeKind::Path(path, args) => Some(TraitRef { path, args, bindings: vec![], span: t.span }),
        _ => None,
    }
}

/// A parsed `{...}` format spec: `[[fill]align][+][#][0][width][.prec][type]`.
#[derive(Default)]
struct FormatSpec {
    fill: Option<char>,
    /// 0 = the type's default, 1 = `<`, 2 = `>`, 3 = `^`.
    align: i64,
    plus: bool,
    alternate: bool,
    zero: bool,
    width: Option<FormatCount>,
    precision: Option<FormatCount>,
    ty: String,
}

/// A width or precision: a literal, `N$` (argument N), `name$` (a variable),
/// or `*` (the next positional argument; precision only).
enum FormatCount { Lit(u64), Arg(usize), Name(String), Next }

fn parse_format_spec(spec: &str) -> Result<FormatSpec, String> {
    let bad = || format!("invalid format spec `:{}`", spec);
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut out = FormatSpec::default();
    let align_of = |c: char| match c { '<' => Some(1), '>' => Some(2), '^' => Some(3), _ => None };
    if chars.len() >= 2 && align_of(chars[1]).is_some() {
        out.fill = Some(chars[0]);
        out.align = align_of(chars[1]).unwrap();
        i = 2;
    } else if let Some(a) = chars.first().and_then(|c| align_of(*c)) {
        out.align = a;
        i = 1;
    }
    if chars.get(i) == Some(&'+') { out.plus = true; i += 1; }
    else if chars.get(i) == Some(&'-') { i += 1; }
    if chars.get(i) == Some(&'#') { out.alternate = true; i += 1; }
    // `0` is the zero-pad flag unless it is the argument index of `0$`.
    if chars.get(i) == Some(&'0') && chars.get(i + 1) != Some(&'$') { out.zero = true; i += 1; }
    // A count: digits, or an identifier, optionally followed by `$`.
    let count = |i: &mut usize, allow_star: bool| -> Result<Option<FormatCount>, String> {
        if allow_star && chars.get(*i) == Some(&'*') { *i += 1; return Ok(Some(FormatCount::Next)); }
        let start = *i;
        if chars.get(*i).is_some_and(|c| c.is_ascii_digit()) {
            while chars.get(*i).is_some_and(|c| c.is_ascii_digit()) { *i += 1; }
        } else {
            while chars.get(*i).is_some_and(|c| c.is_alphanumeric() || *c == '_') { *i += 1; }
        }
        let word: String = chars[start..*i].iter().collect();
        if word.is_empty() { return Ok(None); }
        if chars.get(*i) == Some(&'$') {
            *i += 1;
            return Ok(Some(match word.parse::<usize>() { Ok(n) => FormatCount::Arg(n), Err(_) => FormatCount::Name(word) }));
        }
        match word.parse::<u64>() {
            Ok(n) => Ok(Some(FormatCount::Lit(n))),
            // Not a count: it is the trailing type (`x`, `e`, ...). Rewind.
            Err(_) => { *i = start; Ok(None) }
        }
    };
    out.width = count(&mut i, false)?;
    if chars.get(i) == Some(&'.') {
        i += 1;
        out.precision = Some(count(&mut i, true)?.ok_or_else(bad)?);
    }
    let ty: String = chars[i..].iter().collect();
    if !matches!(ty.as_str(), "" | "?" | "x" | "X" | "o" | "b" | "e" | "E" | "x?" | "X?") { return Err(bad()); }
    out.ty = ty;
    Ok(out)
}

/// Expand a `format!` string and its arguments into a `String` expression.
///
/// Supports `{}` (the next argument), `{N}` (argument N), `{name}` (a variable
/// in scope), Rust's format specs after a `:` (fill/alignment, `+`, `#`, `0`,
/// width, precision, and the `?`, `x`, `X`, `o`, `b`, `e`, `E` types; widths
/// and precisions may be `N$`, `name$`, or `.*`), and the escapes `{{` / `}}`.
/// `{}` renders with `to_string()` and `{:?}` with `Debug`. Arguments are
/// evaluated once, in order. An argument the string never uses is an error.
fn expand_format(fmt: &str, args: Vec<Expr>, span: Span) -> PResult<Expr> {
    let fail = |msg: String| Err(ParseError { msg, span });
    let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
    let arg_name = |i: usize| format!("format#{}", i);
    let var = |n: String| ex(ExprKind::Path(Path::single(n, span)));
    let method = |e: Expr, m: &str, args: Vec<Expr>| ex(ExprKind::MethodCall { recv: e, method: m.into(), args, span });
    let int = |n: i64| {
        let lit = ex(ExprKind::Int(n.unsigned_abs(), crate::lexer::NumSuffix::None));
        if n < 0 { ex(ExprKind::Unary(UnOp::Neg, lit)) } else { lit }
    };
    let text = |t: &str| ex(ExprKind::Str(t.to_string()));
    let boolean = |b: bool| ex(ExprKind::Bool(b));
    let mut pieces: Vec<Expr> = Vec::new();
    let mut literal = String::new();
    let mut used = vec![false; args.len()];
    let mut next = 0;
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => { chars.next(); literal.push('{'); }
            '}' if chars.peek() == Some(&'}') => { chars.next(); literal.push('}'); }
            '}' => return fail("unmatched `}` in format string (write `}}` for a literal brace)".into()),
            '{' => {
                let mut placeholder = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(ch) => placeholder.push(ch),
                        None => return fail("unterminated `{` in format string (write `{{` for a literal brace)".into()),
                    }
                }
                let (arg, spec) = match placeholder.split_once(':') {
                    Some((a, s)) => (a.trim().to_string(), Some(s.to_string())),
                    None => (placeholder.trim().to_string(), None),
                };
                let spec = match &spec {
                    Some(s) => match parse_format_spec(s) { Ok(s) => s, Err(m) => return fail(m) },
                    None => FormatSpec::default(),
                };
                let mut positional = |index: Option<usize>, used: &mut Vec<bool>| -> PResult<Expr> {
                    let index = index.unwrap_or_else(|| { next += 1; next - 1 });
                    if index >= args.len() {
                        return fail(format!("format string uses argument {} but {} argument(s) were given", index, args.len()));
                    }
                    used[index] = true;
                    Ok(var(arg_name(index)))
                };
                // `.*` takes the precision from the next positional argument,
                // before the value itself.
                let star_precision = if matches!(spec.precision, Some(FormatCount::Next)) {
                    Some(positional(None, &mut used)?)
                } else { None };
                let value = if arg.is_empty() || arg.chars().all(|ch| ch.is_ascii_digit()) {
                    positional(if arg.is_empty() { None } else { Some(arg.parse::<usize>().unwrap()) }, &mut used)?
                } else if arg.chars().next().is_some_and(|ch| ch.is_alphabetic() || ch == '_')
                    && arg.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
                {
                    var(arg.to_string())
                } else {
                    return fail(format!("invalid format placeholder `{{{}}}`", placeholder));
                };
                let count_expr = |c: &Option<FormatCount>, used: &mut Vec<bool>| -> PResult<Option<Expr>> {
                    Ok(match c {
                        None => None,
                        Some(FormatCount::Lit(n)) => Some(int(*n as i64)),
                        Some(FormatCount::Arg(n)) => {
                            if *n >= args.len() { return Err(ParseError { msg: format!("format string uses argument {} but {} argument(s) were given", n, args.len()), span }); }
                            used[*n] = true;
                            Some(var(arg_name(*n)))
                        }
                        Some(FormatCount::Name(name)) => Some(var(name.clone())),
                        Some(FormatCount::Next) => star_precision.clone(),
                    })
                };
                let width = count_expr(&spec.width, &mut used)?;
                let precision = count_expr(&spec.precision, &mut used)?;
                let prec_or_none = || precision.clone().unwrap_or_else(|| int(-1));
                let (body, prefix) = match spec.ty.as_str() {
                    "?" | "x?" | "X?" => {
                        let m = if spec.alternate { "fmt#debug_pretty" } else { "fmt#debug" };
                        (method(value.clone(), m, vec![prec_or_none()]), "")
                    }
                    "x" | "X" | "o" | "b" => {
                        let (radix, prefix) = match spec.ty.as_str() { "o" => (8, "0o"), "b" => (2, "0b"), _ => (16, "0x") };
                        (method(value.clone(), "fmt#radix", vec![int(radix), boolean(spec.ty == "X")]), if spec.alternate { prefix } else { "" })
                    }
                    "e" | "E" => (method(value.clone(), "fmt#exp", vec![prec_or_none(), boolean(spec.ty == "E")]), ""),
                    _ => match &precision {
                        Some(p) => (method(value.clone(), "fmt#prec", vec![p.clone()]), ""),
                        None => (method(value.clone(), "to_string", vec![]), ""),
                    },
                };
                let piece = if width.is_some() || spec.plus || spec.zero || !prefix.is_empty() {
                    let call = |f: &str, args: Vec<Expr>| ex(ExprKind::Call(var(f.to_string()), args));
                    call("fmt_pad", vec![
                        body,
                        text(prefix),
                        text(&spec.fill.unwrap_or(' ').to_string()),
                        int(spec.align),
                        width.unwrap_or_else(|| int(0)),
                        method(value, "fmt#numeric", vec![]),
                        boolean(spec.plus),
                        boolean(spec.zero),
                    ])
                } else {
                    body
                };
                if !literal.is_empty() {
                    pieces.push(ex(ExprKind::Str(std::mem::take(&mut literal))));
                }
                pieces.push(piece);
            }
            _ => literal.push(c),
        }
    }
    if let Some(unused) = used.iter().position(|u| !u) {
        return fail(format!("format argument {} is never used by the format string", unused));
    }
    if !literal.is_empty() || pieces.is_empty() {
        pieces.push(ex(ExprKind::Str(literal)));
    }
    let joined = pieces.into_iter().reduce(|a, b| ex(ExprKind::Binary(BinOp::Add, a, b))).unwrap();
    if args.is_empty() {
        return Ok(joined);
    }
    // Bind each argument once, in order, then build the string.
    let stmts = args.into_iter().enumerate().map(|(i, value)| Stmt::Let {
        pattern: Pattern { kind: PatternKind::Binding { is_mut: false, name: arg_name(i) }, span },
        ty: None,
        init: Some(value),
        span,
    }).collect();
    Ok(ex(ExprKind::Block(Block { stmts, tail: Some(joined), span })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parse(src: &str) -> Module {
        let toks = lex(src).expect("lex");
        parse_module(&toks).unwrap_or_else(|e| panic!("parse error: {} at {:?}", e.msg, e.span))
    }

    #[test]
    fn parses_fib() {
        let m = parse(include_str!("../examples/fib.gcr"));
        assert_eq!(m.items.len(), 2);
        match &m.items[0].kind {
            ItemKind::Fn(f) => {
                assert_eq!(f.name, "fib");
                assert_eq!(f.params.len(), 1);
            }
            _ => panic!("expected fn"),
        }
    }

    #[test]
    fn parses_types_example() {
        let m = parse(include_str!("../examples/types.gcr"));
        // The tour has 2 enums (Option, Result), a trait, 2 impls, structs + fns.
        let kinds: Vec<&str> = m.items.iter().map(|i| match &i.kind {
            ItemKind::Fn(_) => "fn",
            ItemKind::Struct(_) => "struct",
            ItemKind::Enum(_) => "enum",
            ItemKind::Trait(_) => "trait",
            ItemKind::Impl(_) => "impl",
            _ => "other",
        }).collect();
        assert!(kinds.contains(&"trait"));
        assert_eq!(kinds.iter().filter(|k| **k == "impl").count(), 2);
        assert_eq!(kinds.iter().filter(|k| **k == "enum").count(), 2);
    }

    #[test]
    fn nested_generics_close() {
        let m = parse("fn f() -> Vec<Vec<i64>> { x }");
        match &m.items[0].kind {
            ItemKind::Fn(f) => match &f.ret.as_ref().unwrap().kind {
                TypeKind::Path(p, args) => {
                    assert_eq!(p.last(), "Vec");
                    assert_eq!(args.len(), 1);
                }
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn precedence() {
        // 1 + 2 * 3 == 7  parses as  (1 + (2*3)) == 7
        let m = parse("fn f() -> bool { 1 + 2 * 3 == 7 }");
        let ItemKind::Fn(f) = &m.items[0].kind else { panic!() };
        let tail = f.body.tail.as_ref().unwrap();
        match &*tail.kind {
            ExprKind::Binary(BinOp::Eq, lhs, _) => match &*lhs.kind {
                ExprKind::Binary(BinOp::Add, _, r) => {
                    assert!(matches!(&*r.kind, ExprKind::Binary(BinOp::Mul, _, _)));
                }
                _ => panic!("add not at top of lhs"),
            },
            _ => panic!("eq not at top"),
        }
    }

    #[test]
    fn if_cond_not_struct_lit() {
        // `if x { 1 } else { 2 }` — x is a path, not a struct literal.
        let m = parse("fn f(x: bool) -> i64 { if x { 1 } else { 2 } }");
        let ItemKind::Fn(f) = &m.items[0].kind else { panic!() };
        assert!(matches!(&*f.body.tail.as_ref().unwrap().kind, ExprKind::If { .. }));
    }

    #[test]
    fn match_and_try() {
        let m = parse(
            "fn f() -> i64 { let q = g()?; match q { Result::Ok(v) => v, _ => 0 } }",
        );
        let ItemKind::Fn(f) = &m.items[0].kind else { panic!() };
        assert!(matches!(&*f.body.tail.as_ref().unwrap().kind, ExprKind::Match { .. }));
    }

    #[test]
    fn condition_restriction_does_not_leak_into_delimited_expressions() {
        let source = "struct S { n: i64 } fn main() { if f(S { n: 1 }) {} while f({ let s = S { n: 2 }; s }) { break; } match f(|n: i64| S { n: n }) { _ => {} } if (S { n: 3 }).n == 3 {} }";
        parse_module(&lex(source).unwrap()).unwrap();
    }

    #[test]
    fn closures() {
        let m = parse("fn f() -> i64 { let g = |x: i64| x + 1; g(2) }");
        let ItemKind::Fn(f) = &m.items[0].kind else { panic!() };
        let Stmt::Let { init: Some(init), .. } = &f.body.stmts[0] else { panic!() };
        assert!(matches!(&*init.kind, ExprKind::Closure { .. }));
    }
}
