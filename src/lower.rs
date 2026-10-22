//! Type-checking + lowering to core IR.
//!
//! This pass walks the resolved surface AST, checks types, and emits the
//! monomorphic [`CoreProgram`]. For the v0 vertical slice it handles the
//! non-generic, non-heap subset: primitive scalars, arithmetic / comparison /
//! logic, `if`/`else`, `let`, blocks, direct function calls (incl. recursion),
//! and `return`. Structs, enums, generics+monomorphization, closures, and
//! traits are layered on in subsequent steps against this same scaffold.

use crate::ast::*;
use crate::core::*;
use crate::layout::LayoutRegistry;
use crate::lexer::Span;
use crate::types::{Prim, Ty, TyCtx, lower_type, suffix_prim};
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq)]
pub struct LowerError {
    pub msg: String,
    pub span: Span,
}
type LResult<T> = Result<T, LowerError>;
#[cfg(test)]
thread_local! {
    static PROBE_WORK: std::cell::Cell<usize> = const {std::cell::Cell::new(0)};
    static EXPR_WORK: std::cell::Cell<usize> = const {std::cell::Cell::new(0)};
}
#[cfg(test)]
pub(crate) fn reset_probe_work() { PROBE_WORK.with(|n|n.set(0)); EXPR_WORK.with(|n|n.set(0)); }
#[cfg(test)]
pub(crate) fn probe_work() -> usize {PROBE_WORK.with(|n|n.get())}
#[cfg(test)]
pub(crate) fn expr_work() -> usize {EXPR_WORK.with(|n|n.get())}


/// A method operand may already have been evaluated into a typed place local.
/// Keeping that fact in Core avoids synthesizing source names during lowering.
enum MethodArg<'a> {
    Source(&'a Expr),
    Lowered(CoreExpr, Ty, Span),
}

fn err<T>(msg: impl Into<String>, span: Span) -> LResult<T> {
    Err(LowerError { msg: msg.into(), span })
}

/// These primitives expose representation/handle obligations. Public typed
/// wrappers discharge those obligations inside explicit unsafe blocks.
pub fn native_intrinsic_requires_unsafe(name: &str) -> bool {
    matches!(name,
        "ptr_read_i64" | "thread_spawn" | "thread_join" | "thread_bind_owner" | "atomic_i64_bind_owner"
        | "atomic_i64_new" | "atomic_i64_load" | "atomic_i64_store"
        | "atomic_i64_fetch_add" | "atomic_i64_cas"
        | "atom_load" | "atom_cas"
        | "chan_bind_owner" | "chan_new" | "chan_send" | "chan_recv" | "chan_close"
        | "chan_sender_clone" | "chan_sender_drop"
        | "chan_waiting_senders" | "chan_waiting_receivers")
}

/// Levenshtein edit distance between two short identifiers (for "did you mean?"
/// suggestions). Bounded use only — fine for identifier-length strings.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Pick the closest candidate name to `name` (within a small edit distance), for
/// a "did you mean `x`?" hint. Returns `None` if nothing is close enough.
fn closest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    // Allow up to ~1/3 of the name's length in edits (min 1, max 3).
    let budget = (name.len() / 3).clamp(1, 3);
    let mut best: Option<(usize, String)> = None;
    for c in candidates {
        if c == "_" || c.is_empty() { continue; }
        let d = edit_distance(name, c);
        if d <= budget && best.as_ref().map_or(true, |(bd, _)| d < *bd) {
            best = Some((d, c.to_string()));
        }
    }
    best.map(|(_, s)| s)
}

/// A resolved function instantiation queued for lowering: the source `FnDef`,
/// the type-parameter substitution to apply, and the mangled name. Works for
/// free functions AND impl methods (methods aren't in `ctx.fns`, so we carry
/// the `FnDef` directly).
#[derive(Clone, PartialEq)]
struct Job {
    f: Rc<FnDef>,
    subst: HashMap<String, Ty>,
    mangled: String,
    /// For an impl method, the receiver's `self` type (already ground). `None`
    /// for a free function.
    self_ty: Option<Ty>,
}

/// Source identity and ground types determine an instantiation. Display names
/// are deliberately excluded: qualified names and type spellings can collide.
#[derive(Clone, PartialEq, Eq, Hash)]
struct InstantiationKey {
    source: crate::lexer::SourceId,
    start: u32,
    end: u32,
    /// Functions a macro expands share their body's span; the name keeps
    /// each expansion distinct.
    name: String,
    subst: Vec<(String, Ty)>,
    self_ty: Option<Ty>,
}

/// Drives monomorphization: assigns each needed instantiation (keyed by source identity and ground types) a [`FuncId`] and keeps a worklist of jobs to lower.
#[derive(Clone, PartialEq)]
struct Mono {
    ids: HashMap<InstantiationKey, FuncId>,
    worklist: Vec<(FuncId, Job)>,
    /// Fully-formed lifted closure functions, keyed by their FuncId, to be
    /// installed into `prog.funcs` by the driver.
    closures: Vec<(FuncId, CoreFn)>,
    /// Interned source spans (span-threading). Accumulated across all lowered
    /// functions; moved into `CoreProgram::spans` at the end. A returned id `k`
    /// means `spans[k-1]`; `NO_SPAN` (0) is never returned here.
    spans: Vec<Span>,
    /// The concrete result type of each instantiation whose signature returns
    /// `impl Trait`, and the ones being worked out (to report recursion).
    /// Discovering one lowers the body and rolls it back, so these are not
    /// part of a snapshot.
    opaque: HashMap<InstantiationKey, Ty>,
    opaque_pending: std::collections::HashSet<InstantiationKey>,
}

/// `Mono`'s contents at a point in time; see [`Mono::snapshot`].
struct MonoSnapshot {
    ids: HashMap<InstantiationKey, FuncId>,
    worklist: usize,
    closures: usize,
    spans: usize,
}

impl Mono {
    fn new() -> Self {
        Mono { ids: HashMap::new(), worklist: Vec::new(), closures: Vec::new(), spans: Vec::new(), opaque: HashMap::new(), opaque_pending: Default::default() }
    }
    /// Record the monomorphizer so a discarded lowering attempt can be undone.
    /// Everything but the interning map is append-only.
    fn snapshot(&self) -> MonoSnapshot {
        MonoSnapshot { ids: self.ids.clone(), worklist: self.worklist.len(), closures: self.closures.len(), spans: self.spans.len() }
    }
    /// Drop every instantiation, closure, and span added since `snap`.
    fn restore(&mut self, snap: MonoSnapshot) {
        self.ids = snap.ids;
        self.worklist.truncate(snap.worklist);
        self.closures.truncate(snap.closures);
        self.spans.truncate(snap.spans);
    }
    /// Intern a source span, returning its 1-based `SpanId` (so `NO_SPAN`=0 stays
    /// reserved). Threaded onto construction/alloc Core nodes for file:line:col.
    fn intern_span(&mut self, s: Span) -> crate::core::SpanId {
        self.spans.push(s);
        self.spans.len() as crate::core::SpanId
    }
    /// Allocate a fresh FuncId for a lifted closure function (not name-keyed).
    fn fresh_closure_id(&mut self, count: &mut u32) -> FuncId {
        let id = *count;
        *count += 1;
        id
    }
    /// Get (or assign) the FuncId for a job (keyed by source identity and ground types), queueing it
    /// for lowering the first time it's seen.
    fn key(job: &Job) -> InstantiationKey {
        let mut subst: Vec<_> = job.subst.iter().map(|(name, ty)| (name.clone(), ty.clone())).collect();
        subst.sort_by(|a, b| a.0.cmp(&b.0));
        InstantiationKey {
            source: job.f.span.source, start: job.f.span.start, end: job.f.span.end,
            name: job.f.name.clone(), subst, self_ty: job.self_ty.clone(),
        }
    }
    fn intern(&mut self, job: Job, count: &mut u32) -> FuncId {
        let key = Self::key(&job);
        if let Some(id) = self.ids.get(&key) {
            return *id;
        }
        let id = *count;
        *count += 1;
        self.ids.insert(key, id);
        self.worklist.push((id, job));
        id
    }
}

fn mangle(fq: &str, args: &[Ty]) -> String {
    if args.is_empty() {
        fq.to_string()
    } else {
        format!("{}${}", fq, args.iter().map(ty_mangle).collect::<Vec<_>>().join("_"))
    }
}

/// Rust's other entry signatures, `fn main()` and `fn main() -> Result<(), E>`,
/// run through a generated `fn main() -> i64` wrapper: `()` and `Ok(())` exit
/// 0, and `Err(e)` prints `Error: {e:?}` to stderr and exits 1.
fn wrap_entry(ctx: &mut TyCtx) -> LResult<()> {
    let Some(main_f) = ctx.fns.get("main").cloned() else { return Ok(()) };
    if main_f.is_extern || !main_f.generics.params.is_empty() || !main_f.params.is_empty() {
        return Ok(());
    }
    let ret = main_f.ret.as_ref().map(|t| lower_type(t, &[], ctx)).transpose().map_err(conv)?;
    let body = match &ret {
        None | Some(Ty::Prim(Prim::Unit)) => "{ __gcr_user_main(); 0 }",
        Some(Ty::Named { name, args }) if name == "Result" && args.first().is_some_and(Ty::is_unit) =>
            "{ match __gcr_user_main() { Result::Ok(_) => 0, Result::Err(e) => { eprintln!(\"Error: {:?}\", e); 1 } } }",
        _ => return Ok(()),
    };
    let source = format!("fn main() -> i64 {}", body);
    // The wrapper reports at the user's body; it needs its own span (a
    // function's span is its instantiation identity).
    let at = main_f.body.span;
    let mut tokens = crate::lexer::lex_with_source(&source, at.source).expect("entry wrapper lexes");
    for t in &mut tokens { t.span = at; }
    let module = crate::parser::parse_module(&tokens).expect("entry wrapper parses");
    let Some(ItemKind::Fn(wrapper)) = module.items.into_iter().next().map(|i| i.kind) else { unreachable!() };
    let mut user = (*main_f).clone();
    user.name = "__gcr_user_main".into();
    ctx.fns.insert("__gcr_user_main".into(), Rc::new(user));
    ctx.fns.insert("main".into(), Rc::new(wrapper));
    Ok(())
}

pub fn lower_program(globals: &crate::resolve::GlobalTable) -> LResult<CoreProgram> {
    let mut ctx = TyCtx::from_globals(globals);
    wrap_entry(&mut ctx)?;
    let ctx = ctx;
    let _normalizer = crate::types::enter_normalizer(&ctx);
    let mut reg = LayoutRegistry::new(&ctx);
    let mut prog = CoreProgram::default();
    let mut mono = Mono::new();
    let mut count: u32 = 0;

    // Seed from `main` (which must be non-generic).
    let main_fq = ctx.fns.contains_key("main").then(|| "main".to_string());
    let Some(main_fq) = main_fq else {
        return err("no `main` function found", Span::new(0, 0));
    };
    let main_f = ctx.fns[&main_fq].clone();
    if main_f.is_extern || !main_f.generics.params.is_empty() || !main_f.params.is_empty() {
        return err("entry point must be a non-generic `fn main() -> i64`", main_f.span);
    }
    let ret = main_f.ret.as_ref().map(|t| lower_type(t, &[], &ctx)).transpose().map_err(conv)?;
    if ret != Some(Ty::Prim(Prim::I64)) {
        return err("entry point must return i64", main_f.span);
    }

    let entry = mono.intern(
        Job { f: main_f, subst: HashMap::new(), mangled: main_fq, self_ty: None },
        &mut count,
    );
    prog.entry = Some(entry);

    // Lower instantiations until the worklist drains (transitively reachable).
    // Lifted closure functions accumulate in `mono.closures` as bodies are
    // lowered; install them too.
    loop {
        while let Some((id, job)) = mono.worklist.pop() {
            let lowered = lower_fn(&job, &ctx, &mut reg, &mut mono, &mut count)?;
            install_fn(&mut prog, id, lowered);
        }
        if let Some((id, cf)) = mono.closures.pop() {
            install_fn(&mut prog, id, cf);
        } else {
            break;
        }
    }

    prog.layouts = reg.layouts;
    prog.values = reg.values;
    prog.spans = std::mem::take(&mut mono.spans);
    // GC-safety normalization: let-bind every GC temporary so the collector can
    // find it (no live pointer is ever stranded in a register across a safepoint).
    crate::anf::anf_program(&mut prog);
    Ok(prog)
}

/// Type-check every non-generic top-level function (and method) independently,
/// collecting ALL errors rather than stopping at the first. This is the
/// multi-error pass: unlike `lower_program` (which is demand-driven from `main`
/// and halts on the first `?`), this checks each function in isolation, so one
/// broken function doesn't hide errors in the others.
///
/// Generic functions are skipped here — they can only be checked once
/// instantiated with concrete type args, which the demand-driven `lower_program`
/// does. So this catches the common case (broken concrete functions) up front;
/// `lower_program` still reports any error reachable only through a generic
/// instantiation.
pub fn check_program(globals: &crate::resolve::GlobalTable) -> Result<(), Vec<LowerError>> {
    let ctx = TyCtx::from_globals(globals);
    let _normalizer = crate::types::enter_normalizer(&ctx);
    let mut errors = Vec::new();

    // Free functions: check each non-generic one in a throwaway lowering context.
    let mut names: Vec<&String> = ctx.fns.keys().collect();
    names.sort(); // deterministic error order
    for fq in names {
        let f = &ctx.fns[fq];
        if !f.generics.params.is_empty() {
            continue; // generic — needs instantiation; left to lower_program
        }
        let mut reg = LayoutRegistry::new(&ctx);
        let mut mono = Mono::new();
        let mut count = 0u32;
        let job = Job { f: f.clone(), subst: HashMap::new(), mangled: (*fq).clone(), self_ty: None };
        if let Err(e) = lower_fn(&job, &ctx, &mut reg, &mut mono, &mut count) {
            // A caller of an `impl Trait` function lowers its body too, so
            // the same error can arrive from both.
            if !errors.contains(&e) { errors.push(e); }
        }
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

fn install_fn(prog: &mut CoreProgram, id: FuncId, f: CoreFn) {
    while prog.funcs.len() <= id as usize {
        prog.funcs.push(placeholder_fn());
    }
    prog.funcs[id as usize] = f;
}

fn placeholder_fn() -> CoreFn {
    CoreFn { name: String::new(), params: vec![], extern_by_ref: vec![], ret: Repr::Unit, locals: vec![], local_names: vec![], body: CoreBlock { stmts: vec![], tail: None }, closure_captures: vec![], is_closure: false, is_extern: false, span: crate::core::NO_SPAN }
}

/// Collect free variable names referenced in `e`: single-segment path uses that
/// are not in `params` (the closure's own params) and not in `bound` (names
/// bound by inner `let`/closures/match arms). Order-preserving, may repeat.
fn collect_free_vars(e: &Expr, params: &[&str], bound: &mut Vec<String>, out: &mut Vec<String>) {
    match &*e.kind {
        ExprKind::Path(p) if p.is_single() => {
            let n = p.last();
            if !params.contains(&n) && !bound.iter().any(|b| b == n) {
                out.push(n.to_string());
            }
        }
        ExprKind::Path(_) | ExprKind::Int(..) | ExprKind::Float(..) | ExprKind::Str(_)
        | ExprKind::Char(_) | ExprKind::Bool(_) | ExprKind::Unit | ExprKind::Continue => {}
        ExprKind::Call(c, args) => {
            collect_free_vars(c, params, bound, out);
            for a in args { collect_free_vars(a, params, bound, out); }
        }
        ExprKind::MethodCall { recv, args, .. } => {
            collect_free_vars(recv, params, bound, out);
            for a in args { collect_free_vars(a, params, bound, out); }
        }
        ExprKind::Field { base, .. } => collect_free_vars(base, params, bound, out),
        ExprKind::Index { base, index } => { collect_free_vars(base, params, bound, out); collect_free_vars(index, params, bound, out); }
        ExprKind::Unary(_, x) | ExprKind::Cast(x, _) | ExprKind::Try(x) => collect_free_vars(x, params, bound, out),
        ExprKind::Binary(_, l, r) => { collect_free_vars(l, params, bound, out); collect_free_vars(r, params, bound, out); }
        ExprKind::Assign { target, value, .. } => { collect_free_vars(target, params, bound, out); collect_free_vars(value, params, bound, out); }
        ExprKind::StructLit { fields, .. } => {
            for f in fields { if let Some(v) = &f.value { collect_free_vars(v, params, bound, out); } }
        }
        ExprKind::Tuple(es) => for x in es { collect_free_vars(x, params, bound, out); },
        ExprKind::Array(a) => match a {
            ArrayLit::Elems(es) => for x in es { collect_free_vars(x, params, bound, out); },
            ArrayLit::Repeat(v, n) => { collect_free_vars(v, params, bound, out); collect_free_vars(n, params, bound, out); }
        },
        ExprKind::Block(b) | ExprKind::Unsafe(b) => collect_free_in_block(b, params, bound, out),
        ExprKind::If { cond, then_branch, else_branch } => {
            collect_free_vars(cond, params, bound, out);
            collect_free_in_block(then_branch, params, bound, out);
            if let Some(e) = else_branch { collect_free_vars(e, params, bound, out); }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_free_vars(scrutinee, params, bound, out);
            for arm in arms {
                let mark = bound.len();
                bind_pattern_names(&arm.pattern, bound);
                if let Some(g) = &arm.guard { collect_free_vars(g, params, bound, out); }
                collect_free_vars(&arm.body, params, bound, out);
                bound.truncate(mark);
            }
        }
        ExprKind::While { cond, body } => { collect_free_vars(cond, params, bound, out); collect_free_in_block(body, params, bound, out); }
        ExprKind::Loop { body } => collect_free_in_block(body, params, bound, out),
        ExprKind::For { pat, iter, body } => {
            collect_free_vars(iter, params, bound, out);
            let mark = bound.len();
            bind_pattern_names(pat, bound);
            collect_free_in_block(body, params, bound, out);
            bound.truncate(mark);
        }
        ExprKind::Closure { params: cps, body, .. } => {
            // Nested closure: its own params shadow; treat as bound.
            let mark = bound.len();
            for cp in cps { bound.push(cp.name.clone()); }
            collect_free_vars(body, params, bound, out);
            bound.truncate(mark);
        }
        ExprKind::Return(v) | ExprKind::Break(v) => { if let Some(x) = v { collect_free_vars(x, params, bound, out); } }
        ExprKind::Range { lo, hi, .. } => {
            if let Some(l) = lo { collect_free_vars(l, params, bound, out); }
            if let Some(h) = hi { collect_free_vars(h, params, bound, out); }
        }
    }
}

fn collect_free_in_block(b: &Block, params: &[&str], bound: &mut Vec<String>, out: &mut Vec<String>) {
    let mark = bound.len();
    for s in &b.stmts {
        match s {
            Stmt::Let { pattern, init, .. } => {
                if let Some(e) = init { collect_free_vars(e, params, bound, out); }
                bind_pattern_names(pattern, bound);
            }
            Stmt::Expr(e) => collect_free_vars(e, params, bound, out),
            Stmt::Item(_) => {}
        }
    }
    if let Some(t) = &b.tail { collect_free_vars(t, params, bound, out); }
    bound.truncate(mark);
}

fn bind_pattern_names(p: &Pattern, bound: &mut Vec<String>) {
    match &p.kind {
        PatternKind::Binding { name, .. } => bound.push(name.clone()),
        PatternKind::Variant { payload, .. } => for sp in payload { bind_pattern_names(sp, bound); },
        PatternKind::Struct { fields, .. } => for (_, sp) in fields { bind_pattern_names(sp, bound); },
        PatternKind::Tuple(ps) => for sp in ps { bind_pattern_names(sp, bound); },
        PatternKind::At { name, pat, .. } => { bound.push(name.clone()); bind_pattern_names(pat, bound); }
        PatternKind::Wildcard | PatternKind::Literal(_) => {}
    }
}

/// A human-readable rendering of a type for diagnostics.
/// The value of the first plain assignment `name = value` in `e`, outside
/// closures.
fn first_assignment(e: &Expr, name: &str) -> Option<Expr> {
    match &*e.kind {
        ExprKind::Assign { target, op: None, value } if matches!(&*target.kind, ExprKind::Path(p) if p.is_single() && p.last() == name) => Some(value.clone()),
        ExprKind::Closure { .. } => None,
        ExprKind::Block(b) | ExprKind::Unsafe(b) | ExprKind::Loop { body: b } => first_assignment_block(b, name),
        ExprKind::If { cond, then_branch, else_branch } => first_assignment(cond, name)
            .or_else(|| first_assignment_block(then_branch, name))
            .or_else(|| else_branch.as_ref().and_then(|e| first_assignment(e, name))),
        ExprKind::While { cond, body } => first_assignment(cond, name).or_else(|| first_assignment_block(body, name)),
        ExprKind::For { iter, body, .. } => first_assignment(iter, name).or_else(|| first_assignment_block(body, name)),
        ExprKind::Match { scrutinee, arms } => first_assignment(scrutinee, name)
            .or_else(|| arms.iter().find_map(|a| first_assignment(&a.body, name))),
        ExprKind::Break(Some(v)) | ExprKind::Return(Some(v)) => first_assignment(v, name),
        _ => None,
    }
}

fn first_assignment_block(b: &Block, name: &str) -> Option<Expr> {
    b.stmts.iter().find_map(|s| match s {
        Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) => first_assignment(e, name),
        _ => None,
    }).or_else(|| b.tail.as_ref().and_then(|t| first_assignment(t, name)))
}

/// What the lowering knows about an enclosing loop: whether a `break` leaves
/// it, and the type of its `break value`s (a `loop` only).
#[derive(Clone, Debug, Default, PartialEq)]
struct LoopExit {
    exits: bool,
    takes_value: bool,
    value: Option<Ty>,
}

thread_local! {
    /// Explicit type arguments (`f::<T>()`) for the call about to be lowered:
    /// (on the type segment, types). Taken by `call` on entry.
    static TURBOFISH: std::cell::RefCell<Option<(bool, Vec<Ty>)>> = const { std::cell::RefCell::new(None) };
    /// A method call's type arguments, held while its receiver is lowered,
    /// then handed to `method_call_operands`.
    static METHOD_TURBOFISH: std::cell::RefCell<Option<Vec<Ty>>> = const { std::cell::RefCell::new(None) };
    static OPERAND_TURBOFISH: std::cell::RefCell<Option<Vec<Ty>>> = const { std::cell::RefCell::new(None) };
}

/// `(inner, marker, type args)` for an expression the parser tagged with
/// explicit type arguments (`parser::turbofish_marker`).
fn turbofish_of(e: &Expr) -> Option<(&Expr, &str, &Vec<Type>)> {
    let ExprKind::Cast(inner, ty) = &*e.kind else { return None };
    let TypeKind::Path(p, args) = &ty.kind else { return None };
    (p.is_single() && matches!(p.last(), "turbofish#" | "typeargs#")).then(|| (inner, p.last(), args))
}

/// Does `ty` contain an `impl Trait` result placeholder?
fn has_opaque(ty: &Ty) -> bool {
    match ty {
        Ty::Named { name, args } => name.rsplit("::").next().is_some_and(|n| n.starts_with("impl#")) || args.iter().any(has_opaque),
        Ty::Tuple(es) => es.iter().any(has_opaque),
        Ty::Array(e, _) => has_opaque(e),
        Ty::Fn { params, ret } | Ty::ExternFn { params, ret } => params.iter().any(has_opaque) || has_opaque(ret),
        Ty::Prim(_) | Ty::Var(_) | Ty::Infer(_) => false,
    }
}

/// The trait a `dyn Trait` object type stands for (`geo::dyn#Shape` is
/// `geo::Shape`).
pub(crate) fn dyn_trait(ty: &Ty) -> Option<String> {
    let Ty::Named { name, .. } = ty else { return None };
    let (module, last) = match name.rsplit_once("::") { Some((m, l)) => (Some(m), l), None => (None, name.as_str()) };
    let t = last.strip_prefix("dyn#")?;
    Some(match module { Some(m) => format!("{m}::{t}"), None => t.to_string() })
}

pub(crate) fn ty_display(ty: &Ty) -> String {
    match ty {
        Ty::Prim(p) => crate::types::prim_name(*p).to_string(),
        Ty::Named { name, args } => {
            let base = name.rsplit("::").next().unwrap();
            if let (Some(assoc), Some(of)) = (base.strip_prefix("proj#"), args.first()) {
                return format!("{}::{}", ty_display(of), assoc);
            }
            let base = match base.strip_prefix("dyn#") { Some(t) => format!("dyn {}", t), None => base.to_string() };
            if args.is_empty() { base }
            else { format!("{}<{}>", base, args.iter().map(ty_display).collect::<Vec<_>>().join(", ")) }
        }
        Ty::Var(v) => v.clone(),
        Ty::Array(e, n) => format!("[{}; {}]", ty_display(e), n),
        Ty::Tuple(es) => format!("({})", es.iter().map(ty_display).collect::<Vec<_>>().join(", ")),
        Ty::Fn { params, ret } => format!("fn({}) -> {}", params.iter().map(ty_display).collect::<Vec<_>>().join(", "), ty_display(ret)),
        Ty::ExternFn { params, ret } => format!("extern fn({}) -> {}", params.iter().map(ty_display).collect::<Vec<_>>().join(", "), ty_display(ret)),
        Ty::Infer(_) => "_".to_string(),
    }
}

/// The element type of an `Array<T>` or a fixed-length `[T; N]` (which share
/// the runtime array object).
fn array_elem_ty(ty: &Ty) -> Option<Ty> {
    match ty {
        Ty::Named { name, args } if name == "Array" && !args.is_empty() => Some(args[0].clone()),
        Ty::Array(elem, _) => Some((**elem).clone()),
        _ => None,
    }
}

fn float_intrinsic(name: &str) -> Option<FloatIntrinsic> {
    Some(match name {
        "sqrt" => FloatIntrinsic::Sqrt,
        "abs" => FloatIntrinsic::Abs,
        "floor" => FloatIntrinsic::Floor,
        "ceil" => FloatIntrinsic::Ceil,
        _ => return None,
    })
}

fn ty_mangle(t: &Ty) -> String {
    match t {
        Ty::Prim(p) => format!("{:?}", p),
        Ty::Named { name, args } => if args.is_empty() { name.replace("::", ".") } else {
            format!("{}.{}", name.replace("::", "."), args.iter().map(ty_mangle).collect::<Vec<_>>().join("."))
        },
        Ty::Var(v) => format!("V{}", v),
        Ty::Array(e, n) => format!("A{}x{}", ty_mangle(e), n),
        Ty::Tuple(es) => format!("T{}", es.iter().map(ty_mangle).collect::<Vec<_>>().join("_")),
        Ty::Fn { params, ret } => format!("F{}_{}", params.iter().map(ty_mangle).collect::<Vec<_>>().join("_"), ty_mangle(ret)),
        Ty::ExternFn { params, ret } => format!("XF{}_{}", params.iter().map(ty_mangle).collect::<Vec<_>>().join("_"), ty_mangle(ret)),
        Ty::Infer(n) => format!("I{}", n),
    }
}

/// Is `repr` allowed to cross the FFI boundary? Scalars are blittable; a value
/// struct is blittable iff it is a plain struct (not a value enum) whose fields
/// are all transitively blittable. Managed heap (`Ref`) types and `Unit` are
/// not. See `docs/ffi.md`.
/// Expected argument kind for a runtime-call intrinsic (atomics etc.).
#[derive(Clone, Copy)]
enum ArgKind { Int, Ptr }

fn repr_is_blittable(repr: &Repr, values: &[ValueLayout]) -> bool {
    match repr {
        Repr::Scalar(_) => true,
        Repr::Value(vid) => {
            let v = &values[*vid as usize];
            // Value enums carry a tag union whose layout we don't yet expose as
            // a C-stable type; only plain value structs cross.
            v.variants.is_none() && v.fields.iter().all(|f| repr_is_blittable(f, values))
        }
        Repr::Ref(_) | Repr::Unit => false,
    }
}

// A node is retained by each entry, so an address cannot be recycled into an
// unrelated expression. The key also compares the span: desugaring can reuse
// a kind at a different source location. Failures are reusable only under the
// same complete state, just like successful type facts.
type ProbeLocals = (HashMap<String, Ty>, Vec<HashMap<String, (LocalId, Ty, bool)>>,
    Vec<Repr>, Vec<Option<String>>, Vec<Ty>, Ty, bool, LetInference, usize,
    Vec<LocalId>, Vec<LoopExit>, bool);
struct ProbeEntry {
    _node: std::sync::Arc<ExprKind>,
    span: Span,
    expected: Option<Ty>,
    locals: ProbeLocals,
    registry: crate::layout::LayoutProbeState,
    mono: Mono,
    count: u32,
    result: LResult<Ty>,
}

struct FnLowerer<'a, 'r, 'm> {
    ctx: &'a TyCtx,
    reg: &'r mut LayoutRegistry<'a>,
    mono: &'m mut Mono,
    count: &'m mut u32,
    /// Type-parameter substitution for the current instantiation (empty if the
    /// enclosing function is non-generic).
    subst: HashMap<String, Ty>,
    /// Lexical scope: name → (LocalId, Ty, is_mut). `is_mut` drives the
    /// immutable-by-default mutability discipline (see `docs/mutability.md`).
    scope: Vec<HashMap<String, (LocalId, Ty, bool)>>,
    /// All locals' reprs, indexed by LocalId.
    locals: Vec<Repr>,
    /// Parallel to `locals`: each local's source name, when it has one (set by
    /// `bind_mut`). `None` for compiler temps. Carried onto `CoreFn.local_names`
    /// for DWARF locals (debugger P3).
    local_names: Vec<Option<String>>,
    /// Parallel: each local's semantic Ty (for checking).
    local_tys: Vec<Ty>,
    ret_ty: Ty,
    /// True until an unannotated closure obtains a return constraint.
    infer_ret: bool,
    /// Inference for unannotated `let`s; see `LetInference`.
    infer: LetInference,
    /// Lexical only: reset when checking a lifted closure or another function.
    unsafe_depth: usize,
    deferred_locals: Vec<LocalId>,
    loop_exits: Vec<LoopExit>,
    type_probe: bool,
    probe_cache: Rc<std::cell::RefCell<HashMap<usize, Vec<ProbeEntry>>>>,
}

/// Identifies a `let` for inference: its statement span plus the bound name
/// (macro expansions give several `let`s the same span).
type SpanKey = (u32, u32, crate::lexer::SourceId, String);
fn span_key(s: Span, name: &str) -> SpanKey { (s.start, s.end, s.source, name.to_string()) }

/// State for inferring the type of `let x = Vec::new();` from a later use.
///
/// Lowering is single-pass and assigns representations immediately, so a
/// `let` whose initializer leaves type parameters open cannot be lowered in
/// place. Instead `lower_fn` retries: a normal attempt records the open `let`
/// (`needs`); a probe attempt binds every open `let` to a placeholder type
/// (`Ty::Infer` ids) and stops at the first type check that pins one
/// (`solution`); the next attempt lowers that `let` with the solved type.
#[derive(Clone, Default, PartialEq)]
struct LetInference {
    /// Run backward argument discovery only after ordinary lowering failed.
    /// Common calls retain the direct source-order path and its linear work.
    arguments: bool,
    /// Types solved by earlier probes, keyed by the `let` statement's span.
    solved_lets: HashMap<SpanKey, Ty>,
    /// True during a probe attempt.
    probing: bool,
    /// Placeholder id -> the `let` it stands for.
    placeholders: HashMap<u32, SpanKey>,
    /// Each open `let`'s type, with placeholders for its unknowns.
    templates: Vec<(SpanKey, Ty)>,
    /// Placeholder solutions found so far in this probe.
    solved: HashMap<u32, Ty>,
    next_placeholder: u32,
    /// Output of a normal attempt: the first open `let` and its binding name.
    needs: Option<(Span, String)>,
    /// Output of a probe: the first `let` whose type became fully known.
    solution: Option<(SpanKey, Ty)>,
    /// The partially known type behind the most recent "cannot infer" error.
    unresolved: Option<Ty>,
}

/// Is `t` (or does it contain) a `let` placeholder?
fn has_placeholder(t: &Ty, placeholders: &HashMap<u32, SpanKey>) -> bool {
    match t {
        Ty::Infer(n) => placeholders.contains_key(n),
        Ty::Named { args, .. } | Ty::Tuple(args) => args.iter().any(|a| has_placeholder(a, placeholders)),
        Ty::Array(e, _) => has_placeholder(e, placeholders),
        Ty::Fn { params, ret } | Ty::ExternFn { params, ret } => {
            params.iter().any(|p| has_placeholder(p, placeholders)) || has_placeholder(ret, placeholders)
        }
        Ty::Var(_) | Ty::Prim(_) => false,
    }
}

/// Match `pattern` against `concrete`, recording each placeholder in
/// `pattern` that lines up with a fully known type.
fn bind_placeholders(pattern: &Ty, concrete: &Ty, placeholders: &HashMap<u32, SpanKey>, out: &mut HashMap<u32, Ty>) {
    match (pattern, concrete) {
        (Ty::Infer(n), _) if placeholders.contains_key(n) => {
            if !has_var(concrete) && !is_never(concrete) {
                out.insert(*n, concrete.clone());
            }
        }
        (Ty::Named { name: pn, args: pa }, Ty::Named { name: cn, args: ca }) if pn == cn && pa.len() == ca.len() => {
            for (p, c) in pa.iter().zip(ca) { bind_placeholders(p, c, placeholders, out); }
        }
        (Ty::Tuple(pa), Ty::Tuple(ca)) if pa.len() == ca.len() => {
            for (p, c) in pa.iter().zip(ca) { bind_placeholders(p, c, placeholders, out); }
        }
        (Ty::Array(pe, _), Ty::Array(ce, _)) => bind_placeholders(pe, ce, placeholders, out),
        (Ty::Fn { params: pp, ret: pr }, Ty::Fn { params: cp, ret: cr }) if pp.len() == cp.len() => {
            for (p, c) in pp.iter().zip(cp) { bind_placeholders(p, c, placeholders, out); }
            bind_placeholders(pr, cr, placeholders, out);
        }
        _ => {}
    }
}

/// Replace solved placeholders in `t`.
fn fill_placeholders(t: &Ty, solved: &HashMap<u32, Ty>) -> Ty {
    match t {
        Ty::Infer(n) => solved.get(n).cloned().unwrap_or_else(|| t.clone()),
        Ty::Named { name, args } => Ty::Named { name: name.clone(), args: args.iter().map(|a| fill_placeholders(a, solved)).collect() },
        Ty::Tuple(args) => Ty::Tuple(args.iter().map(|a| fill_placeholders(a, solved)).collect()),
        Ty::Array(e, n) => Ty::Array(Box::new(fill_placeholders(e, solved)), *n),
        Ty::Fn { params, ret } => Ty::Fn { params: params.iter().map(|p| fill_placeholders(p, solved)).collect(), ret: Box::new(fill_placeholders(ret, solved)) },
        Ty::ExternFn { params, ret } => Ty::ExternFn { params: params.iter().map(|p| fill_placeholders(p, solved)).collect(), ret: Box::new(fill_placeholders(ret, solved)) },
        Ty::Var(_) | Ty::Prim(_) => t.clone(),
    }
}

/// Replace each distinct type variable in `t` with a fresh placeholder id.
fn placeholder_template(t: &Ty, ids: &mut HashMap<String, u32>, next: &mut u32) -> Ty {
    match t {
        Ty::Var(v) => {
            let id = *ids.entry(v.clone()).or_insert_with(|| { let id = *next; *next += 1; id });
            Ty::Infer(id)
        }
        Ty::Named { name, args } => Ty::Named { name: name.clone(), args: args.iter().map(|a| placeholder_template(a, ids, next)).collect() },
        Ty::Tuple(args) => Ty::Tuple(args.iter().map(|a| placeholder_template(a, ids, next)).collect()),
        Ty::Array(e, n) => Ty::Array(Box::new(placeholder_template(e, ids, next)), *n),
        Ty::Fn { params, ret } => Ty::Fn { params: params.iter().map(|p| placeholder_template(p, ids, next)).collect(), ret: Box::new(placeholder_template(ret, ids, next)) },
        Ty::ExternFn { params, ret } => Ty::ExternFn { params: params.iter().map(|p| placeholder_template(p, ids, next)).collect(), ret: Box::new(placeholder_template(ret, ids, next)) },
        Ty::Prim(_) | Ty::Infer(_) => t.clone(),
    }
}

fn lower_fn<'a>(
    job: &Job,
    ctx: &'a TyCtx,
    reg: &mut LayoutRegistry<'a>,
    mono: &mut Mono,
    count: &mut u32,
) -> LResult<CoreFn> {
    let f = &job.f;
    let subst = &job.subst;
    // Intern the function's definition span (for DWARF: its DISubprogram file +
    // line) before `mono` is borrowed by the lowerer. Carries the def's source.
    let fn_span = mono.intern_span(f.span);
    let ret_ty = match &f.ret {
        Some(t) => ground_type(t, subst, ctx)?,
        None => Ty::unit(),
    };

    // Foreign `extern "C"` declaration: no body to lower. Enforce the Phase-A
    // blittable-only rule (scalars only cross the boundary — never a managed
    // GC pointer), then emit a body-less `CoreFn` named after the bare C symbol.
    // See `docs/ffi.md`.
    if f.is_extern {
        let mut reg_lo = FnLowerer {
            ctx, reg, mono, count,
            subst: subst.clone(),
            scope: vec![HashMap::new()],
            locals: vec![],
            local_names: vec![],
            local_tys: vec![],
            ret_ty: ret_ty.clone(),
            infer_ret: false,
            infer: LetInference::default(),
            unsafe_depth: 0,
            deferred_locals: vec![],
            loop_exits: vec![],
            type_probe: false,
            probe_cache: Rc::new(std::cell::RefCell::new(HashMap::new())),
        };
        let mut params = Vec::new();
        let mut extern_by_ref = Vec::new();
        for p in &f.params {
            let ty = ground_type(&p.ty, subst, ctx)?;
            let repr = reg_lo.repr_of(&ty, p.span)?;
            // Scalars cross by value; `#[repr(C)]` value structs of transitively
            // blittable fields cross either by value (non-`mut`, per the C ABI) or
            // by pointer (a `mut` out-param — `gettimeofday(struct timeval*)`).
            // A managed heap (`Ref`) type never crosses. See `docs/ffi.md`.
            if !repr_is_blittable(&repr, &reg_lo.reg.values) {
                return err(
                    format!(
                        "extern function `{}` parameter `{}` has type `{}`, but only scalar \
                         types and value structs of scalar fields may cross the FFI boundary \
                         (a managed/heap type cannot — see docs/ffi.md)",
                        f.name, p.name, ty_display(&ty),
                    ),
                    p.span,
                );
            }
            // A value-struct parameter is by-pointer iff declared `mut`; scalars
            // are always by value (`false`).
            extern_by_ref.push(matches!(repr, Repr::Value(_)) && p.is_mut);
            params.push(repr);
        }
        let ret = reg_lo.repr_of(&ret_ty, f.span)?;
        // A return may be a scalar, nothing, or a blittable value struct that the
        // C ABI returns in registers (≤ 16 bytes — larger needs sret, not yet
        // supported). The codegen coerces it per AAPCS64.
        let ret_ok = match &ret {
            Repr::Scalar(_) | Repr::Unit => true,
            Repr::Value(vid) => {
                repr_is_blittable(&ret, &reg_lo.reg.values)
                    && reg_lo.reg.values[*vid as usize].size <= 16
            }
            _ => false,
        };
        if !ret_ok {
            return err(
                format!(
                    "extern function `{}` returns `{}`, but only scalar types, nothing, or a \
                     value struct of scalar fields ≤ 16 bytes may cross the FFI boundary \
                     (see docs/ffi.md)",
                    f.name, ty_display(&ret_ty),
                ),
                f.span,
            );
        }
        return Ok(CoreFn {
            name: f.name.clone(),
            params,
            extern_by_ref,
            ret,
            locals: vec![],
            local_names: vec![],
            body: CoreBlock { stmts: vec![], tail: None },
            closure_captures: vec![], is_closure: false,
            is_extern: true,
            span: fn_span,
        });
    }

    // Unannotated-`let` inference: retry until no `let` is left open (see
    // `LetInference`). Each discarded attempt's monomorphization and layout
    // registrations are rolled back so they cannot leak placeholder types.
    let mut solved_lets: HashMap<SpanKey, Ty> = HashMap::new();
    let mut infer_arguments = false;
    loop {
        let snap = (mono.snapshot(), reg.snapshot(), *count);
        let mut infer = LetInference { solved_lets: solved_lets.clone(), arguments: infer_arguments, ..LetInference::default() };
        let result = lower_fn_attempt(job, ctx, reg, mono, count, &ret_ty, fn_span, &mut infer);
        if result.is_err() && infer.needs.is_none() && !infer_arguments {
            // No constraints were needed on the ordinary path until this
            // failure. Retry from the original function state; discarded IR,
            // closure IDs, layouts and spans cannot escape into the real pass.
            mono.restore(snap.0); reg.restore(snap.1); *count = snap.2;
            infer_arguments = true;
            continue;
        }
        let Some((let_span, name)) = infer.needs.take() else { return result };
        if result.is_ok() { return result; }
        mono.restore(snap.0); reg.restore(snap.1); *count = snap.2;

        let snap = (mono.snapshot(), reg.snapshot(), *count);
        let mut probe = LetInference { solved_lets: solved_lets.clone(), arguments: infer_arguments, probing: true, ..LetInference::default() };
        let _ = lower_fn_attempt(job, ctx, reg, mono, count, &ret_ty, fn_span, &mut probe);
        mono.restore(snap.0); reg.restore(snap.1); *count = snap.2;
        match probe.solution.take() {
            Some((key, ty)) if !solved_lets.contains_key(&key) => { solved_lets.insert(key, ty); }
            _ => {
                if !infer_arguments {
                    infer_arguments = true;
                    continue;
                }
                return err(
                    format!("cannot infer the type of `{}`; add a type annotation (`let {}: Type = ...`)", name, name),
                    let_span,
                );
            }
        }
    }
}

/// One attempt at lowering `job`'s body with the given `let` inference state.
#[allow(clippy::too_many_arguments)]
fn lower_fn_attempt<'a>(
    job: &Job,
    ctx: &'a TyCtx,
    reg: &mut LayoutRegistry<'a>,
    mono: &mut Mono,
    count: &mut u32,
    ret_ty: &Ty,
    fn_span: crate::core::SpanId,
    infer: &mut LetInference,
) -> LResult<CoreFn> {
    let ret_ty = ret_ty.clone();
    let mut lo = FnLowerer {
        ctx,
        reg,
        mono,
        count,
        subst: job.subst.clone(),
        scope: vec![HashMap::new()],
        locals: vec![],
        local_names: vec![],
        local_tys: vec![],
        ret_ty: ret_ty.clone(),
        infer_ret: false,
        infer: std::mem::take(infer),
        // An `unsafe fn`'s body is unsafe context.
        unsafe_depth: job.f.is_unsafe as usize,
        deferred_locals: vec![],
        loop_exits: vec![],
        type_probe: false,
        probe_cache: Rc::new(std::cell::RefCell::new(HashMap::new())),
    };
    let result = lower_fn_with(&mut lo, job, &ret_ty, fn_span);
    *infer = std::mem::take(&mut lo.infer);
    result
}

fn lower_fn_with(lo: &mut FnLowerer<'_, '_, '_>, job: &Job, ret_ty: &Ty, fn_span: crate::core::SpanId) -> LResult<CoreFn> {
    let f = &job.f;
    let ret_ty = ret_ty.clone();
    let mut params = Vec::new();
    // A method's implicit `self` parameter comes first.
    if f.has_self {
        let self_ty = job.self_ty.clone()
            .ok_or_else(|| LowerError { msg: "method without a self type".into(), span: f.span })?;
        let repr = lo.repr_of(&self_ty, f.span)?;
        let id = lo.fresh_local(repr.clone(), self_ty.clone());
        lo.bind_mut("self", id, self_ty, f.self_is_mut);
        params.push(repr);
    }
    for p in &f.params {
        let ty = ground_type(&p.ty, &lo.subst.clone(), lo.ctx)?;
        let repr = lo.repr_of(&ty, p.span)?;
        let id = lo.fresh_local(repr.clone(), ty.clone());
        lo.bind_mut(&p.name, id, ty, p.is_mut);
        params.push(repr);
    }

    // An `impl Trait` result is whatever the body returns.
    let opaque = has_opaque(&ret_ty);
    if opaque { lo.infer_ret = true; }
    let (body, body_ty) = lo.block_expected(&f.body, if opaque { None } else { Some(&ret_ty) })?;
    lo.check_definite_init(&body, f.body.span)?;
    // Point a body/return type mismatch at the tail expression (or the function
    // body if there's no tail), so the caret lands on the offending value.
    let body_err_span = f.body.tail.as_ref().map(|t| t.span).unwrap_or(f.body.span);
    let ret_ty = if opaque {
        // Without a `return`, the tail's type; otherwise the first return's.
        let concrete = if lo.infer_ret { body_ty.clone() } else { lo.ret_ty.clone() };
        if !lo.infer_ret && !is_never(&body_ty) { lo.check_assignable(&body_ty, &concrete, body_err_span)?; }
        lo.check_opaque(&ret_ty, &concrete, body_err_span)?;
        lo.mono.opaque.insert(Mono::key(job), concrete.clone());
        concrete
    } else {
        lo.check_assignable(&body_ty, &ret_ty, body_err_span)?;
        ret_ty
    };
    let ret = lo.repr_of(&ret_ty, f.span)?;

    Ok(CoreFn {
        name: job.mangled.clone(),
        params,
        extern_by_ref: vec![],
        ret,
        locals: std::mem::take(&mut lo.locals),
        local_names: std::mem::take(&mut lo.local_names),
        body,
        closure_captures: vec![], is_closure: false,
        is_extern: false,
        span: fn_span,
    })
}

/// Lower a surface type to a ground `Ty`, applying the instantiation subst.
fn ground_type(t: &Type, subst: &HashMap<String, Ty>, ctx: &TyCtx) -> LResult<Ty> {
    let gparams: Vec<String> = subst.keys().cloned().collect();
    let raw = lower_type(t, &gparams, ctx).map_err(conv)?;
    Ok(apply_subst(&raw, subst))
}

/// Substitute type variables in a `Ty` using `subst`.
fn apply_subst(ty: &Ty, subst: &HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Var(v) => subst.get(v).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Named { name, args } => {
            let t = Ty::Named { name: name.clone(), args: args.iter().map(|a| apply_subst(a, subst)).collect() };
            crate::types::normalize_projection(&t).unwrap_or(t)
        }
        Ty::Tuple(es) => Ty::Tuple(es.iter().map(|e| apply_subst(e, subst)).collect()),
        Ty::Array(e, n) => Ty::Array(Box::new(apply_subst(e, subst)), *n),
        Ty::Fn { params, ret } => Ty::Fn {
            params: params.iter().map(|p| apply_subst(p, subst)).collect(),
            ret: Box::new(apply_subst(ret, subst)),
        },
        Ty::ExternFn { params, ret } => Ty::ExternFn {
            params: params.iter().map(|p| apply_subst(p, subst)).collect(),
            ret: Box::new(apply_subst(ret, subst)),
        },
        Ty::Prim(_) | Ty::Infer(_) => ty.clone(),
    }
}

fn conv(e: crate::types::TypeError) -> LowerError {
    LowerError { msg: e.msg, span: e.span }
}

impl<'a, 'r, 'm> FnLowerer<'a, 'r, 'm> {
    /// Type-only speculative lowering. Every mutable field is restored on
    /// success or error; exhaustive destructuring makes added state explicit.
    fn probe_expr_type(&mut self, expression: &Expr, expected: Option<&Ty>) -> LResult<Ty> {
        // Integer checking is pure and does not need any compiler-state copy.
        if let ExprKind::Int(magnitude, suffix) = &*expression.kind {
            return self.integer_literal(*magnitude, *suffix, expected, false, expression.span)
                .map(|(_, ty)| ty);
        }
        let FnLowerer { ctx: _, reg, mono, count, subst, scope, locals, local_names,
            local_tys, ret_ty, infer_ret, infer, unsafe_depth, deferred_locals, loop_exits,
            type_probe, probe_cache: _ } = self;
        let shared = (reg.snapshot(), mono.snapshot(), **count);
        let saved = (subst.clone(), scope.clone(), locals.clone(), local_names.clone(),
            local_tys.clone(), ret_ty.clone(), *infer_ret, infer.clone(), *unsafe_depth,
            deferred_locals.clone(), loop_exits.clone(), *type_probe);
        let registry = reg.probe_state();
        let monomorphizer = (**mono).clone();
        let identity = std::sync::Arc::as_ptr(&expression.kind) as usize;
        if let Some(entries) = self.probe_cache.borrow().get(&identity) {
            if let Some(entry) = entries.iter().find(|entry| entry.span == expression.span
                && entry.expected.as_ref() == expected && entry.locals == saved
                && entry.registry == registry && entry.mono == monomorphizer
                && entry.count == shared.2) {
                return entry.result.clone();
            }
        }
        #[cfg(test)]
        PROBE_WORK.with(|n| n.set(n.get()+1));
        // A type-only probe must not turn an error skipped by let discovery
        // into a fabricated unit-type constraint for another argument.
        self.type_probe = true;
        let result = self.expr(expression, expected).map(|(_, ty)| ty);
        self.reg.restore(shared.0);
        self.mono.restore(shared.1);
        *self.count = shared.2;
        self.probe_cache.borrow_mut().entry(identity).or_default().push(ProbeEntry {
            _node: expression.kind.clone(), span: expression.span,
            expected: expected.cloned(), locals: saved.clone(), registry,
            mono: monomorphizer, count: shared.2, result: result.clone(),
        });
        (self.subst, self.scope, self.locals, self.local_names, self.local_tys,
            self.ret_ty, self.infer_ret, self.infer, self.unsafe_depth,
            self.deferred_locals, self.loop_exits, self.type_probe) = saved;
        result
    }

    /// Learn only ground facts from independent speculative arguments. The
    /// real pass performs source-order checking and emits all effects.
    fn infer_argument_types<'e, I>(&mut self, arguments: I, inferred: &mut HashMap<String, Ty>)
    where I: Iterator<Item = (&'e Expr, &'e Ty)> + Clone {
        loop {
            let previous = inferred.clone();
            for (argument, declared) in arguments.clone() {
                let expected = apply_subst(declared, inferred);
                if !has_var(&expected) { continue; }
                if let Ok(actual) = self.probe_expr_type(argument, compound_hint(&expected)) {
                    if !has_var(&actual) && !is_never(&actual) {
                        unify_infer(declared, &actual, inferred);
                    }
                }
            }
            if previous == *inferred { break; }
        }
    }

    /// Enforce both spellings of a generic constraint through one path.
    fn check_generic_bounds(&self, generics: &Generics, scope: &[String], subst: &HashMap<String, Ty>, required_by: &str, span: Span) -> LResult<()> {
        let instantiate = |ty: &Type| lower_type(ty, scope, self.ctx)
            .map(|ty| apply_subst(&ty, subst)).map_err(conv);
        let check = |ty: &Ty, bound: &TraitRef| -> LResult<()> {
            let resolved = self.infer.probing.then(|| fill_placeholders(ty, &self.infer.solved));
            let ty = resolved.as_ref().unwrap_or(ty);
            let name = bound.path.segments.join("::");
            let arguments = bound.args.iter().map(&instantiate).map(|argument| argument.map(|ty| {
                if self.infer.probing { fill_placeholders(&ty, &self.infer.solved) } else { ty }
            })).collect::<LResult<Vec<_>>>()?;
            // Probe IR is discarded. Defer only obligations that still depend
            // on registered let placeholders; the ground retry checks them all.
            if self.infer.probing && (has_placeholder(ty, &self.infer.placeholders)
                || arguments.iter().any(|ty| has_placeholder(ty, &self.infer.placeholders))) {
                return Ok(());
            }
            let satisfied = if name == "Sync" && arguments.is_empty() {
                self.ctx.try_is_sync(ty).map_err(|error| LowerError { msg: error.msg, span })?
            } else { self.ctx.try_type_implements_trait_args(ty, &name, &arguments).map_err(|error| LowerError { msg: error.msg, span })? };
            if satisfied {
                // `Iterator<Item = T>`: the associated type must be `T`.
                for (assoc, written) in &bound.bindings {
                    let want = instantiate(written)?;
                    if has_var(&want) || has_var(ty) { continue; }
                    let got = self.ctx.associated_type_of(ty, &name, &arguments, assoc);
                    if got.as_ref() != Some(&want) {
                        return err(format!(
                            "expected `{}::{}` to be `{}`, found `{}` (required by `{required_by}`)",
                            ty_display(ty), assoc, ty_display(&want), got.as_ref().map(ty_display).unwrap_or_else(|| "nothing".into())
                        ), span);
                    }
                }
                return Ok(())
            }
            let label = if arguments.is_empty() { name } else {
                format!("{name}<{}>", arguments.iter().map(ty_display).collect::<Vec<_>>().join(", "))
            };
            err(format!("the trait bound `{}: {label}` is not satisfied (required by `{required_by}`)", ty_display(ty)), span)
        };
        for parameter in &generics.params {
            if parameter.bounds.is_empty() { continue; }
            let ty = subst.get(&parameter.name).ok_or_else(|| LowerError {
                msg: format!("cannot infer type parameter `{}` for `{required_by}`", parameter.name), span,
            })?;
            for bound in &parameter.bounds { check(ty, bound)?; }
        }
        for predicate in &generics.where_clauses {
            let ty = instantiate(&predicate.ty)?;
            for bound in &predicate.bounds { check(&ty, bound)?; }
        }
        Ok(())
    }

    /// Infer type parameters named only in associated type bindings: with
    /// `I: Iterator<Item = T>` and `I` known, `T` is `I`'s `Item`.
    fn infer_from_bindings(&self, generics: &Generics, scope: &[String], subst: &mut HashMap<String, Ty>) -> LResult<()> {
        let mut bounded: Vec<(Ty, &TraitRef)> = Vec::new();
        for p in &generics.params {
            if let Some(t) = subst.get(&p.name) { for b in &p.bounds { bounded.push((t.clone(), b)); } }
        }
        for w in &generics.where_clauses {
            let t = apply_subst(&lower_type(&w.ty, scope, self.ctx).map_err(conv)?, subst);
            for b in &w.bounds { bounded.push((t.clone(), b)); }
        }
        for (ty, bound) in bounded {
            if has_var(&ty) { continue; }
            let args: Vec<Ty> = bound.args.iter().filter_map(|a| lower_type(a, scope, self.ctx).ok()).map(|t| apply_subst(&t, subst)).collect();
            for (assoc, written) in &bound.bindings {
                let declared = lower_type(written, scope, self.ctx).map_err(conv)?;
                if let Some(actual) = self.ctx.associated_type_of(&ty, &bound.path.segments.join("::"), &args, assoc) {
                    unify_infer(&declared, &actual, subst);
                }
            }
        }
        Ok(())
    }

    fn check_alias_bounds(&self, name: &str, actual: &Ty, span: Span) -> LResult<()> {
        let Some(alias) = self.ctx.aliases.get(name) else { return Ok(()); };
        if alias.generics.params.iter().all(|p| p.bounds.is_empty()) && alias.generics.where_clauses.is_empty() { return Ok(()); }
        let inferred = crate::types::type_pattern_bindings(&alias.target, actual).ok_or_else(|| LowerError {
            msg: format!("alias `{name}` does not match specialization `{}`", ty_display(actual)), span,
        })?;
        // All parts of every obligation must be known. In particular, an erased
        // alias parameter cannot be guessed from its trait bound.
        let instantiate = |ty: &Type| -> LResult<Ty> {
            let ty = apply_subst(&lower_type(ty, &alias.params, self.ctx).map_err(conv)?, &inferred);
            if has_var(&ty) { return err(format!("cannot infer type arguments required by alias `{name}`"), span); }
            Ok(ty)
        };
        for parameter in &alias.generics.params {
            if parameter.bounds.is_empty() { continue; }
            if !inferred.get(&parameter.name).is_some_and(|ty| !has_var(ty)) {
                return err(format!("cannot infer type parameter `{}` required by alias `{name}`", parameter.name), span);
            }
            for bound in &parameter.bounds { for argument in &bound.args { instantiate(argument)?; } }
        }
        for predicate in &alias.generics.where_clauses {
            instantiate(&predicate.ty)?;
            for bound in &predicate.bounds { for argument in &bound.args { instantiate(argument)?; } }
        }
        self.check_generic_bounds(&alias.generics, &alias.params, &inferred, name, span)
    }

    fn integer_literal(
        &self,
        magnitude: u64,
        suffix: crate::lexer::NumSuffix,
        expected: Option<&Ty>,
        negative: bool,
        span: Span,
    ) -> LResult<(CoreExpr, Ty)> {
        let prim = match expected {
            Some(Ty::Prim(p)) if is_int_prim(*p) && suffix == crate::lexer::NumSuffix::None => *p,
            _ => suffix_prim(suffix, false),
        };
        let repr = scalar_of(prim).unwrap();
        let bits = repr.bits();
        let maximum = if repr.is_signed() {
            (1u64 << (bits - 1)) - u64::from(!negative)
        } else if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        if magnitude > maximum {
            return err(
                format!(
                    "integer literal out of range for `{}`",
                    ty_display(&Ty::Prim(prim))
                ),
                span,
            );
        }
        let value = if negative {
            0u64.wrapping_sub(magnitude)
        } else {
            magnitude
        };
        Ok((
            CoreExpr::new(CoreExprKind::ConstInt(value, repr), Repr::Scalar(repr)),
            Ty::Prim(prim),
        ))
    }

    fn check_definite_init(&self, body: &CoreBlock, fallback: Span) -> LResult<()> {
        crate::definite_init::check(body, &self.deferred_locals).map_err(|(local, span)| {
            let name = self.local_names[local as usize]
                .as_deref()
                .unwrap_or("local");
            LowerError {
                msg: format!(
                    "`{name}` may be uninitialized; assign it on every path before reading it"
                ),
                span: span
                    .checked_sub(1)
                    .and_then(|id| self.mono.spans.get(id as usize))
                    .copied()
                    .unwrap_or(fallback),
            }
        })
    }

    fn require_unsafe(&self, operation: &str, span: Span) -> LResult<()> {
        if self.unsafe_depth == 0 {
            return err(
                format!("{operation} requires an explicit `unsafe {{ ... }}` block"),
                span,
            );
        }
        Ok(())
    }

    /// Repr of a type, via the shared layout registry.
    fn repr_of(&mut self, ty: &Ty, span: Span) -> LResult<Repr> {
        if is_never(ty) { return Ok(Repr::Unit); }
        if has_placeholder(ty, &self.infer.placeholders) {
            // Only a probe has placeholders, and its output is discarded: a
            // stand-in repr lets it keep gathering constraints.
            return Ok(Repr::Unit);
        }
        if has_var(ty) { self.infer.unresolved = Some(ty.clone()); }
        self.reg.repr(ty).map_err(|e| LowerError { msg: e.0, span })
    }

    /// `actual` must be usable where `expected` is required. During a probe a
    /// check that lines a `let` placeholder up with a known type solves it;
    /// the probe stops at the first `let` whose type is then fully known.
    fn check_assignable(&mut self, actual: &Ty, expected: &Ty, span: Span) -> LResult<()> {
        let inf = &mut self.infer;
        if inf.probing && (has_placeholder(actual, &inf.placeholders) || has_placeholder(expected, &inf.placeholders)) {
            let mut found = HashMap::new();
            bind_placeholders(expected, actual, &inf.placeholders, &mut found);
            bind_placeholders(actual, expected, &inf.placeholders, &mut found);
            inf.solved.extend(found);
            for (key, template) in &inf.templates {
                let ty = fill_placeholders(template, &inf.solved);
                if !has_placeholder(&ty, &inf.placeholders) {
                    inf.solution = Some((key.clone(), ty));
                    return err("`let` type inference probe finished", span);
                }
            }
            return Ok(());
        }
        check_assignable(actual, expected, span)
    }

    fn fresh_local(&mut self, repr: Repr, ty: Ty) -> LocalId {
        let id = self.locals.len() as LocalId;
        self.locals.push(repr);
        self.local_names.push(None);
        self.local_tys.push(ty);
        id
    }
    /// Bind an immutable local (the default). Use `bind_mut` for `let mut`,
    /// `mut` params, and `mut self`.
    fn bind(&mut self, name: &str, id: LocalId, ty: Ty) {
        self.bind_mut(name, id, ty, false);
    }
    fn bind_mut(&mut self, name: &str, id: LocalId, ty: Ty, is_mut: bool) {
        // Record the source name for this local (DWARF locals, debugger P3). The
        // FIRST binding of a slot names it; later rebindings of the same name get
        // their own fresh slots, so we never clobber a name with a shadow.
        if let Some(slot) = self.local_names.get_mut(id as usize) {
            if slot.is_none() {
                *slot = Some(name.to_string());
            }
        }
        self.scope.last_mut().unwrap().insert(name.to_string(), (id, ty, is_mut));
    }
    fn lookup(&self, name: &str) -> Option<(LocalId, Ty)> {
        for s in self.scope.iter().rev() {
            if let Some((id, ty, _)) = s.get(name) {
                return Some((*id, ty.clone()));
            }
        }
        None
    }
    /// Is the binding `name` mutable in the current scope? `None` if unbound.
    fn lookup_mut(&self, name: &str) -> Option<bool> {
        for s in self.scope.iter().rev() {
            if let Some((_, _, is_mut)) = s.get(name) {
                return Some(*is_mut);
            }
        }
        None
    }

    /// The root binding name of an assignment target's access path:
    /// `a` for `a`, `a.b`, `a.b.c`, `a[i]`, `a.b[i].c`. Used to enforce
    /// transitive (deep) immutability: mutating any place requires the root of
    /// its path to be a `mut` binding. Returns `None` if the path isn't rooted
    /// in a simple variable (e.g. a function-call result).
    fn path_root_name(e: &Expr) -> Option<String> {
        match &*e.kind {
            ExprKind::Path(p) if p.is_single() => Some(p.last().to_string()),
            ExprKind::Field { base, .. } => Self::path_root_name(base),
            ExprKind::Index { base, .. } => Self::path_root_name(base),
            _ => None,
        }
    }

    /// Enforce the immutable-by-default rule for an assignment to `target`:
    /// the root binding of the access path must be declared `mut`. `what`
    /// describes the place for the error message ("variable", "field of",
    /// "element of").
    fn check_mutable_root(&self, target: &Expr, what: &str, span: Span) -> LResult<()> {
        match Self::path_root_name(target) {
            Some(root) => match self.lookup_mut(&root) {
                Some(true) => Ok(()),
                Some(false) => {
                    let fix = if root == "self" {
                        "declare the receiver `mut self`".to_string()
                    } else {
                        format!("declare it `let mut {}`, or take it as `mut {}`", root, root)
                    };
                    err(format!("cannot assign to {} immutable binding `{}` ({})", what, root, fix), span)
                }
                None => err(format!("unknown variable `{}`", root), span),
            },
            // No simple root (e.g. assigning into a temporary) — reject; there's
            // nothing to make `mut`.
            None => err(
                format!("cannot assign to {} a temporary value (it is not rooted in a `mut` binding)", what),
                span,
            ),
        }
    }
    fn push_scope(&mut self) { self.scope.push(HashMap::new()); }
    fn pop_scope(&mut self) { self.scope.pop(); }

    fn block(&mut self, b: &Block) -> LResult<(CoreBlock, Ty)> {
        self.block_expected(b, None)
    }

    fn block_expected(&mut self, b: &Block, expected: Option<&Ty>) -> LResult<(CoreBlock, Ty)> {
        self.push_scope();
        let mut stmts = Vec::new();
        let mut diverges = false;
        for s in &b.stmts {
            if let Err(e) = self.block_stmt(s, b, &mut stmts, &mut diverges) {
                if self.probe_tolerates() { continue; }
                return Err(e);
            }
        }
        let (tail, ty) = match &b.tail {
            Some(e) => {
                let (ce, t) = match self.expr(e, expected) {
                    Ok(v) => v,
                    Err(_) if self.probe_tolerates() => (CoreExpr::new(CoreExprKind::Unit, Repr::Unit), Ty::unit()),
                    Err(e) => return Err(e),
                };
                // P2 (DWARF): the block's tail/result expression is steppable.
                let ce = if ce.span == NO_SPAN {
                    let sid = self.mono.intern_span(e.span);
                    ce.at(sid)
                } else {
                    ce
                };
                (Some(ce), t)
            }
            None => (None, Ty::unit()),
        };
        self.pop_scope();
        Ok((CoreBlock { stmts, tail }, if diverges { never_ty() } else { ty }))
    }

    /// During a `let` inference probe, an error that has not solved anything
    /// is skipped so later statements can still contribute constraints. The
    /// probe's output is discarded; the real attempt reports every error.
    fn probe_tolerates(&self) -> bool {
        !self.type_probe && self.infer.probing && self.infer.solution.is_none()
    }

    /// Lower one statement of a block into `stmts`.
    fn block_stmt(&mut self, s: &Stmt, b: &Block, stmts: &mut Vec<CoreStmt>, diverges: &mut bool) -> LResult<()> {
        match s {
            Stmt::Let { pattern, ty, init, span } => {
                let (name, is_mut) = match &pattern.kind {
                    PatternKind::Binding { name, is_mut } => (name.clone(), *is_mut),
                    PatternKind::Wildcard => ("_".to_string(), false),
                    _ => {
                        let Some(init) = init else {
                            return err("a destructuring `let` needs an initializer", *span);
                        };
                        *diverges |= self.let_pattern(pattern, ty.as_ref(), init, *span, stmts)?;
                        return Ok(());
                    }
                };
                match init {
                    Some(e) => {
                        let expected = match ty {
                            Some(t) => Some(ground_type(t, &self.subst.clone(), self.ctx)?),
                            None => self.infer.solved_lets.get(&span_key(*span, &name)).cloned(),
                        };
                        self.infer.unresolved = None;
                        let lowered = self.expr(e, expected.as_ref());
                        let (init_expr, init_ty) = {
                            let (ce, at) = match lowered {
                                Ok(v) => v,
                                Err(error) => {
                                    // An unannotated `let` whose initializer leaves type
                                    // parameters open (`Vec::new()`, `None`): its type
                                    // comes from a later use. See `LetInference`.
                                    let open = self.infer.unresolved.take()
                                        .filter(|_| expected.is_none() && error.span == e.span);
                                    let Some(partial) = open else { return Err(error) };
                                    if !self.infer.probing {
                                        if self.infer.needs.is_none() {
                                            self.infer.needs = Some((*span, name.clone()));
                                        }
                                        return Err(error);
                                    }
                                    let key = span_key(*span, &name);
                                    let mut ids = HashMap::new();
                                    let template = placeholder_template(&partial, &mut ids, &mut self.infer.next_placeholder);
                                    for id in ids.values() { self.infer.placeholders.insert(*id, key.clone()); }
                                    self.infer.templates.push((key, template.clone()));
                                    let id = self.fresh_local(Repr::Unit, template.clone());
                                    self.bind_mut(&name, id, template, is_mut);
                                    return Ok(());
                                }
                            };
                            // A `let x: T = e` whose `e` doesn't match `T` is
                            // an error pointing at `e`.
                            if let Some(exp) = &expected {
                                self.check_assignable(&at, exp, e.span)?;
                            }
                            (ce, at)
                        };
                        // P2 (DWARF): give the statement a source line for
                        // stepping, unless the value already carries a more
                        // precise span (e.g. a construction/alloc node).
                        let init_expr = if init_expr.span == NO_SPAN {
                            let sid = self.mono.intern_span(e.span);
                            init_expr.at(sid)
                        } else {
                            init_expr
                        };
                        *diverges |= is_never(&init_ty);
                        let repr = init_expr.repr.clone();
                        let id = self.fresh_local(repr, init_ty.clone());
                        self.bind_mut(&name, id, init_ty, is_mut);
                        stmts.push(CoreStmt::Let(id, init_expr));
                    }
                    // Deferred initialization: `let mut x: T;`. Requires a type
                    // annotation (nothing to infer from) and `mut` (the binding
                    // will be assigned later — see docs/mutability.md). The slot
                    // is zero-initialized; a later `x = ...` fills it.
                    // Rust's `let x;` is assigned exactly once later; here
                    // the binding is simply mutable.
                    None => {
                        let dty = match ty {
                            Some(t) => ground_type(t, &self.subst.clone(), self.ctx)?,
                            // `let mut x; ... x = e;`: the type of the first
                            // assignment's value, when it can be typed here.
                            None => {
                                let rest = b.stmts.iter().skip_while(|t| !std::ptr::eq(*t, s)).skip(1);
                                let first = rest.filter_map(|t| match t {
                                    Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) => first_assignment(e, &name),
                                    _ => None,
                                }).next().or_else(|| b.tail.as_ref().and_then(|t| first_assignment(t, &name)));
                                let probed = match first {
                                    Some(value) => self.probe_expr_type(&value, None).ok().filter(|t| !has_var(t) && !is_never(t)),
                                    None => None,
                                };
                                probed.ok_or_else(|| LowerError { msg: "cannot infer the type of this `let`; add an annotation (`let mut x: T;`)".into(), span: *span })?
                            }
                        };
                        let repr = self.repr_of(&dty, *span)?;
                        let id = self.fresh_local(repr.clone(), dty.clone());
                        // The result of a labeled `break 'l value` is assigned
                        // on every path that reads it (the break flags guard
                        // the read), which the definite-init analysis can't see.
                        if !name.starts_with("loop#") { self.deferred_locals.push(id); }
                        self.bind_mut(&name, id, dty.clone(), true);
                        // Zero-initialize so the slot is well-defined before the
                        // first assignment (and GC-safe for ref slots → null).
                        let zero = self.zero_value(&repr);
                        stmts.push(CoreStmt::Let(id, zero));
                    }
                }
            }
            Stmt::Expr(e) => {
                let (ce, statement_ty) = self.expr(e, None)?;
                *diverges |= is_never(&statement_ty);
                let ce = if ce.span == NO_SPAN {
                    let sid = self.mono.intern_span(e.span);
                    ce.at(sid)
                } else {
                    ce
                };
                stmts.push(CoreStmt::Expr(ce));
            }
            Stmt::Item(_) => return err("nested items not supported in v0", b.span),
        }
        Ok(())
    }

    /// `let PAT = init;` for a tuple or struct pattern. The pattern must be
    /// irrefutable. Returns whether the initializer diverges.
    fn let_pattern(&mut self, pattern: &Pattern, ty: Option<&Type>, init: &Expr, span: Span, stmts: &mut Vec<CoreStmt>) -> LResult<bool> {
        let expected = match ty {
            Some(t) => Some(ground_type(t, &self.subst.clone(), self.ctx)?),
            None => None,
        };
        let (cinit, ity) = self.expr(init, expected.as_ref())?;
        if let Some(exp) = &expected {
            self.check_assignable(&ity, exp, init.span)?;
        }
        let id = self.fresh_local(cinit.repr.clone(), ity.clone());
        stmts.push(CoreStmt::Let(id, cinit));
        let whole = CoreExpr::new(CoreExprKind::Local(id), self.locals[id as usize].clone());
        let (cond, binds, _) = self.pattern_test(pattern, &whole, &ity, pattern.span)?;
        if cond.is_some() {
            return err("refutable pattern in `let`: use `if let` or `match`", span);
        }
        stmts.extend(binds.into_iter().map(|(bid, value)| CoreStmt::Let(bid, value)));
        Ok(is_never(&ity))
    }

    /// Check + lower an expression. `expected` guides literal defaulting.
    fn expr(&mut self, e: &Expr, expected: Option<&Ty>) -> LResult<(CoreExpr, Ty)> {
        // A value where a trait object is expected becomes that object. Only
        // expressions that pass the expectation on to their results (blocks,
        // branches) see it; anything else is lowered as itself and coerced.
        let passes_on = matches!(&*e.kind, ExprKind::Block(_) | ExprKind::Unsafe(_) | ExprKind::If { .. }
            | ExprKind::Match { .. } | ExprKind::Loop { .. } | ExprKind::Path(_));
        let to_dyn = expected.filter(|t| dyn_trait(t).is_some() && !has_var(t));
        let inner_expected = if to_dyn.is_some() && !passes_on { None } else { expected };
        let (value, ty) = stacker::maybe_grow(256 * 1024, 2 * 1024 * 1024, || self.expr_inner(e, inner_expected))?;
        if let Some(target) = to_dyn {
            // A value, or another trait object (upcasting `dyn Derived` to
            // `dyn Base`), becomes the expected object.
            if dyn_trait(&ty) != dyn_trait(target) && !is_never(&ty) && !has_var(&ty) && !has_var(target) {
                return self.coerce_to_dyn(value, ty, target, e.span);
            }
        }
        Ok((value, ty))
    }

    /// Build the `dyn Trait` object for `value`: a `dyn#Trait` struct whose
    /// fields are closures calling each trait method on the value.
    fn coerce_to_dyn(&mut self, value: CoreExpr, ty: Ty, target: &Ty, span: Span) -> LResult<(CoreExpr, Ty)> {
        let Ty::Named { name: object, args: object_args } = target else { unreachable!() };
        let trait_name = dyn_trait(target).unwrap();
        let own = self.ctx.traits.get(&trait_name).map_or(0, |d| d.generics.params.len());
        let trait_args = &object_args[..own.min(object_args.len())];
        if !self.ctx.try_type_implements_trait_args(&ty, &trait_name, trait_args).map_err(conv)? {
            return err(format!("`{}` does not implement `{}`, so it cannot be used as `{}`", ty_display(&ty), trait_name.rsplit("::").next().unwrap(), ty_display(target)), span);
        }
        // `dyn Iterator<Item = i64>` needs a value whose `Item` is `i64`.
        if let Some(def) = self.ctx.structs.get(object) {
            for (param, want) in def.generics.params.iter().zip(object_args.iter()).skip(own) {
                let assoc = param.name.trim_start_matches("assoc#");
                let got = self.ctx.associated_type(&ty, assoc);
                if got.as_ref() != Some(want) {
                    return err(format!("`{}::{}` is `{}`, but `{}` needs `{}`", ty_display(&ty), assoc, got.as_ref().map(ty_display).unwrap_or_else(|| "undefined".into()), ty_display(target), ty_display(want)), span);
                }
            }
        }
        let Some(def) = self.ctx.structs.get(object).cloned() else { return err(format!("unknown trait object `{}`", ty_display(target)), span) };
        let StructBody::Named(fields) = &def.body else { unreachable!() };
        let id = self.fresh_local(value.repr.clone(), ty.clone());
        self.push_scope();
        self.bind_mut("dyn#value", id, ty.clone(), true);
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let var = |n: &str| ex(ExprKind::Path(Path::single(n.to_string(), span)));
        let inits = fields.iter().map(|f| {
            let TypeKind::Fn(params, _) = &f.ty.kind else { unreachable!() };
            let names: Vec<String> = (0..params.len()).map(|i| format!("dyn#arg{i}")).collect();
            // `Trait::method(value, ..)`: names the declaring trait, so a
            // method name shared by two traits (`fmt`) is unambiguous.
            let call = match self.declaring_trait(&trait_name, &f.name) {
                Some(declaring) => {
                    let mut segments: Vec<String> = declaring.split("::").map(str::to_string).collect();
                    segments.push(f.name.clone());
                    let mut call_args = vec![var("dyn#value")];
                    call_args.extend(names.iter().map(|n| var(n)));
                    ex(ExprKind::Call(ex(ExprKind::Path(Path { segments, span })), call_args))
                }
                None => ex(ExprKind::MethodCall {
                    recv: var("dyn#value"),
                    method: f.name.clone(),
                    args: names.iter().map(|n| var(n)).collect(),
                    span,
                }),
            };
            let closure = ex(ExprKind::Closure {
                params: names.iter().map(|n| ClosureParam { name: n.clone(), ty: None, span }).collect(),
                ret: None,
                body: call,
            });
            FieldInit { name: f.name.clone(), value: Some(closure), span }
        }).collect();
        let lit = ex(ExprKind::StructLit { path: Path { segments: object.split("::").map(str::to_string).collect(), span }, fields: inits, span });
        // The object's own literal is built at the target type (not coerced).
        let result = self.expr_inner(&lit, Some(target));
        self.pop_scope();
        let (object_value, object_ty) = result?;
        let repr = object_value.repr.clone();
        Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts: vec![CoreStmt::Let(id, value)], tail: Some(object_value) })), repr), object_ty))
    }

    /// `it.collect()`: build the expected `FromIterator` type from an
    /// iterator (`Vec`, `String`, `HashMap`, ...), or a `Vec` when nothing
    /// is expected. Holes (`Vec<_>`) are filled from the iterator's `Item`.
    fn collect_call(&mut self, recv: &Expr, expected: Option<&Ty>, span: Span) -> LResult<(CoreExpr, Ty)> {
        let (crecv, rty) = self.expr(recv, None)?;
        if let Some(entry) = self.lookup_method(&rty, "collect") {
            return self.method_call_operands(crecv, rty, Some(recv), entry, &[], span);
        }
        // A Vec collects its elements (`s.chars().collect::<String>()`).
        let (crecv, rty) = match (&rty, self.lookup_method(&rty, "iter")) {
            (Ty::Named { name, .. }, Some(entry)) if name == "Vec" => self.method_call_operands(crecv, rty.clone(), None, entry, &[], span)?,
            _ => (crecv, rty),
        };
        let Some(item) = self.ctx.associated_type(&rty, "Item") else {
            return err(format!("no method `collect` on `{}` (it is not an iterator)", ty_display(&rty)), span);
        };
        let target = match expected {
            Some(t) if !has_var(t) => t.clone(),
            Some(Ty::Named { name, args }) => {
                // Fill the holes from the `FromIterator<Item>` impl of that type.
                let mut filled = None;
                for imp in &self.ctx.impls {
                    let Some(tr) = &imp.trait_ref else { continue };
                    if tr.path.last() != "FromIterator" || tr.args.len() != 1 { continue; }
                    let params: Vec<String> = imp.generics.params.iter().map(|p| p.name.clone()).collect();
                    let self_ty = lower_type(&imp.self_ty, &params, self.ctx).map_err(conv)?;
                    let Ty::Named { name: n, .. } = &self_ty else { continue };
                    if n != name { continue; }
                    let arg = lower_type(&tr.args[0], &params, self.ctx).map_err(conv)?;
                    let Some(bound) = crate::types::type_pattern_bindings(&arg, &item) else { continue };
                    let candidate = apply_subst(&self_ty, &bound);
                    let mut holes = HashMap::new();
                    unify_infer(&Ty::Named { name: name.clone(), args: args.clone() }, &candidate, &mut holes);
                    if !has_var(&candidate) { filled = Some(candidate); break; }
                }
                filled.ok_or_else(|| LowerError { msg: format!("cannot collect `{}` items into `{}`", ty_display(&item), ty_display(expected.unwrap())), span })?
            }
            _ => Ty::Named { name: "Vec".into(), args: vec![item.clone()] },
        };
        let Some(base) = crate::types::receiver_base(&target) else {
            return err(format!("cannot collect into `{}`", ty_display(&target)), span);
        };
        let entry = self.lookup_trait_method(&target, "FromIterator", "from_iter")
            .ok_or_else(|| LowerError { msg: format!("`{}` cannot be built from an iterator (no `FromIterator` impl)", ty_display(&target)), span })?;
        let id = self.fresh_local(crecv.repr.clone(), rty.clone());
        self.push_scope();
        self.bind("collect#iter", id, rty);
        let arg = Expr { kind: std::sync::Arc::new(ExprKind::Path(Path::single("collect#iter".into(), span))), span };
        let result = self.assoc_call(&base, Some(&target), None, &entry, std::slice::from_ref(&arg), Some(&target), span);
        self.pop_scope();
        let (value, ty) = result?;
        let repr = value.repr.clone();
        Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts: vec![CoreStmt::Let(id, crecv)], tail: Some(value) })), repr), ty))
    }

    /// The trait in `trait_name`'s supertrait hierarchy that declares `method`.
    fn declaring_trait(&self, trait_name: &str, method: &str) -> Option<String> {
        let mut pending = vec![trait_name.to_string()];
        let mut seen = std::collections::HashSet::new();
        while let Some(t) = pending.pop() {
            if !seen.insert(t.clone()) { continue; }
            let def = self.ctx.traits.get(&t).or_else(|| {
                let mut found = self.ctx.traits.iter().filter(|(k, _)| k.rsplit("::").next() == t.rsplit("::").next());
                let first = found.next()?;
                found.next().is_none().then_some(first.1)
            })?;
            let declares = def.items.iter().any(|i| match i {
                TraitItem::Required(f) => f.name == method,
                TraitItem::Provided(f) => f.name == method,
                TraitItem::AssocType(_) | TraitItem::Const(_) => false,
            });
            if declares { return Some(t); }
            pending.extend(def.supertraits.iter().map(|s| s.path.segments.join("::")));
        }
        None
    }

    /// Check that `concrete`, the type a function body returns, satisfies its
    /// declared `impl Trait` result (the trait, and any `Item = T` bindings).
    fn check_opaque(&self, declared: &Ty, concrete: &Ty, span: Span) -> LResult<()> {
        let Ty::Named { name, args } = declared else { return Ok(()) };
        let (module, last) = match name.rsplit_once("::") { Some((m, l)) => (Some(m), l), None => (None, name.as_str()) };
        let Some(t) = last.strip_prefix("impl#") else {
            return err("`impl Trait` is supported only as the whole result type", span);
        };
        let trait_name = match module { Some(m) => format!("{m}::{t}"), None => t.to_string() };
        let (trait_name, def) = match self.ctx.traits.get(&trait_name) {
            Some(d) => (trait_name.clone(), d.clone()),
            None => {
                let mut found = self.ctx.traits.iter().filter(|(k, _)| k.rsplit("::").next() == Some(t));
                match (found.next(), found.next()) {
                    (Some((k, d)), None) => (k.clone(), d.clone()),
                    _ => return err(format!("unknown trait `{t}`"), span),
                }
            }
        };
        let own = def.generics.params.len();
        if !self.ctx.try_type_implements_trait_args(concrete, &trait_name, &args[..own.min(args.len())]).map_err(conv)? {
            return err(format!("the returned `{}` does not implement `{}`", ty_display(concrete), t), span);
        }
        let mut assocs: Vec<&String> = def.items.iter().filter_map(|i| match i { TraitItem::AssocType(n) => Some(n), _ => None }).collect();
        assocs.sort();
        for (assoc, want) in assocs.into_iter().zip(args.iter().skip(own)) {
            let got = self.ctx.associated_type(concrete, assoc);
            if got.as_ref() != Some(want) {
                return err(format!("the returned `{}` has `{}` = `{}`, not `{}`", ty_display(concrete), assoc, got.as_ref().map(ty_display).unwrap_or_else(|| "undefined".into()), ty_display(want)), span);
            }
        }
        Ok(())
    }

    /// The concrete result of a call whose signature returns `impl Trait`:
    /// lower the callee's body for this instantiation (rolled back; the
    /// worklist lowers it for real) and take the type it returns.
    fn opaque_result(&mut self, job: &Job, declared: Ty, span: Span) -> LResult<Ty> {
        if !has_opaque(&declared) { return Ok(declared); }
        let key = Mono::key(job);
        if let Some(t) = self.mono.opaque.get(&key) { return Ok(t.clone()); }
        if !self.mono.opaque_pending.insert(key.clone()) {
            return err(format!("`{}` returns `impl Trait` and calls itself; its result type cannot be inferred (name the type)", job.f.name), span);
        }
        let snap = (self.mono.snapshot(), self.reg.snapshot(), *self.count);
        let result = lower_fn(job, self.ctx, self.reg, self.mono, self.count);
        self.mono.restore(snap.0);
        self.reg.restore(snap.1);
        *self.count = snap.2;
        self.mono.opaque_pending.remove(&key);
        let lowered = result?;
        let _ = lowered;
        let concrete = self.mono.opaque.get(&key).cloned()
            .ok_or_else(|| LowerError { msg: format!("cannot infer the result type of `{}`", job.f.name), span })?;
        Ok(concrete)
    }

    /// The length a path names in `[x; N]`: a const generic parameter's
    /// value, or a `const` defined as an integer literal.
    fn const_len(&self, p: &Path) -> Option<u64> {
        if p.is_single() {
            if let Some(Ty::Named { name, .. }) = self.subst.get(p.last()) {
                if let Some(n) = name.strip_prefix("const#") { return n.parse().ok(); }
            }
        }
        let (_, def) = self.const_def(p)?;
        match &*def.value.kind { ExprKind::Int(n, _) => Some(*n), _ => None }
    }

    /// The `const`/`static` a value path names: by its full path, a `use`
    /// alias, or an unambiguous final segment.
    fn const_def(&self, path: &Path) -> Option<(String, Rc<ConstDef>)> {
        let written = path.segments.join("::");
        if let Some(d) = self.ctx.consts.get(&written) { return Some((written, d.clone())); }
        // `Type::NAME`, `Self::NAME`, `T::NAME`: an associated constant.
        if path.segments.len() >= 2 {
            let prefix = &path.segments[..path.segments.len() - 1];
            let ty = if prefix.len() == 1 && self.subst.contains_key(&prefix[0]) {
                self.subst.get(&prefix[0]).cloned()
            } else {
                let joined = prefix.join("::");
                let span = path.span;
                lower_type(&Type { kind: TypeKind::Path(Path { segments: prefix.to_vec(), span }, vec![]), span }, &[], self.ctx).ok()
                    .or_else(|| self.ctx.canon(&joined).map(|name| Ty::Named { name, args: vec![] }))
            };
            if let Some(base) = ty.as_ref().and_then(crate::types::receiver_base) {
                if let Some(d) = self.ctx.assoc_consts.get(&(base.clone(), path.last().to_string())) {
                    if matches!(&*d.value.kind, ExprKind::Path(p) if p.last() == "const#required") { return None; }
                    return Some((format!("{base}::{}", path.last()), d.clone()));
                }
            }
        }
        if path.is_single() {
            if let Some(full) = self.ctx.use_aliases.get(&written) {
                if let Some(d) = self.ctx.consts.get(full) { return Some((full.clone(), d.clone())); }
            }
            let mut found = self.ctx.consts.iter().filter(|(k, _)| k.rsplit("::").next() == Some(written.as_str()));
            let first = found.next()?;
            if found.next().is_none() { return Some((first.0.clone(), first.1.clone())); }
        }
        None
    }

    /// A constant's value, inlined where it is used (as Rust does), checked
    /// against its declared type. Its expression sees no local bindings.
    fn const_value(&mut self, name: &str, def: &ConstDef, expected: Option<&Ty>, span: Span) -> LResult<(CoreExpr, Ty)> {
        thread_local! { static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) }; }
        if DEPTH.with(|d| d.get()) > 64 {
            return err(format!("constant `{}` is defined in terms of itself", name), span);
        }
        let ty = ground_type(&def.ty, &HashMap::new(), self.ctx)?;
        let _ = expected;
        let saved = std::mem::replace(&mut self.scope, vec![HashMap::new()]);
        DEPTH.with(|d| d.set(d.get() + 1));
        let result = self.expr(&def.value, Some(&ty));
        DEPTH.with(|d| d.set(d.get() - 1));
        self.scope = saved;
        let (value, actual) = result?;
        self.check_assignable(&actual, &ty, def.value.span)?;
        Ok((value, ty))
    }

    /// `eq`, `cmp`, `hash`, and `clone` on a tuple, element by element
    /// (tuples have no impls; `==` and `<` on tuples come here too).
    fn tuple_method(&mut self, recv: CoreExpr, recv_ty: Ty, method: &str, args: &[Expr], span: Span) -> LResult<(CoreExpr, Ty)> {
        let Ty::Tuple(elems) = &recv_ty else { unreachable!() };
        let n = elems.len();
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let var = |name: &str| ex(ExprKind::Path(Path::single(name.to_string(), span)));
        let elem = |name: &str, i: usize| ex(ExprKind::Field { base: var(name), field: FieldAccess::Tuple(i as u32) });
        let call = |r: Expr, m: &str, args: Vec<Expr>| ex(ExprKind::MethodCall { recv: r, method: m.into(), args, span });
        let ordering = |v: &str| ex(ExprKind::Path(Path { segments: vec!["Ordering".into(), v.into()], span }));
        let arity = |k: usize| if args.len() == k { Ok(()) } else { err(format!("`{}` on a tuple takes {} argument(s)", method, k), span) };
        let other = |stmts: &mut Vec<Stmt>| stmts.push(Stmt::Let {
            pattern: Pattern { kind: PatternKind::Binding { is_mut: false, name: "tup#b".into() }, span },
            ty: None, init: Some(args[0].clone()), span,
        });
        let mut stmts = Vec::new();
        let body = match method {
            "eq" | "ne" => {
                arity(1)?;
                other(&mut stmts);
                let all = (0..n).map(|i| ex(ExprKind::Binary(BinOp::Eq, elem("tup#a", i), elem("tup#b", i))))
                    .reduce(|a, b| ex(ExprKind::Binary(BinOp::And, a, b))).unwrap_or_else(|| ex(ExprKind::Bool(true)));
                if method == "ne" { ex(ExprKind::Unary(UnOp::Not, all)) } else { all }
            }
            "cmp" | "lt" | "le" | "gt" | "ge" => {
                arity(1)?;
                other(&mut stmts);
                let mut chain = ordering("Equal");
                for i in (0..n).rev() {
                    let c = format!("tup#c{i}");
                    chain = ex(ExprKind::Block(Block {
                        stmts: vec![Stmt::Let { pattern: Pattern { kind: PatternKind::Binding { is_mut: false, name: c.clone() }, span }, ty: None, init: Some(call(elem("tup#a", i), "cmp", vec![elem("tup#b", i)])), span }],
                        tail: Some(ex(ExprKind::If {
                            cond: ex(ExprKind::Binary(BinOp::Ne, var(&c), ordering("Equal"))),
                            then_branch: Block { stmts: vec![], tail: Some(var(&c)), span },
                            else_branch: Some(ex(ExprKind::Block(Block { stmts: vec![], tail: Some(chain), span }))),
                        })),
                        span,
                    }));
                }
                match method {
                    "cmp" => chain,
                    m => call(chain, &format!("is_{m}"), vec![]),
                }
            }
            "hash" => {
                arity(0)?;
                (0..n).fold(ex(ExprKind::Int(17, crate::lexer::NumSuffix::None)), |h, i| ex(ExprKind::Binary(BinOp::Add,
                    ex(ExprKind::Binary(BinOp::Mul, h, ex(ExprKind::Int(31, crate::lexer::NumSuffix::None)))),
                    call(elem("tup#a", i), "hash", vec![]))))
            }
            _ => {
                arity(0)?;
                ex(ExprKind::Tuple((0..n).map(|i| call(elem("tup#a", i), "clone", vec![])).collect()))
            }
        };
        let id = self.fresh_local(recv.repr.clone(), recv_ty.clone());
        self.push_scope();
        self.bind("tup#a", id, recv_ty.clone());
        let result = self.expr(&ex(ExprKind::Block(Block { stmts, tail: Some(body), span })), None);
        self.pop_scope();
        let (value, ty) = result?;
        let repr = value.repr.clone();
        Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts: vec![CoreStmt::Let(id, recv)], tail: Some(value) })), repr), ty))
    }

    /// `a == b` on arrays: equal lengths and equal elements.
    fn array_eq(&mut self, left: CoreExpr, ty: Ty, right: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let var = |n: &str| ex(ExprKind::Path(Path::single(n.into(), span)));
        let int = |n: u64| ex(ExprKind::Int(n, crate::lexer::NumSuffix::None));
        let len = |n: &str| ex(ExprKind::MethodCall { recv: var(n), method: "len".into(), args: vec![], span });
        let let_ = |n: &str, m: bool, init: Expr| Stmt::Let { pattern: Pattern { kind: PatternKind::Binding { is_mut: m, name: n.into() }, span }, ty: None, init: Some(init), span };
        let assign = |n: &str, v: Expr| Stmt::Expr(ex(ExprKind::Assign { target: var(n), op: None, value: v }));
        let at = |n: &str| ex(ExprKind::Index { base: var(n), index: var("arr#i") });
        let body = ex(ExprKind::Block(Block {
            stmts: vec![
                let_("arr#b", false, right.clone()),
                let_("arr#same", true, ex(ExprKind::Binary(BinOp::Eq, len("arr#a"), len("arr#b")))),
                let_("arr#i", true, int(0)),
                Stmt::Expr(ex(ExprKind::While {
                    cond: ex(ExprKind::Binary(BinOp::And, var("arr#same"), ex(ExprKind::Binary(BinOp::Lt, var("arr#i"), len("arr#a"))))),
                    body: Block { stmts: vec![
                        assign("arr#same", ex(ExprKind::Binary(BinOp::Eq, at("arr#a"), at("arr#b")))),
                        assign("arr#i", ex(ExprKind::Binary(BinOp::Add, var("arr#i"), int(1)))),
                    ], tail: None, span },
                })),
            ],
            tail: Some(var("arr#same")),
            span,
        }));
        let id = self.fresh_local(left.repr.clone(), ty.clone());
        self.push_scope();
        self.bind("arr#a", id, ty);
        let result = self.expr(&body, None);
        self.pop_scope();
        let (value, vty) = result?;
        let repr = value.repr.clone();
        Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts: vec![CoreStmt::Let(id, left)], tail: Some(value) })), repr), vty))
    }

    /// Call `method` on a trait object: invoke the object's closure for it.
    fn dyn_method_call(&mut self, crecv: CoreExpr, recv_ty: Ty, method: &str, args: &[Expr], span: Span) -> LResult<(CoreExpr, Ty)> {
        let Ty::Named { name, .. } = &recv_ty else { unreachable!() };
        let has_field = matches!(self.ctx.structs.get(name).map(|d| &d.body), Some(StructBody::Named(fs)) if fs.iter().any(|f| f.name == method));
        if !has_field {
            return err(format!("no method `{}` on `{}` (only methods taking `self` without type parameters or `Self` can be called on a trait object)", method, ty_display(&recv_ty)), span);
        }
        let (callee, signature) = self.field_read(crecv, &recv_ty, method, span)?;
        self.invoke_value(callee, signature, args, span)
    }

    #[inline(never)]
    fn expr_inner(&mut self, e: &Expr, expected: Option<&Ty>) -> LResult<(CoreExpr, Ty)> {
        #[cfg(test)]
        EXPR_WORK.with(|n| n.set(n.get()+1));
        match &*e.kind {
            ExprKind::Int(n, sfx) => {
                self.integer_literal(*n, *sfx, expected, false, e.span)
            }
            ExprKind::Float(f, sfx) => {
                let prim = match expected {
                    Some(Ty::Prim(p)) if matches!(p, Prim::F32 | Prim::F64) && *sfx == crate::lexer::NumSuffix::None => *p,
                    _ => suffix_prim(*sfx, true),
                };
                let sr = scalar_of(prim).unwrap();
                if !f.is_finite() || (prim == Prim::F32 && !(*f as f32).is_finite()) {
                    return err(format!("floating literal out of range for `{}`", ty_display(&Ty::Prim(prim))), e.span);
                }
                Ok((CoreExpr::new(CoreExprKind::ConstFloat(*f, sr), Repr::Scalar(sr)), Ty::Prim(prim)))
            }
            ExprKind::Bool(b) => Ok((CoreExpr::new(CoreExprKind::ConstBool(*b), Repr::Scalar(ScalarRepr::Bool)), Ty::bool())),
            ExprKind::Char(c) => Ok((CoreExpr::new(CoreExprKind::ConstChar(*c), Repr::Scalar(ScalarRepr::Char)), Ty::Prim(Prim::Char))),
            ExprKind::Str(s) => {
                let repr = self.repr_of(&Ty::Prim(Prim::Str), e.span)?;
                let Repr::Ref(lid) = repr else { return err("String repr is not a reference", e.span) };
                let sid = self.mono.intern_span(e.span);
                Ok((CoreExpr::new(CoreExprKind::ConstStr(s.clone()), Repr::Ref(lid)).at(sid), Ty::Prim(Prim::Str)))
            }
            ExprKind::Unit => Ok((CoreExpr::new(CoreExprKind::Unit, Repr::Unit), Ty::unit())),

            ExprKind::Path(path) if path.is_single() => {
                let name = path.last();
                if let Some((id, ty)) = self.lookup(name) {
                    // A probe's placeholder-typed `let` has no layout yet; its
                    // stand-in repr is only ever discarded.
                    let repr = if has_placeholder(&ty, &self.infer.placeholders) {
                        self.locals[id as usize].clone()
                    } else {
                        self.repr_of(&ty, e.span)?
                    };
                    let sid = self.mono.intern_span(e.span);
                    Ok((CoreExpr::new(CoreExprKind::Local(id), repr).at(sid), ty))
                } else if let Some(Ty::Named { name: c, .. }) = self.subst.get(name).filter(|t| matches!(t, Ty::Named { name, .. } if name.starts_with("const#"))) {
                    // A const generic parameter used as a value.
                    let n: u64 = c.trim_start_matches("const#").parse().unwrap_or(0);
                    let lit = Expr { kind: std::sync::Arc::new(ExprKind::Int(n, crate::lexer::NumSuffix::None)), span: e.span };
                    self.expr(&lit, expected)
                } else if let Some((cname, def)) = self.const_def(path) {
                    self.const_value(&cname, &def, expected, e.span)
                } else if self.ctx.fns.contains_key(name) {
                    self.function_value(path, expected, e.span)
                } else if self.ctx.variants.contains_key(name) {
                    // An unqualified unit variant brought into scope (`None`).
                    self.variant_ctor(path, &[], expected, e.span)
                } else {
                    // Suggest a similarly-spelled in-scope local or known fn.
                    let in_scope: Vec<String> = self.scope.iter()
                        .flat_map(|s| s.keys().cloned())
                        .chain(self.ctx.fns.keys().map(|k| k.rsplit("::").next().unwrap().to_string()))
                        .collect();
                    let hint = closest(name, in_scope.iter().map(|s| s.as_str()))
                        .map(|s| format!(" (did you mean `{}`?)", s))
                        .unwrap_or_default();
                    err(format!("unknown variable `{}`{}", name, hint), e.span)
                }
            }

            ExprKind::Unary(op, inner) => {
                if *op == UnOp::Neg {
                    if let ExprKind::Int(n, suffix) = inner.kind.as_ref() {
                        return self.integer_literal(*n, *suffix, expected, true, e.span);
                    }
                }
                let (ci, ty) = self.expr(inner, expected)?;
                let valid = match op {
                    UnOp::Neg => is_numeric(&ty),
                    UnOp::Not => matches!(ty, Ty::Prim(p) if is_int_prim(p) || p == Prim::Bool),
                };
                // `-v` / `!v` on other types call `Neg::neg` / `Not::not`.
                if !valid && !matches!(ci.repr, Repr::Scalar(_)) {
                    let (tn, mn, sym) = match op { UnOp::Neg => ("Neg", "neg", "unary `-`"), UnOp::Not => ("Not", "not", "`!`") };
                    return self.trait_method_call(ci, ty, Some(inner), tn, mn, sym, &[], e.span);
                }
                if !valid {
                    return err(format!("invalid unary operator for `{}`", ty_display(&ty)), e.span);
                }
                let repr = ci.repr.clone();
                Ok((CoreExpr::new(CoreExprKind::Un(*op, Box::new(ci)), repr), ty))
            }

            ExprKind::Binary(op, l, r) => self.binary(*op, l, r, e.span),

            // `f::<T>(..)` / `Vec::<T>::new(..)`: pass the type arguments on.
            ExprKind::Call(callee, args) if turbofish_of(callee).is_some() => {
                let (inner, marker, tys) = turbofish_of(callee).unwrap();
                let tys = tys.iter().map(|t| ground_type(t, &self.subst.clone(), self.ctx)).collect::<LResult<Vec<_>>>()?;
                let inner = inner.clone();
                if let ExprKind::Path(path) = &*inner.kind {
                    let key = path.segments.join("::");
                    if self.ctx.variants.contains_key(&key) || (path.is_single() && self.ctx.variants.contains_key(path.last())) {
                        return self.variant_ctor(path, args, expected, e.span);
                    }
                }
                TURBOFISH.with(|t| *t.borrow_mut() = Some((marker == "typeargs#", tys)));
                let result = self.call(&inner, args, expected, e.span);
                TURBOFISH.with(|t| t.borrow_mut().take());
                result
            }
            // `x.m::<T>(..)`: the method's type arguments.
            ExprKind::Cast(inner, target) if turbofish_of(e).is_some() && matches!(&*inner.kind, ExprKind::MethodCall { .. }) => {
                let tys = match &target.kind { TypeKind::Path(_, a) => a.clone(), _ => vec![] };
                let tys = tys.iter().map(|t| ground_type(t, &self.subst.clone(), self.ctx)).collect::<LResult<Vec<_>>>()?;
                let ExprKind::MethodCall { recv, method, args, span } = &*inner.kind else { unreachable!() };
                METHOD_TURBOFISH.with(|t| *t.borrow_mut() = Some(tys));
                let result = self.method_call(recv, method, args, *span);
                METHOD_TURBOFISH.with(|t| t.borrow_mut().take());
                result
            }
            // A path with type arguments not being called (`f::<T>` as a value).
            ExprKind::Cast(inner, _) if turbofish_of(e).is_some() => self.expr(inner, expected),
            ExprKind::Call(callee, args) => {
                // A variant constructor call (`Result::Err(x)`) needs `expected`
                // to infer type params the payload doesn't pin; intercept it
                // here where `expected` is in scope.
                if let ExprKind::Path(path) = &*callee.kind {
                    let key = path.segments.join("::");
                    if !(path.is_single() && self.lookup(path.last()).is_some())
                        && (self.ctx.variants.contains_key(&key) || self.ctx.variants.contains_key(path.last())) {
                        return self.variant_ctor(path, args, expected, e.span);
                    }
                }
                self.call(callee, args, expected, e.span)
            }

            ExprKind::If { cond, then_branch, else_branch } => {
                let (cc, ct) = self.expr(cond, Some(&Ty::bool()))?;
                self.check_assignable(&ct, &Ty::bool(), cond.span)?;
                let (tb, tt) = self.block_expected(then_branch, expected)?;
                // The else branch is checked against the then branch's type (or
                // the outer expectation if the then branch was itself inferred).
                let else_exp = expected.or_else(|| if is_never(&tt) { None } else { Some(&tt) });
                let (eb, et) = match else_branch {
                    Some(e) => {
                        let (ce, t) = self.expr(e, else_exp)?;
                        (block_of(ce), t)
                    }
                    None => (CoreBlock { stmts: vec![], tail: None }, Ty::unit()),
                };
                let result = if is_never(&tt) { et.clone() } else { tt.clone() };
                self.check_assignable(&et, &result, e.span)?;
                let repr = self.repr_of(&result, e.span)?;
                Ok((
                    CoreExpr::new(CoreExprKind::If(Box::new(cc), Box::new(tb), Box::new(eb)), repr),
                    result,
                ))
            }

            ExprKind::Block(b) => {
                let (cb, t) = self.block_expected(b, expected)?;
                let repr = self.repr_of(&t, e.span)?;
                Ok((CoreExpr::new(CoreExprKind::Block(Box::new(cb)), repr), t))
            }

            ExprKind::Unsafe(b) => {
                self.unsafe_depth += 1;
                let result = self.block_expected(b, expected);
                self.unsafe_depth -= 1;
                let (cb, t) = result?;
                let repr = self.repr_of(&t, e.span)?;
                Ok((CoreExpr::new(CoreExprKind::Block(Box::new(cb)), repr), t))
            }

            ExprKind::Return(v) => {
                let cv = match v {
                    Some(e) => {
                        let expected = if self.infer_ret {
                            None
                        } else {
                            Some(self.ret_ty.clone())
                        };
                        let (ce, t) = self.expr(e, expected.as_ref())?;
                        if self.infer_ret && !is_never(&t) {
                            self.ret_ty = t.clone();
                            self.infer_ret = false;
                        }
                        self.check_assignable(&t, &self.ret_ty.clone(), e.span)?;
                        Some(Box::new(ce))
                    }
                    None => {
                        if self.infer_ret {
                            self.ret_ty = Ty::unit();
                            self.infer_ret = false;
                        }
                        self.check_assignable(&Ty::unit(), &self.ret_ty.clone(), e.span)?;
                        None
                    }
                };
                // `return` diverges — type never (assignable to any context).
                // P2 (DWARF): span the return so stepping stops on it.
                let sid = self.mono.intern_span(e.span);
                Ok((CoreExpr::new(CoreExprKind::Return(cv), Repr::Unit).at(sid), never_ty()))
            }

            ExprKind::While { cond, body } => {
                self.loop_exits.push(LoopExit::default());
                let result = (|| {
                    let (cc, ct) = self.expr(cond, Some(&Ty::bool()))?;
                    self.check_assignable(&ct, &Ty::bool(), cond.span)?;
                    let (cb, _) = self.block(body)?;
                    Ok((cc, cb))
                })();
                self.loop_exits.pop();
                let (cc, cb) = result?;
                // while => loop { if !cond { break } body }
                let break_blk = CoreBlock { stmts: vec![], tail: Some(CoreExpr::new(CoreExprKind::Break(None), Repr::Unit)) };
                let guard = CoreExpr::new(
                    CoreExprKind::If(
                        Box::new(negate(cc)),
                        Box::new(break_blk),
                        Box::new(CoreBlock { stmts: vec![], tail: None }),
                    ),
                    Repr::Unit,
                );
                let mut stmts = vec![CoreStmt::Expr(guard)];
                stmts.extend(cb.stmts);
                if let Some(t) = cb.tail {
                    stmts.push(CoreStmt::Expr(t));
                }
                let loop_body = CoreBlock { stmts, tail: None };
                Ok((CoreExpr::new(CoreExprKind::Loop(Box::new(loop_body)), Repr::Unit), Ty::unit()))
            }

            ExprKind::Loop { body } => {
                self.loop_exits.push(LoopExit { takes_value: true, ..LoopExit::default() });
                let result = self.block(body);
                let exit = self.loop_exits.pop().unwrap();
                let (cb, _) = result?;
                // A loop yields its `break value`s; with plain breaks it yields
                // unit. Only a loop without a break has the never type.
                if let Some(ty) = exit.value {
                    let repr = self.repr_of(&ty, e.span)?;
                    return Ok((CoreExpr::new(CoreExprKind::Loop(Box::new(cb)), repr), ty));
                }
                Ok((CoreExpr::new(CoreExprKind::Loop(Box::new(cb)), Repr::Unit), if exit.exits { Ty::unit() } else { never_ty() }))
            }

            // `for c in 'a'..='z'`: loop over the code points.
            ExprKind::For { pat, iter, body } if matches!(&*iter.kind, ExprKind::Range { lo: Some(lo), .. }
                if matches!(self.probe_expr_type(lo, None), Ok(Ty::Prim(Prim::Char)))) =>
            {
                let ExprKind::Range { lo: Some(lo), hi: Some(hi), inclusive } = &*iter.kind else {
                    return err("a `for` range needs both bounds", iter.span);
                };
                let span = e.span;
                let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
                let ty = |n: &str| Type { kind: TypeKind::Path(Path::single(n.into(), span), vec![]), span };
                let range = ex(ExprKind::Range {
                    lo: Some(ex(ExprKind::Cast(lo.clone(), ty("i64")))),
                    hi: Some(ex(ExprKind::Cast(hi.clone(), ty("i64")))),
                    inclusive: *inclusive,
                });
                let code = Pattern { kind: PatternKind::Binding { is_mut: false, name: "for#code".into() }, span };
                let bind = Stmt::Let { pattern: pat.clone(), ty: None, init: Some(ex(ExprKind::Cast(ex(ExprKind::Path(Path::single("for#code".into(), span))), ty("char")))), span };
                let body = Block { stmts: vec![bind, Stmt::Expr(ex(ExprKind::Block(body.clone())))], tail: None, span };
                self.expr(&ex(ExprKind::For { pat: code, iter: range, body }), None)
            }
            ExprKind::For { pat, iter, body } => {
                // Ranges, Vecs and arrays loop by index; anything else that
                // implements `Iterator` is driven by `next` (`lo..` too).
                if !matches!(&*iter.kind, ExprKind::Range { hi: Some(_), .. }) {
                    let ty = self.probe_expr_type(iter, None)?;
                    let indexed = array_elem_ty(&ty).is_some() || matches!(&ty, Ty::Named { name, .. } if name == "Vec");
                    if !indexed && self.ctx.type_implements_trait(&ty, "Iterator") {
                        let desugared = desugar_for_iterator(pat, iter, body, e.span);
                        return self.expr(&desugared, None);
                    }
                    // Anything else iterable: loop over `iter.into_iter()`.
                    if !indexed && self.ctx.type_implements_trait(&ty, "IntoIterator") {
                        let into = Expr { kind: std::sync::Arc::new(ExprKind::MethodCall { recv: iter.clone(), method: "into_iter".into(), args: vec![], span: iter.span }), span: iter.span };
                        let desugared = desugar_for_iterator(pat, &into, body, e.span);
                        return self.expr(&desugared, None);
                    }
                }
                let desugared = desugar_for(pat, iter, body, e.span)?;
                self.expr(&desugared, None)
            }

            // `lo..` as a value is an endless `RangeFrom` iterator.
            ExprKind::Range { lo: Some(lo), hi: None, .. } if self.ctx.structs.contains_key("RangeFrom") => {
                let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span: e.span };
                let lit = ex(ExprKind::StructLit {
                    path: Path::single("RangeFrom".into(), e.span),
                    fields: vec![FieldInit { name: "start".into(), value: Some(lo.clone()), span: e.span }],
                    span: e.span,
                });
                self.expr(&lit, None)
            }
            // `lo..hi` / `lo..=hi` as a value is a `Range` iterator.
            ExprKind::Range { lo: Some(lo), hi: Some(hi), inclusive } if self.ctx.structs.contains_key("Range") => {
                let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span: e.span };
                let end = if *inclusive {
                    ex(ExprKind::Binary(BinOp::Add, hi.clone(), ex(ExprKind::Int(1, crate::lexer::NumSuffix::None))))
                } else { hi.clone() };
                let lit = ex(ExprKind::StructLit {
                    path: Path::single("Range".into(), e.span),
                    fields: vec![
                        FieldInit { name: "start".into(), value: Some(lo.clone()), span: e.span },
                        FieldInit { name: "end".into(), value: Some(end), span: e.span },
                    ],
                    span: e.span,
                });
                self.expr(&lit, None)
            }

            ExprKind::Break(v) => {
                let Some(exit) = self.loop_exits.last_mut() else { return err("break outside loop", e.span); };
                exit.exits = true;
                let Some(v) = v else {
                    if exit.value.is_some() { return err("this `break` needs a value: the loop's other breaks give one", e.span); }
                    return Ok((CoreExpr::new(CoreExprKind::Break(None), Repr::Unit), never_ty()));
                };
                if !exit.takes_value { return err("only `loop` can `break` with a value (not `while` or `for`)", e.span); }
                let want = exit.value.clone();
                let (cv, vt) = self.expr(v, want.as_ref())?;
                match &want {
                    Some(w) => self.check_assignable(&vt, w, v.span)?,
                    None if !is_never(&vt) => { self.loop_exits.last_mut().unwrap().value = Some(vt); }
                    None => {}
                }
                Ok((CoreExpr::new(CoreExprKind::Break(Some(Box::new(cv))), Repr::Unit), never_ty()))
            }

            ExprKind::Continue => {
                if self.loop_exits.is_empty() { return err("continue outside loop", e.span); }
                Ok((CoreExpr::new(CoreExprKind::Continue, Repr::Unit), never_ty()))
            }

            // `(a, b) = (b, a)`, `[x, y] = arr`: assign each part from a
            // temporary holding the value (evaluated once, first).
            ExprKind::Assign { target, op: None, value } if matches!(&*target.kind, ExprKind::Tuple(_) | ExprKind::Array(ArrayLit::Elems(_))) => {
                let span = e.span;
                let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
                // A nested target reuses the name; its `let` reads the outer
                // temporary before shadowing it.
                let tmp = format!("assign#{}", span.start);
                let var = || ex(ExprKind::Path(Path::single(tmp.clone(), span)));
                let mut stmts = vec![Stmt::Let { pattern: Pattern { kind: PatternKind::Binding { is_mut: false, name: tmp.clone() }, span }, ty: None, init: Some(value.clone()), span }];
                let parts: Vec<(Expr, Expr)> = match &*target.kind {
                    ExprKind::Tuple(ts) => ts.iter().enumerate().map(|(i, t)| (t.clone(), ex(ExprKind::Field { base: var(), field: FieldAccess::Tuple(i as u32) }))).collect(),
                    ExprKind::Array(ArrayLit::Elems(ts)) => ts.iter().enumerate().map(|(i, t)| (t.clone(), ex(ExprKind::Index { base: var(), index: ex(ExprKind::Int(i as u64, crate::lexer::NumSuffix::None)) }))).collect(),
                    _ => unreachable!(),
                };
                for (t, v) in parts {
                    if matches!(&*t.kind, ExprKind::Path(p) if p.is_single() && p.last() == "_") { continue; }
                    stmts.push(Stmt::Expr(ex(ExprKind::Assign { target: t, op: None, value: v })));
                }
                self.expr(&ex(ExprKind::Block(Block { stmts, tail: None, span })), None)
            }
            ExprKind::Assign { target, op, value } => {
                if let ExprKind::Index { base, index } = &*target.kind {
                    return match op {
                        None => self.index_set(base, index, value, e.span),
                        Some(op) => self.compound_index_set(base, index, *op, value, e.span),
                    };
                }
                // `s.field = v` → store into the heap object's field slot.
                if let ExprKind::Field { base, field } = &*target.kind {
                    return self.assign_field(base, field, op, value, e.span);
                }
                // Otherwise: assignment to a simple local.
                let ExprKind::Path(p) = &*target.kind else {
                    return err("assignment target must be a variable, field, or index", e.span);
                };
                if !p.is_single() {
                    return err("assignment target must be a simple variable", e.span);
                }
                let Some((id, ty)) = self.lookup(p.last()) else {
                    return err(format!("unknown variable `{}`", p.last()), e.span);
                };
                // Immutable-by-default: reassigning a non-`mut` binding is an error.
                if self.lookup_mut(p.last()) != Some(true) {
                    return err(
                        format!("cannot reassign immutable binding `{}` (declare it `let mut {}`)", p.last(), p.last()),
                        e.span,
                    );
                }
                if let Some(binop) = op {
                    let repr = self.repr_of(&ty, e.span)?;
                    if matches!(repr, Repr::Scalar(_)) { self.check_scalar_binary(*binop, &ty, e.span)?; }
                    if !matches!(repr, Repr::Scalar(_)) {
                        // `x += v` calls `AddAssign::add_assign` when the type
                        // implements it; otherwise it is `x = x + v`.
                        let assign_trait = match binop {
                            BinOp::Add => Some(("AddAssign", "add_assign")), BinOp::Sub => Some(("SubAssign", "sub_assign")),
                            BinOp::Mul => Some(("MulAssign", "mul_assign")), BinOp::Div => Some(("DivAssign", "div_assign")),
                            BinOp::Rem => Some(("RemAssign", "rem_assign")), _ => None,
                        };
                        if let Some((tn, mn)) = assign_trait {
                            if self.lookup_trait_method(&ty, tn, mn).is_some() {
                                let load = CoreExpr::new(CoreExprKind::Local(id), repr);
                                let what = format!("`{}=`", mn);
                                return self.trait_method_call(load, ty.clone(), Some(target), tn, mn, &what, std::slice::from_ref(value), e.span);
                            }
                        }
                        let load = CoreExpr::new(CoreExprKind::Local(id), repr);
                        let (call, ct) = self.overloaded_binary(*binop, load, ty.clone(), target, value, e.span)?;
                        self.check_assignable(&ct, &ty, e.span)?;
                        return Ok((CoreExpr::new(CoreExprKind::Assign { local: id, value: Box::new(call) }, Repr::Unit), Ty::unit()));
                    }
                }
                let (cv, vt) = self.expr(value, Some(&ty))?;
                self.check_assignable(&vt, &ty, e.span)?;
                let final_val = match op {
                    None => cv,
                    Some(binop) => {
                        // x op= v  =>  x = x op v
                        let repr = cv.repr.clone();
                        let load = CoreExpr::new(CoreExprKind::Local(id), repr.clone());
                        CoreExpr::new(CoreExprKind::Bin(*binop, Box::new(load), Box::new(cv)), repr)
                    }
                };
                Ok((CoreExpr::new(CoreExprKind::Assign { local: id, value: Box::new(final_val) }, Repr::Unit), Ty::unit()))
            }

            // `it.collect::<T>()`.
            ExprKind::Cast(inner, target) if matches!(&*inner.kind, ExprKind::MethodCall { method, args, .. } if method == "collect" && args.is_empty()) => {
                let ExprKind::MethodCall { recv, span, .. } = &*inner.kind else { unreachable!() };
                let want = ground_type(target, &self.subst.clone(), self.ctx)?;
                self.collect_call(recv, Some(&want), *span)
            }
            ExprKind::Cast(inner, target) => {
                let (ci, from_ty) = self.expr(inner, None)?;
                let to_ty = lower_type(target, &[], self.ctx).map_err(conv)?;
                // `enum as i64`: the variant's discriminant (explicit, or one
                // more than the previous variant's, starting at 0).
                if let (Ty::Named { name, .. }, Ty::Prim(p)) = (&from_ty, &to_ty) {
                    if let (Some(def), true) = (self.ctx.enums.get(name).cloned(), is_int_prim(*p)) {
                        let span = e.span;
                        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
                        let mut next = 0i64;
                        let arms = def.variants.iter().map(|v| {
                            let d = v.discriminant.unwrap_or(next);
                            next = d.wrapping_add(1);
                            let mut path: Vec<String> = name.split("::").map(str::to_string).collect();
                            path.push(v.name.clone());
                            let payload = match &v.payload {
                                VariantPayload::None => vec![],
                                VariantPayload::Tuple(ts) => ts.iter().map(|_| Pattern { kind: PatternKind::Wildcard, span }).collect(),
                                VariantPayload::Named(_) => vec![],
                            };
                            let pattern = match &v.payload {
                                VariantPayload::Named(_) => Pattern { kind: PatternKind::Struct { path: Path { segments: path, span }, fields: vec![], rest: true }, span },
                                _ => Pattern { kind: PatternKind::Variant { path: Path { segments: path, span }, payload }, span },
                            };
                            let lit = ex(ExprKind::Int(d.unsigned_abs(), crate::lexer::NumSuffix::None));
                            let body = if d < 0 { ex(ExprKind::Unary(UnOp::Neg, lit)) } else { lit };
                            MatchArm { pattern, guard: None, body, span }
                        }).collect();
                        let id = self.fresh_local(ci.repr.clone(), from_ty.clone());
                        self.push_scope();
                        self.bind("cast#enum", id, from_ty.clone());
                        let m = ex(ExprKind::Match { scrutinee: ex(ExprKind::Path(Path::single("cast#enum".into(), span))), arms });
                        let result = self.expr(&m, Some(&to_ty));
                        self.pop_scope();
                        let (value, ty) = result?;
                        let repr = value.repr.clone();
                        return Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts: vec![CoreStmt::Let(id, ci)], tail: Some(value) })), repr), ty));
                    }
                }
                let from = ci.repr.clone();
                let to = self.repr_of(&to_ty, e.span)?;
                // A representation conversion is not a general type coercion.
                // In particular, never manufacture an untraced native pointer
                // from an integer or reinterpret a managed object as a scalar.
                let valid = match (&from, &to) {
                    (Repr::Scalar(ScalarRepr::Ptr), _) | (_, Repr::Scalar(ScalarRepr::Ptr)) => false,
                    (Repr::Scalar(source), Repr::Scalar(ScalarRepr::Bool)) => *source == ScalarRepr::Bool,
                    (Repr::Scalar(source), Repr::Scalar(ScalarRepr::Char)) => source.is_int(),
                    (Repr::Scalar(ScalarRepr::Bool | ScalarRepr::Char), Repr::Scalar(target)) => {
                        !target.is_float()
                    }
                    (Repr::Scalar(_), Repr::Scalar(_)) => true,
                    _ => false,
                };
                if !valid {
                    return err(
                        format!(
                            "invalid cast from `{}` to `{}`",
                            ty_display(&from_ty),
                            ty_display(&to_ty)
                        ),
                        e.span,
                    );
                }
                Ok((
                    CoreExpr::new(CoreExprKind::Cast { value: Box::new(ci), from, to: to.clone() }, to),
                    to_ty,
                ))
            }

            ExprKind::StructLit { path, fields, .. } => {
                let key = path.segments.join("::");
                let is_struct = self.ctx.canon(&key).is_some_and(|c| self.ctx.structs.contains_key(&c));
                if !is_struct && self.ctx.variant_by_path(path).is_some() {
                    self.variant_struct_lit(path, fields, expected, e.span)
                } else {
                    self.struct_lit(path, fields, expected, e.span)
                }
            }

            ExprKind::Field { base, field } => self.field_access(base, field, e.span),

            ExprKind::Index { base, index } => self.index_get(base, index, e.span),

            ExprKind::Tuple(elems) => {
                let wanted = match expected {
                    Some(Ty::Tuple(types)) => {
                        if types.len() != elems.len() {
                            return err(format!("expected a tuple with {} element(s), found {}", types.len(), elems.len()), e.span);
                        }
                        Some(types.as_slice())
                    }
                    _ => None,
                };
                let mut inferred = HashMap::new();
                if let Some(types) = wanted.filter(|types| types.iter().any(has_var)) {
                    self.infer_argument_types(elems.iter().zip(types.iter()), &mut inferred);
                }
                let mut cfields = Vec::with_capacity(elems.len());
                let mut tys = Vec::with_capacity(elems.len());
                for (index, el) in elems.iter().enumerate() {
                    let declared = wanted.map(|types| &types[index]);
                    let expected = declared.map(|ty| apply_subst(ty, &inferred));
                    let (ce, ty) = self.expr(el, expected.as_ref().and_then(compound_hint))?;
                    if let Some(declared) = declared {
                        unify_infer(declared, &ty, &mut inferred);
                        self.check_assignable(&ty, &apply_subst(declared, &inferred), el.span)?;
                    }
                    cfields.push(ce);
                    tys.push(ty);
                }
                let ty = Ty::Tuple(tys);
                let repr = self.repr_of(&ty, e.span)?;
                let Repr::Value(vid) = repr else { return err("tuple must be a value type", e.span) };
                Ok((CoreExpr::new(CoreExprKind::MakeValue { value: vid, fields: cfields }, Repr::Value(vid)), ty))
            }

            ExprKind::Path(path) if !path.is_single() && self.const_def(path).is_some() => {
                let (cname, def) = self.const_def(path).unwrap();
                self.const_value(&cname, &def, expected, e.span)
            }
            // `i64::MAX`, `f64::INFINITY`, `f64::consts::PI`, ...
            ExprKind::Path(path) if primitive_const(&path.segments).is_some() => {
                let (suffix, value) = primitive_const(&path.segments).unwrap();
                let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span: e.span };
                let lit = match value {
                    PrimConst::Int(n) if n < 0 => ex(ExprKind::Binary(
                        BinOp::Sub,
                        ex(ExprKind::Unary(UnOp::Neg, ex(ExprKind::Int((n + 1).unsigned_abs() as u64, suffix)))),
                        ex(ExprKind::Int(1, suffix)),
                    )),
                    PrimConst::Int(n) => ex(ExprKind::Int(n as u64, suffix)),
                    // Non-finite values have no literal: `1.0 / 0.0` and so on.
                    PrimConst::Float(f) if !f.is_finite() => ex(ExprKind::Binary(
                        BinOp::Div,
                        ex(ExprKind::Float(if f.is_nan() { 0.0 } else { f.signum() }, suffix)),
                        ex(ExprKind::Float(0.0, suffix)),
                    )),
                    PrimConst::Float(f) => ex(ExprKind::Float(f, suffix)),
                };
                self.expr(&lit, expected)
            }
            ExprKind::Path(path) if !path.is_single() => {
                // A qualified path that's a unit enum variant: `Color::Red`,
                // `Option::None`.
                if self.ctx.fns.contains_key(&path.segments.join("::")) {
                    self.function_value(path, expected, e.span)
                } else { self.variant_ctor(path, &[], expected, e.span) }
            }

            ExprKind::Match { scrutinee, arms } => self.match_expr(scrutinee, arms, expected, e.span),

            ExprKind::Try(inner) => self.try_op(inner, e.span),

            // `x.into()`: `From::from(x)` into the expected type.
            ExprKind::MethodCall { recv, method, args, span } if method == "into" && args.is_empty()
                && expected.is_some_and(|t| !has_var(t)) =>
            {
                let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span: *span };
                let from = ex(ExprKind::Path(Path { segments: vec!["From".into(), "from".into()], span: *span }));
                self.expr(&ex(ExprKind::Call(from, vec![recv.clone()])), expected)
            }
            ExprKind::MethodCall { recv, method, args, span } if method == "collect" && args.is_empty() => {
                self.collect_call(recv, expected, *span)
            }
            ExprKind::MethodCall { recv, method, args, span } => {
                self.method_call(recv, method, args, *span)
            }

            ExprKind::Closure { params, ret, body } => {
                self.closure(params, ret, body, expected, e.span)
            }

            ExprKind::Array(lit) => self.array_literal(lit, expected, e.span),

            other => err(format!("expression not supported in v0 slice: {:?}", disc(other)), e.span),
        }
    }

    /// `[a, b, c]` and `[x; n]`. The value is a fixed-length array `[T; N]`,
    /// or an `Array<T>` where one is expected (then `n` may be computed at
    /// run time). Both share the runtime array object. In `[x; n]`, `x` is
    /// evaluated once and every slot holds that same value.
    fn array_literal(&mut self, lit: &ArrayLit, expected: Option<&Ty>, span: Span) -> LResult<(CoreExpr, Ty)> {
        let (want_elem, dynamic) = match expected {
            Some(Ty::Array(e, _)) => (Some((**e).clone()), false),
            Some(t) => match array_elem_ty(t) { Some(e) => (Some(e), true), None => (None, false) },
            None => (None, false),
        };
        let i64_repr = Repr::Scalar(ScalarRepr::I64);
        let int = |n: u64| CoreExpr::new(CoreExprKind::ConstInt(n, ScalarRepr::I64), Repr::Scalar(ScalarRepr::I64));
        // Preserve the container context while inferring its element variables.
        let mut inferred = HashMap::new();
        if let (Some(pattern), ArrayLit::Elems(elements)) = (want_elem.as_ref().filter(|ty| has_var(ty)), lit) {
            self.infer_argument_types(elements.iter().map(|value| (value, pattern)), &mut inferred);
        }
        let mut infer_element = |this: &mut Self, value: &Expr, element: &mut Option<Ty>| -> LResult<(CoreExpr, Ty)> {
            let expected = element.as_ref().map(|ty| apply_subst(ty, &inferred));
            let (core, actual) = this.expr(value, expected.as_ref().and_then(compound_hint))?;
            if let Some(pattern) = element.as_ref() {
                unify_infer(pattern, &actual, &mut inferred);
                this.check_assignable(&actual, &apply_subst(pattern, &inferred), value.span)?;
            } else { *element = Some(actual.clone()); }
            Ok((core, actual))
        };
        let (elem_ty, values, count): (Ty, Vec<CoreExpr>, CoreExpr) = match lit {
            ArrayLit::Elems(elements) => {
                let mut element = want_elem.clone();
                let mut values = Vec::new();
                for value in elements { values.push(infer_element(self, value, &mut element)?.0); }
                let element = element.map(|ty| apply_subst(&ty, &inferred)).filter(|ty| !has_var(ty))
                    .ok_or_else(|| LowerError { msg: if elements.is_empty() {
                        "cannot infer the element type of an empty array literal; annotate its type"
                    } else {
                        "cannot infer the element type of an unconstrained array literal; annotate its type"
                    }.into(), span })?;
                (element, values, int(elements.len() as u64))
            }
            ArrayLit::Repeat(value, count) => {
                let mut element = want_elem.clone();
                let (core, actual) = infer_element(self, value, &mut element)?;
                let (count_core, count_ty) = self.expr(count, Some(&Ty::i64()))?;
                self.check_assignable(&count_ty, &Ty::i64(), count.span)?;
                (actual, vec![core], count_core)
            }
        };
        let ty = if dynamic {
            Ty::Named { name: "Array".into(), args: vec![elem_ty.clone()] }
        } else {
            let n = match lit {
                ArrayLit::Elems(es) => es.len() as u64,
                ArrayLit::Repeat(_, count) => match &*count.kind {
                    ExprKind::Int(n, _) => *n,
                    // A const generic parameter, or a `const` with a literal value.
                    ExprKind::Path(p) if self.const_len(p).is_some() => self.const_len(p).unwrap(),
                    _ => return err("the length in `[x; n]` must be an integer literal (or the literal must initialize an `Array<T>`)", count.span),
                },
            };
            if let Some(Ty::Array(_, want)) = expected {
                if *want != n {
                    return err(format!("expected an array of length {}, this literal has length {}", want, n), span);
                }
            }
            Ty::Array(Box::new(elem_ty.clone()), n)
        };
        let Repr::Ref(layout) = self.repr_of(&ty, span)? else {
            return err("array literal without an array representation", span);
        };
        let elem = self.repr_of(&elem_ty, span)?;
        let arr = self.fresh_local(Repr::Ref(layout), ty.clone());
        let local = |id: LocalId, repr: Repr| CoreExpr::new(CoreExprKind::Local(id), repr);
        let set = |index: CoreExpr, value: CoreExpr| CoreStmt::Expr(CoreExpr::new(
            CoreExprKind::ArraySet { array: Box::new(local(arr, Repr::Ref(layout))), index: Box::new(index), value: Box::new(value), elem: elem.clone() },
            Repr::Unit,
        ));
        let mut stmts = Vec::new();
        match lit {
            ArrayLit::Elems(_) => {
                stmts.push(CoreStmt::Let(arr, CoreExpr::new(CoreExprKind::ArrayNew { layout, len: Box::new(count), elem: elem.clone() }, Repr::Ref(layout))));
                for (i, v) in values.into_iter().enumerate() {
                    stmts.push(set(int(i as u64), v));
                }
            }
            ArrayLit::Repeat(..) => {
                // let n = count; let arr = new(n); let v = value; let mut i = 0;
                // loop { if i >= n { break } arr[i] = v; i = i + 1 }
                let n = self.fresh_local(i64_repr.clone(), Ty::i64());
                let v = self.fresh_local(elem.clone(), elem_ty.clone());
                let i = self.fresh_local(i64_repr.clone(), Ty::i64());
                stmts.push(CoreStmt::Let(n, count));
                stmts.push(CoreStmt::Let(arr, CoreExpr::new(CoreExprKind::ArrayNew { layout, len: Box::new(local(n, i64_repr.clone())), elem: elem.clone() }, Repr::Ref(layout))));
                stmts.push(CoreStmt::Let(v, values.into_iter().next().unwrap()));
                stmts.push(CoreStmt::Let(i, int(0)));
                let done = CoreExpr::new(CoreExprKind::Bin(BinOp::Ge, Box::new(local(i, i64_repr.clone())), Box::new(local(n, i64_repr.clone()))), Repr::Scalar(ScalarRepr::Bool));
                let exit = CoreExpr::new(CoreExprKind::If(
                    Box::new(done),
                    Box::new(CoreBlock { stmts: vec![], tail: Some(CoreExpr::new(CoreExprKind::Break(None), Repr::Unit)) }),
                    Box::new(CoreBlock { stmts: vec![], tail: None }),
                ), Repr::Unit);
                let step = CoreExpr::new(CoreExprKind::Assign {
                    local: i,
                    value: Box::new(CoreExpr::new(CoreExprKind::Bin(BinOp::Add, Box::new(local(i, i64_repr.clone())), Box::new(int(1))), i64_repr.clone())),
                }, Repr::Unit);
                let body = CoreBlock { stmts: vec![CoreStmt::Expr(exit), set(local(i, i64_repr.clone()), local(v, elem.clone())), CoreStmt::Expr(step)], tail: None };
                stmts.push(CoreStmt::Expr(CoreExpr::new(CoreExprKind::Loop(Box::new(body)), Repr::Unit)));
            }
        }
        let sid = self.mono.intern_span(span);
        Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts, tail: Some(local(arr, Repr::Ref(layout))) })), Repr::Ref(layout)).at(sid), ty))
    }

    /// A `Name { field: val, ... }` literal — struct OR an enum struct-variant.
    /// `Enum::Variant { a: x, b }`: a variant with named fields, built like
    /// the positional form with independent written-order evaluation.
    fn variant_struct_lit(&mut self, path: &Path, fields: &[FieldInit], expected: Option<&Ty>, span: Span) -> LResult<(CoreExpr, Ty)> {
        let key = path.segments.join("::");
        let (enum_name, tag) = self.ctx.variant_by_path(path)
            .ok_or_else(|| LowerError { msg: format!("unknown variant `{}`", key), span })?;
        let names = self.variant_field_names(&enum_name, tag);
        if names.is_empty() {
            return err(format!("variant `{}` has no named fields; write `{}(..)`", path.last(), path.last()), span);
        }
        for f in fields {
            if !names.contains(&f.name) {
                return err(format!("variant `{}` has no field `{}`", path.last(), f.name), f.span);
            }
            if fields.iter().filter(|g| g.name == f.name).count() > 1 {
                return err(format!("field `{}` is given twice", f.name), f.span);
            }
        }
        let mut args = Vec::new();
        for n in &names {
            let f = fields.iter().find(|f| &f.name == n)
                .ok_or_else(|| LowerError { msg: format!("missing field `{}` for variant `{}`", n, path.last()), span })?;
            args.push(f.value.clone().unwrap_or_else(|| Expr { kind: std::sync::Arc::new(ExprKind::Path(Path::single(n.clone(), f.span))), span: f.span }));
        }
        let order: Vec<_> = fields.iter().map(|f| names.iter().position(|n| n == &f.name).unwrap()).collect();
        self.variant_ctor_ordered(path, &args, expected, span, Some(&order))
    }

    fn struct_lit(&mut self, path: &Path, fields: &[FieldInit], expected: Option<&Ty>, span: Span) -> LResult<(CoreExpr, Ty)> {
        let name = path.last();
        let qualified = path.segments.join("::");
        let canon = self.ctx.canon(&qualified).unwrap_or(qualified);
        // Constructor binders are independent of the enclosing function's
        // binders. Normalize an alias template before inferring its arguments.
        let alias_name = self.ctx.aliases.contains_key(&canon).then(|| canon.clone());
        let alias_template = self.ctx.aliases.get(&canon).map(|alias| {
            let renamed = alias.params.iter().enumerate().map(|(i, name)|
                (name.clone(), Ty::Var(format!("$constructor{i}")))).collect();
            apply_subst(&alias.target, &renamed)
        });
        let canon = match &alias_template {
            Some(Ty::Named { name, .. }) => name.clone(),
            Some(_) => return err(format!("`{name}` is not a struct"), span),
            None => canon,
        };
        let Some(s) = self.ctx.structs.get(&canon).cloned() else {
            return err(format!("`{}` is not a struct", name), span);
        };
        // Tuple and unit structs are records over their positional fields.
        let def_fields: Vec<FieldDef> = match &s.body {
            StructBody::Named(fs) => fs.clone(),
            StructBody::Tuple(tys) => tys.iter().enumerate().map(|(i, t)| FieldDef { vis: true, name: i.to_string(), ty: t.clone(), span: t.span }).collect(),
            StructBody::Unit => vec![],
        };
        let def_fields = &def_fields;
        // `S { a: 1, ..base }`: every field not written is read from `base`
        // (evaluated once, before the written fields).
        if let Some(pos) = fields.iter().position(|f| f.name == "..") {
            let base = fields[pos].value.clone().unwrap();
            let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
            let var = || ex(ExprKind::Path(Path::single("update#base".into(), span)));
            let mut inits: Vec<FieldInit> = fields.iter().filter(|f| f.name != "..").cloned().collect();
            for df in def_fields {
                if !inits.iter().any(|f| f.name == df.name) {
                    inits.push(FieldInit { name: df.name.clone(), value: Some(ex(ExprKind::Field { base: var(), field: FieldAccess::Named(df.name.clone()) })), span });
                }
            }
            // The base has the struct's type (`..Default::default()` needs it).
            let base_expected = expected.cloned().or_else(|| s.generics.params.is_empty().then(|| Ty::Named { name: canon.clone(), args: vec![] }));
            let (cbase, bty) = self.expr(&base, base_expected.as_ref())?;
            let id = self.fresh_local(cbase.repr.clone(), bty.clone());
            self.push_scope();
            self.bind("update#base", id, bty.clone());
            let result = self.expr(&ex(ExprKind::StructLit { path: path.clone(), fields: inits, span }), expected.or(Some(&bty)));
            self.pop_scope();
            let (value, ty) = result?;
            let repr = value.repr.clone();
            return Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts: vec![CoreStmt::Let(id, cbase)], tail: Some(value) })), repr), ty));
        }
        let gparams: Vec<String> = s.generics.params.iter().map(|p| p.name.clone()).collect();
        let template = alias_template.unwrap_or_else(|| Ty::Named {
            name: canon.clone(),
            args: gparams.iter().enumerate().map(|(i, _)| Ty::Var(format!("$constructor{i}"))).collect(),
        });
        let Ty::Named { args: template_args, .. } = &template else { unreachable!() };
        let field_subst: HashMap<String, Ty> = gparams.iter().cloned().zip(template_args.iter().cloned()).collect();
        // Validate the field set before evaluating any initializer. Evaluate in
        // written order into rooted locals; the object layout uses declaration
        // order independently of that observable evaluation order.
        let mut seen = std::collections::HashSet::new();
        for init in fields {
            if !def_fields.iter().any(|df| df.name == init.name) {
                return err(format!("unknown field `{}`", init.name), span);
            }
            if !seen.insert(init.name.as_str()) {
                return err(format!("duplicate field `{}`", init.name), span);
            }
        }
        for df in def_fields {
            if !seen.contains(df.name.as_str()) {
                return err(format!("missing field `{}`", df.name), span);
            }
        }
        let mut targ: HashMap<String, Ty> = HashMap::new();
        if let Some(expected) = expected {
            unify_infer(&template, expected, &mut targ);
        }
        // Discover constraints without retaining generated code or changing
        // source-order evaluation. Failed probes are checked in the real pass.
        while has_var(&apply_subst(&template, &targ)) {
            let previous = targ.clone();
            for init in fields {
                let field = def_fields.iter().find(|field| field.name == init.name).unwrap();
                let declared = lower_type(&field.ty, &gparams, self.ctx).map_err(conv)?;
                let declared = apply_subst(&declared, &field_subst);
                let expected = apply_subst(&declared, &targ);
                if !has_var(&expected) { continue; }
                let shorthand;
                let value = if let Some(value) = &init.value { value } else {
                    shorthand = Expr { kind: std::sync::Arc::new(ExprKind::Path(Path::single(field.name.clone(), span))), span };
                    &shorthand
                };
                if let Ok(actual) = self.probe_expr_type(value, compound_hint(&expected)) {
                    if !has_var(&actual) && !is_never(&actual) {
                        unify_infer(&declared, &actual, &mut targ);
                    }
                }
            }
            if previous == targ { break; }
        }
        let mut cfields = vec![None; def_fields.len()];
        let mut field_stmts = Vec::with_capacity(fields.len());
        let mut field_checks = Vec::new();
        for init in fields {
            let index = def_fields.iter().position(|df| df.name == init.name).unwrap();
            let df = &def_fields[index];
            let declared = lower_type(&df.ty, &gparams, self.ctx).map_err(conv)?;
            let declared = apply_subst(&declared, &field_subst);
            let val_expr = match &init.value {
                Some(v) => v.clone(),
                None => Expr { kind: std::sync::Arc::new(ExprKind::Path(Path::single(df.name.clone(), span))), span },
            };
            let field_hint = apply_subst(&declared, &targ);
            let (cv, vt) = self.expr(&val_expr, compound_hint(&field_hint))?;
            unify_infer(&declared, &vt, &mut targ);
            let local = self.fresh_local(cv.repr.clone(), vt.clone());
            cfields[index] = Some(CoreExpr::new(CoreExprKind::Local(local), cv.repr.clone()));
            field_stmts.push(CoreStmt::Let(local, cv));
            field_checks.push((declared, vt));
        }
        let ty = apply_subst(&template, &targ);
        if has_var(&ty) {
            self.infer.unresolved = Some(ty);
            return err(format!("cannot infer type arguments for struct `{name}`"), span);
        }
        if let Some(expected) = expected {
            self.check_assignable(&ty, expected, span)?;
        }
        if let Some(alias) = &alias_name { self.check_alias_bounds(alias, &ty, span)?; }
        for (declared, actual) in &field_checks {
            self.check_assignable(actual, &apply_subst(declared, &targ), span)?;
            if matches!(actual, Ty::Prim(Prim::RawPtr)) {
                self.require_unsafe("constructing a record with a RawPtr field", span)?;
            }
        }
        let repr = self.repr_of(&ty, span)?;
        let cfields = cfields.into_iter().map(Option::unwrap).collect();
        let kind = match repr {
            Repr::Ref(lid) => CoreExprKind::New { layout: lid, fields: cfields },
            Repr::Value(vid) => CoreExprKind::MakeValue { value: vid, fields: cfields },
            _ => return err("struct must be a ref or value type", span),
        };
        let sid = self.mono.intern_span(span);
        let construction = CoreExpr::new(kind, repr.clone()).at(sid);
        let block = CoreBlock { stmts: field_stmts, tail: Some(construction) };
        Ok((CoreExpr::new(CoreExprKind::Block(Box::new(block)), repr).at(sid), ty))
    }

    /// Construct an enum variant. `path` is `Enum::Variant`; `args` are payload
    /// values (empty for a unit variant). `expected` provides the enum's type
    /// args when they can't be inferred from the payload (e.g. `None`).
    fn variant_ctor(
        &mut self,
        path: &Path,
        args: &[Expr],
        expected: Option<&Ty>,
        span: Span,
    ) -> LResult<(CoreExpr, Ty)> {
        self.variant_ctor_ordered(path, args, expected, span, None)
    }

    fn variant_ctor_ordered(
        &mut self, path: &Path, args: &[Expr], expected: Option<&Ty>, span: Span,
        written_order: Option<&[usize]>,
    ) -> LResult<(CoreExpr, Ty)> {
        let key = path.segments.join("::");
        let last = path.last();
        let (enum_name, tag) = self.ctx.variant_by_path(path)
            .ok_or_else(|| LowerError { msg: format!("unknown variant `{}`", key), span })?;
        // Builtin Option/Result: enum_name is "Option"/"Result"; handle by name.
        let gparams: Vec<String> = self.ctx.enums.get(&enum_name)
            .map(|e| e.generics.params.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_else(|| builtin_enum_params(&enum_name));

        let alias_name = if path.segments.len() > 1 {
            let prefix = path.segments[..path.segments.len() - 1].join("::");
            self.ctx.canon(&prefix).filter(|name| self.ctx.aliases.contains_key(name))
        } else { None };
        let template = if let Some(alias) = alias_name.as_ref().and_then(|name| self.ctx.aliases.get(name)) {
            let renamed = alias.params.iter().enumerate().map(|(i, name)|
                (name.clone(), Ty::Var(format!("$constructor{i}")))).collect();
            apply_subst(&alias.target, &renamed)
        } else {
            Ty::Named { name: enum_name.clone(), args: gparams.iter().enumerate()
                .map(|(i, _)| Ty::Var(format!("$constructor{i}"))).collect() }
        };
        let Ty::Named { args: template_args, .. } = &template else { unreachable!() };
        let field_subst: HashMap<String, Ty> = gparams.iter().cloned().zip(template_args.iter().cloned()).collect();
        // Declared payload types (may mention the enum's generic params).
        let decl_payload: Vec<Ty> = self.variant_decl_payload(&enum_name, tag, &gparams)?.iter()
            .map(|ty| apply_subst(ty, &field_subst)).collect();
        if args.len() != decl_payload.len() {
            return err(format!("variant `{}` expects {} args, got {}", last, decl_payload.len(), args.len()), span);
        }

        // Infer the enum's type args from (a) the expected type, (b) the payload.
        let mut targ: HashMap<String, Ty> = HashMap::new();
        if let Some(expected) = expected {
            unify_infer(&template, expected, &mut targ);
        }
        if has_var(&apply_subst(&template, &targ)) {
            self.infer_argument_types(args.iter().zip(&decl_payload), &mut targ);
        }
        let mut cargs = vec![None; args.len()];
        let mut actuals = Vec::new();
        let mut field_stmts = Vec::new();
        let positional_order: Vec<_> = (0..args.len()).collect();
        for &index in written_order.unwrap_or(&positional_order) {
            let a = &args[index];
            let decl = &decl_payload[index];
            let payload_hint = apply_subst(decl, &targ);
            let (ca, at) = self.expr(a, compound_hint(&payload_hint))?;
            unify_infer(decl, &at, &mut targ);
            if written_order.is_some() {
                let local = self.fresh_local(ca.repr.clone(), at.clone());
                cargs[index] = Some(CoreExpr::new(CoreExprKind::Local(local), ca.repr.clone()));
                field_stmts.push(CoreStmt::Let(local, ca));
            } else {
                cargs[index] = Some(ca);
            }
            actuals.push((decl.clone(), at));
        }
        let cargs = cargs.into_iter().map(Option::unwrap).collect();
        let ty = apply_subst(&template, &targ);
        if has_var(&ty) {
            self.infer.unresolved = Some(ty);
            return err(format!("cannot infer type arguments for enum `{enum_name}`"), span);
        }
        if let Some(expected) = expected {
            self.check_assignable(&ty, expected, span)?;
        }
        if let Some(alias) = &alias_name { self.check_alias_bounds(alias, &ty, span)?; }
        for (decl, actual) in &actuals {
            self.check_assignable(actual, &apply_subst(decl, &targ), span)?;
        }
        let repr = self.repr_of(&ty, span)?;
        let (construction, ty) = match repr {
            // Only the Ref (heap-allocating) variant is an alloc site → intern a
            // span only here, not for the inline MakeValueVariant.
            Repr::Ref(lid) => {
                let sid = self.mono.intern_span(span);
                Ok((CoreExpr::new(CoreExprKind::MakeVariant { layout: lid, tag, fields: cargs }, repr).at(sid), ty))
            }
            Repr::Value(vid) => {
                Ok((CoreExpr::new(CoreExprKind::MakeValueVariant { value: vid, tag, fields: cargs }, repr), ty))
            }
            _ => err("enum constructor produced a non-aggregate repr", span),
        }?;
        if written_order.is_some() {
            let repr = construction.repr.clone();
            let block = CoreBlock { stmts: field_stmts, tail: Some(construction) };
            Ok((CoreExpr::new(CoreExprKind::Block(Box::new(block)), repr), ty))
        } else {
            Ok((construction, ty))
        }
    }

    /// The declared payload types of `enum::variant[tag]`, with the enum's
    /// generic params left as `Ty::Var`. Handles builtin Option/Result.
    fn variant_decl_payload(&self, enum_name: &str, tag: u32, gparams: &[String]) -> LResult<Vec<Ty>> {
        if let Some(e) = self.ctx.enums.get(enum_name) {
            let variant = &e.variants[tag as usize];
            return match &variant.payload {
                VariantPayload::None => Ok(vec![]),
                VariantPayload::Tuple(tys) => tys.iter().map(|t| lower_type(t, gparams, self.ctx)).collect::<Result<_, _>>().map_err(conv),
                VariantPayload::Named(fs) => fs.iter().map(|f| lower_type(&f.ty, gparams, self.ctx)).collect::<Result<_, _>>().map_err(conv),
            };
        }
        // Builtin Option/Result.
        Ok(match (enum_name, tag) {
            ("Option", 0) => vec![],                       // None
            ("Option", 1) => vec![Ty::Var("T".into())],    // Some(T)
            ("Result", 0) => vec![Ty::Var("T".into())],    // Ok(T)
            ("Result", 1) => vec![Ty::Var("E".into())],    // Err(E)
            _ => return Err(LowerError { msg: format!("unknown variant of `{}`", enum_name), span: Span::new(0, 0) }),
        })
    }

    fn field_access(&mut self, base: &Expr, field: &FieldAccess, span: Span) -> LResult<(CoreExpr, Ty)> {
        let (cbase, bty) = self.expr(base, None)?;
        let fname = match field {
            FieldAccess::Named(n) => n,
            // `p.0` on a tuple struct reads its positional field `0`.
            FieldAccess::Tuple(i) if matches!(&bty, Ty::Named { .. }) => {
                let name = i.to_string();
                return self.field_read(cbase, &bty, &name, span);
            }
            FieldAccess::Tuple(i) => {
                // `t.0` — tuple element access (value aggregate).
                let Ty::Tuple(elems) = &bty else {
                    return err("tuple index on a non-tuple", span);
                };
                let elem_ty = elems.get(*i as usize).cloned()
                    .ok_or_else(|| LowerError { msg: format!("tuple has no element {}", i), span })?;
                let frepr = self.repr_of(&elem_ty, span)?;
                return Ok((
                    CoreExpr::new(CoreExprKind::Field { base: Box::new(cbase), loc: FieldLoc::ValueField { index: *i } }, frepr),
                    elem_ty,
                ));
            }
        };
        self.field_read(cbase, &bty, fname, span)
    }

    /// Read field `fname` of the lowered struct value `cbase` of type `bty`.
    fn field_read(&mut self, cbase: CoreExpr, bty: &Ty, fname: &str, span: Span) -> LResult<(CoreExpr, Ty)> {
        let Ty::Named { name, args: struct_args } = bty else {
            return err("field access on a non-struct", span);
        };
        let s = self.ctx.structs.get(name).cloned()
            .ok_or_else(|| LowerError { msg: format!("`{}` is not a struct", name), span })?;
        let StructBody::Named(def_fields) = &s.body else {
            return err("field access on a non-record struct", span);
        };
        let idx = def_fields.iter().position(|f| f.name == fname)
            .ok_or_else(|| LowerError { msg: format!("no field `{}` on `{}`", fname, name), span })?;
        let gparams: Vec<String> = s.generics.params.iter().map(|p| p.name.clone()).collect();
        let subst: HashMap<String, Ty> = gparams.iter().cloned().zip(struct_args.iter().cloned()).collect();
        let raw_fty = lower_type(&def_fields[idx].ty, &gparams, self.ctx).map_err(conv)?;
        let fty = apply_subst(&raw_fty, &subst);
        if matches!(fty, Ty::Prim(Prim::RawPtr)) {
            self.require_unsafe("reading a RawPtr record field", span)?;
        }
        // The FieldLoc is in the base layout's field_map for reference structs;
        // value structs store fields in declaration order in the LLVM aggregate.
        let loc = match &cbase.repr {
            Repr::Ref(lid) => self.reg.layouts[*lid as usize].field_map[idx],
            Repr::Value(_vid) => FieldLoc::ValueField { index: idx as u32 },
            _ => return err("field access on non-aggregate", span),
        };
        let frepr = self.repr_of(&fty, span)?;
        Ok((CoreExpr::new(CoreExprKind::Field { base: Box::new(cbase), loc }, frepr), fty))
    }

    /// Lower `base.field = value` to a `SetField` (reference structs only).
    fn assign_field(&mut self, base: &Expr, field: &FieldAccess, op: &Option<BinOp>, value: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        // Transitive immutability: `base.field = v` requires the root of `base`'s
        // access path to be a `mut` binding (see docs/mutability.md).
        self.check_mutable_root(base, "field of", span)?;
        let (cbase, bty) = self.expr(base, None)?;
        let positional;
        let fname = match field {
            FieldAccess::Named(n) => n,
            // `p.0 = v` on a tuple struct assigns its field `0`.
            FieldAccess::Tuple(i) if matches!(&bty, Ty::Named { .. }) => { positional = i.to_string(); &positional }
            // `t.0 = v`. Tuples are value aggregates (immutable, like value
            // structs): rebuild the tuple instead of assigning an element.
            FieldAccess::Tuple(_) => return err("tuples are immutable value types; rebuild the tuple instead of assigning `.N` (e.g. `t = (new0, t.1)`)", span),
        };
        let Ty::Named { name, args: struct_args } = &bty else {
            return err("field assignment on a non-struct", span);
        };
        let s = self.ctx.structs.get(name).cloned()
            .ok_or_else(|| LowerError { msg: format!("`{}` is not a struct", name), span })?;
        if s.is_value {
            return err("value structs are immutable; rebuild instead of assigning a field", span);
        }
        let StructBody::Named(def_fields) = &s.body else {
            return err("field assignment on a non-record struct", span);
        };
        let idx = def_fields.iter().position(|f| &f.name == fname)
            .ok_or_else(|| LowerError { msg: format!("no field `{}` on `{}`", fname, name), span })?;
        let gparams: Vec<String> = s.generics.params.iter().map(|p| p.name.clone()).collect();
        let subst: HashMap<String, Ty> = gparams.iter().cloned().zip(struct_args.iter().cloned()).collect();
        let fty = apply_subst(&lower_type(&def_fields[idx].ty, &gparams, self.ctx).map_err(conv)?, &subst);
        if matches!(fty, Ty::Prim(Prim::RawPtr)) {
            self.require_unsafe("writing a RawPtr record field", span)?;
        }
        if let Some(binop) = op {
            if matches!(self.repr_of(&fty, span)?, Repr::Scalar(_)) { self.check_scalar_binary(*binop, &fty, span)?; }
        }
        let loc = match &cbase.repr {
            Repr::Ref(lid) => self.reg.layouts[*lid as usize].field_map[idx],
            _ => return err("field assignment on non-reference", span),
        };
        // A place's base is evaluated once, before reading the old field and
        // evaluating the RHS. Its typed local remains rooted across calls.
        let mut stmts = Vec::new();
        let cbase = if op.is_some() {
            let id = self.fresh_local(cbase.repr.clone(), bty.clone());
            let local = CoreExpr::new(CoreExprKind::Local(id), cbase.repr.clone());
            stmts.push(CoreStmt::Let(id, cbase));
            local
        } else { cbase };
        let new_val = match op {
            None => { let (cv, vt) = self.expr(value, Some(&fty))?; self.check_assignable(&vt, &fty, value.span)?; cv }
            Some(binop) => {
                let frepr = self.repr_of(&fty, span)?;
                let cur = CoreExpr::new(CoreExprKind::Field { base: Box::new(cbase.clone()), loc }, frepr.clone());
                if !matches!(frepr, Repr::Scalar(_)) {
                    // `s.f += v` on a non-scalar field uses the operator trait.
                    let (call, ct) = self.overloaded_binary(*binop, cur, fty.clone(), base, value, span)?;
                    self.check_assignable(&ct, &fty, span)?;
                    call
                } else {
                    let (cv, vt) = self.expr(value, Some(&fty))?;
                    self.check_assignable(&vt, &fty, value.span)?;
                    CoreExpr::new(CoreExprKind::Bin(*binop, Box::new(cur), Box::new(cv)), frepr)
                }
            }
        };
        let store = CoreExpr::new(CoreExprKind::SetField { base: Box::new(cbase), loc, value: Box::new(new_val) }, Repr::Unit);
        let result = if stmts.is_empty() { store } else {
            CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts, tail: Some(store) })), Repr::Unit)
        };
        Ok((result, Ty::unit()))
    }

    fn match_expr(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        expected: Option<&Ty>,
        span: Span,
    ) -> LResult<(CoreExpr, Ty)> {
        let (cscrut, sty) = self.expr(scrutinee, None)?;
        if self.infer.probing && has_placeholder(&sty, &self.infer.placeholders) {
            return self.probe_match(&sty, arms, expected);
        }

        // Dispatch: the fast tag-switch path handles a pure enum match with only
        // variant/wildcard arms and no guards. Anything else — a scalar/string
        // scrutinee, literal patterns, or guards — lowers to a sequential
        // if-else decision chain over the existing core primitives.
        let is_enum = matches!(&sty, Ty::Named { name, .. } if self.ctx.enums.contains_key(name));
        let any_guard = arms.iter().any(|a| a.guard.is_some());
        // The switch path handles only variant arms with simple (binding/wildcard)
        // payloads and an optional trailing wildcard. A bare binding arm (named
        // catch-all), literal, tuple, or struct pattern routes to the chain.
        let switchable = arms.iter().all(|a| match &a.pattern.kind {
            PatternKind::Wildcard => true,
            PatternKind::Variant { payload, .. } => payload.iter().all(|p| matches!(
                &p.kind, PatternKind::Binding { .. } | PatternKind::Wildcard
            )),
            _ => false,
        });
        if !is_enum || any_guard || !switchable {
            return self.match_chain(cscrut, sty, arms, expected, span);
        }

        let Ty::Named { name: enum_name, args: enum_args } = &sty else {
            return err("match on a non-enum is not yet supported in v0", span);
        };
        let enum_name = enum_name.clone();
        let enum_args = enum_args.clone();
        // Variant payload types, with the enum's concrete type args substituted.
        let variants: Vec<(u32, Vec<Ty>)> = self.enum_variants(&enum_name, &enum_args)?;
        let mut carms = Vec::new();
        let mut result_ty: Option<Ty> = None;
        for arm in arms {
            self.push_scope();
            let (tag, binds) = match &arm.pattern.kind {
                PatternKind::Wildcard => {
                    // wildcard: treat as a catch-all → lower as the remaining
                    // tags is complex; v0 requires explicit variant arms but
                    // allows a trailing `_`. Encode as tag = u32::MAX sentinel.
                    (u32::MAX, vec![])
                }
                PatternKind::Variant { path, payload } => {
                    let (tag, ptys) = self.variant_fields(&sty, path, arm.pattern.span)?;
                    if payload.len() != ptys.len() {
                        return err(format!("variant `{}` binds {} fields, pattern has {}", path.last(), ptys.len(), payload.len()), arm.pattern.span);
                    }
                    let mut binds = Vec::new();
                    for (p, pty) in payload.iter().zip(&ptys) {
                        let (bname, is_mut) = match &p.kind {
                            PatternKind::Binding { name, is_mut } => (name.clone(), *is_mut),
                            PatternKind::Wildcard => ("_".to_string(), false),
                            _ => return err("nested patterns not yet supported in v0", p.span),
                        };
                        let prepr = self.repr_of(pty, p.span)?;
                        let id = self.fresh_local(prepr, pty.clone());
                        self.bind_mut(&bname, id, pty.clone(), is_mut);
                        binds.push(id);
                    }
                    (tag, binds)
                }
                _ => return err("only variant / wildcard patterns supported in match (v0)", arm.pattern.span),
            };
            if arm.guard.is_some() {
                return err("match guards not yet supported in v0", arm.span);
            }
            let (body, bty) = self.expr(&arm.body, expected.or_else(|| result_ty.as_ref().filter(|ty| !is_never(ty))))?;
            match &result_ty {
                Some(rt) if !is_never(rt) => self.check_assignable(&bty, rt, arm.span)?,
                _ => result_ty = Some(bty),
            }
            carms.push(CoreArm { tag, binds, body });
            self.pop_scope();
        }

        // Exhaustiveness: every variant must be covered, or a wildcard arm
        // (tag == u32::MAX) must be present. A non-exhaustive match is an error.
        let has_wildcard = carms.iter().any(|a| a.tag == u32::MAX);
        if !has_wildcard {
            let covered: std::collections::HashSet<u32> = carms.iter().map(|a| a.tag).collect();
            let missing: Vec<&str> = variants.iter()
                .filter(|(t, _)| !covered.contains(t))
                .filter_map(|(t, _)| {
                    self.ctx.enums.get(&enum_name)
                        .and_then(|e| e.variants.get(*t as usize))
                        .map(|v| v.name.as_str())
                })
                .collect();
            if !missing.is_empty() {
                return err(
                    format!("non-exhaustive match on `{}`: missing variant(s) {} (add the arms or a `_` wildcard)",
                        enum_name.rsplit("::").next().unwrap(), missing.join(", ")),
                    span,
                );
            }
        }

        let rty = result_ty.unwrap_or_else(Ty::unit);
        let repr = self.repr_of(&rty, span)?;
        // Value enums match on the inline aggregate's tag; reference enums on
        // the heap object's tag word.
        let node = match &cscrut.repr {
            Repr::Value(_) => CoreExprKind::ValueMatch { scrutinee: Box::new(cscrut), arms: carms },
            _ => CoreExprKind::Match { scrutinee: Box::new(cscrut), arms: carms },
        };
        Ok((CoreExpr::new(node, repr), rty))
    }

    /// A `let` inference probe matching on a value whose type is still a
    /// placeholder: bind each arm's names with placeholder types and lower
    /// the guards and bodies only for the constraints they produce.
    fn probe_match(&mut self, sty: &Ty, arms: &[MatchArm], expected: Option<&Ty>) -> LResult<(CoreExpr, Ty)> {
        let mut result: Option<Ty> = None;
        for arm in arms {
            self.push_scope();
            self.bind_probe_pattern(&arm.pattern, sty);
            let mut lowered = Ok(());
            if let Some(g) = &arm.guard {
                lowered = self.expr(g, Some(&Ty::bool())).map(|_| ());
            }
            if lowered.is_ok() {
                lowered = self.expr(&arm.body, expected).map(|(_, t)| {
                    if result.is_none() && !is_never(&t) { result = Some(t); }
                });
            }
            self.pop_scope();
            if let Err(e) = lowered {
                if !self.probe_tolerates() { return Err(e); }
            }
        }
        Ok((CoreExpr::new(CoreExprKind::Unit, Repr::Unit), result.unwrap_or_else(Ty::unit)))
    }

    /// Bind a pattern's names for `probe_match`, as far as the type allows.
    fn bind_probe_pattern(&mut self, p: &Pattern, ty: &Ty) {
        match &p.kind {
            PatternKind::Binding { name, is_mut } => {
                let id = self.fresh_local(Repr::Unit, ty.clone());
                self.bind_mut(name, id, ty.clone(), *is_mut);
            }
            PatternKind::Variant { path, payload } => {
                if let Ok((_, fields)) = self.variant_fields(ty, path, p.span) {
                    let payload = expand_rest(payload, fields.len()).unwrap_or_else(|| payload.clone());
                    for (sp, fty) in payload.iter().zip(&fields) { self.bind_probe_pattern(sp, fty); }
                }
            }
            PatternKind::Tuple(ps) => {
                if let Ty::Tuple(elems) = ty {
                    let ps = expand_rest(ps, elems.len()).unwrap_or_else(|| ps.clone());
                    for (sp, ety) in ps.iter().zip(elems) { self.bind_probe_pattern(sp, ety); }
                }
            }
            PatternKind::Struct { fields, .. } => {
                if let Ok((names, tys)) = self.struct_field_tys(ty, p.span) {
                    for (fname, sp) in fields {
                        if let Some(i) = names.iter().position(|n| n == fname) { self.bind_probe_pattern(sp, &tys[i]); }
                    }
                }
            }
            PatternKind::At { name, is_mut, pat } => {
                let id = self.fresh_local(Repr::Unit, ty.clone());
                self.bind_mut(name, id, ty.clone(), *is_mut);
                self.bind_probe_pattern(pat, ty);
            }
            PatternKind::Wildcard | PatternKind::Literal(_) => {}
        }
    }

    /// Lower a match as a sequential if-else decision chain. Handles scalar /
    /// string scrutinees, literal patterns, bare-binding catch-alls, and guards
    /// — anything the fast enum tag-switch can't. Built entirely from existing
    /// core primitives (`Let`, `If`, `Bin(Eq)`, `StrEq`), so no codegen changes.
    fn match_chain(
        &mut self,
        cscrut: CoreExpr,
        sty: Ty,
        arms: &[MatchArm],
        expected: Option<&Ty>,
        span: Span,
    ) -> LResult<(CoreExpr, Ty)> {
        // Bind the scrutinee to a fresh local so we evaluate it once and can test
        // it repeatedly.
        let srepr = cscrut.repr.clone();
        let sid = self.fresh_local(srepr.clone(), sty.clone());
        let scrut_local = |this: &Self| CoreExpr::new(CoreExprKind::Local(sid), this.locals[sid as usize].clone());

        let mut result_ty: Option<Ty> = None;
        struct Built { cond: Option<CoreExpr>, binds: Vec<(LocalId, CoreExpr)>, body: CoreExpr }
        let mut built: Vec<Built> = Vec::new();

        for arm in arms {
            self.push_scope();
            // The pattern's test, and the bindings to evaluate once it holds.
            let (cond, mut binds, _) =
                self.pattern_test(&arm.pattern, &scrut_local(self), &sty, arm.pattern.span)?;
            // A guard runs after the bindings, which it may reference:
            // `cond && { let binds...; guard }`. The bindings then stay
            // assigned for the body.
            let mut full_cond = cond;
            if let Some(g) = &arm.guard {
                let (cg, gt) = self.expr(g, Some(&Ty::bool()))?;
                self.check_assignable(&gt, &Ty::bool(), g.span)?;
                let bind_stmts: Vec<CoreStmt> = std::mem::take(&mut binds).into_iter().map(|(id, init)| CoreStmt::Let(id, init)).collect();
                let guarded = CoreExpr::new(
                    CoreExprKind::Block(Box::new(CoreBlock { stmts: bind_stmts, tail: Some(cg) })),
                    Repr::Scalar(ScalarRepr::Bool),
                );
                full_cond = Some(match full_cond {
                    Some(c) => CoreExpr::new(CoreExprKind::Bin(BinOp::And, Box::new(c), Box::new(guarded)), Repr::Scalar(ScalarRepr::Bool)),
                    None => guarded,
                });
            }
            let (body, bty) = self.expr(&arm.body, expected.or_else(|| result_ty.as_ref().filter(|ty| !is_never(ty))))?;
            match &result_ty {
                Some(rt) if !is_never(rt) => self.check_assignable(&bty, rt, arm.span)?,
                _ => result_ty = Some(bty),
            }
            // Move bindings out for emission.
            let arm_binds = std::mem::take(&mut binds);
            built.push(Built { cond: full_cond, binds: arm_binds, body });
            self.pop_scope();
        }

        // Exhaustiveness over the unguarded arms (a guard may always fail).
        let unguarded: Vec<&Pattern> = arms.iter().filter(|a| a.guard.is_none()).map(|a| &a.pattern).collect();
        if !self.match_exhaustive(&sty, &unguarded, span)? {
            return err(
                "non-exhaustive match: add a `_` wildcard or binding arm to cover all cases",
                span,
            );
        }
        // The chain's innermost else is reached only if no arm matched, which
        // exhaustiveness rules out. If the last arm is unguarded, any value
        // reaching it matches it (every other unguarded arm already failed), so
        // its test is dropped; this also avoids an empty value-less `else`.
        // A guarded last arm keeps its test and gets an unreachable `else`.
        let last_guarded = arms.last().is_some_and(|a| a.guard.is_some());
        if !last_guarded {
            if let Some(last) = built.last_mut() {
                last.cond = None;
            }
        }

        let rty = result_ty.unwrap_or_else(Ty::unit);
        let repr = self.repr_of(&rty, span)?;

        // Fold the arms into nested ifs, from the last arm up:
        // `if cond { let binds...; body } else { rest }`. Bindings read the
        // scrutinee's payloads, so they run only once the test has passed (a
        // guarded arm already ran them inside its condition). The innermost
        // "else" is unreachable: the exhaustiveness check guarantees coverage.
        let unit_repr = repr.clone();
        let mut chain: Option<CoreExpr> = if last_guarded {
            let msg = CoreExpr::new(CoreExprKind::ConstStr("no match arm applies (compiler invariant)".into()), Repr::Ref(self.string_layout_id(span)?));
            Some(CoreExpr::new(CoreExprKind::Panic(Box::new(msg)), Repr::Unit))
        } else {
            None
        };
        for b in built.into_iter().rev() {
            let bind_stmts: Vec<CoreStmt> = b.binds.into_iter().map(|(id, init)| CoreStmt::Let(id, init)).collect();
            let arm_body = CoreBlock { stmts: bind_stmts, tail: Some(b.body) };
            let arm_expr = match b.cond {
                None => CoreExpr::new(CoreExprKind::Block(Box::new(arm_body)), unit_repr.clone()),
                Some(cond) => {
                    let else_block = match chain.take() {
                        Some(e) => CoreBlock { stmts: vec![], tail: Some(e) },
                        None => CoreBlock { stmts: vec![], tail: None },
                    };
                    CoreExpr::new(
                        CoreExprKind::If(Box::new(cond), Box::new(arm_body), Box::new(else_block)),
                        unit_repr.clone(),
                    )
                }
            };
            chain = Some(arm_expr);
        }

        // Emit `{ let scrut = <cscrut>; <chain> }`.
        let chain = chain.unwrap();
        let block = CoreBlock {
            stmts: vec![CoreStmt::Let(sid, cscrut)],
            tail: Some(chain),
        };
        Ok((CoreExpr::new(CoreExprKind::Block(Box::new(block)), repr), rty))
    }

    /// Compute the test for `pat` against `scrut` (a core expr that is cheap to
    /// repeat, normally a local) and the bindings it introduces. Returns
    /// `(cond, binds, irrefutable)`; `cond == None` means it always matches.
    ///
    /// Sub-patterns are tested left to right with short-circuit `&&`, so a
    /// payload is only read after its enum tag has been checked. Each binding
    /// is a `(LocalId, init)` pair that the caller must evaluate only after
    /// `cond` holds; the names are bound into the current scope here.
    fn pattern_test(
        &mut self,
        pat: &Pattern,
        scrut: &CoreExpr,
        sty: &Ty,
        span: Span,
    ) -> LResult<(Option<CoreExpr>, Vec<(LocalId, CoreExpr)>, bool)> {
        let bool_repr = Repr::Scalar(ScalarRepr::Bool);
        let and_all = |conds: Vec<CoreExpr>| -> Option<CoreExpr> {
            conds.into_iter().reduce(|a, b| CoreExpr::new(CoreExprKind::Bin(BinOp::And, Box::new(a), Box::new(b)), bool_repr.clone()))
        };
        match &pat.kind {
            PatternKind::Wildcard => Ok((None, vec![], true)),
            PatternKind::Binding { name, is_mut } => {
                let id = self.fresh_local(scrut.repr.clone(), sty.clone());
                self.bind_mut(name, id, sty.clone(), *is_mut);
                Ok((None, vec![(id, scrut.clone())], true))
            }
            PatternKind::At { name, is_mut, pat } => {
                let (cond, mut binds, irrefutable) = self.pattern_test(pat, scrut, sty, span)?;
                let id = self.fresh_local(scrut.repr.clone(), sty.clone());
                self.bind_mut(name, id, sty.clone(), *is_mut);
                binds.insert(0, (id, scrut.clone()));
                Ok((cond, binds, irrefutable))
            }
            PatternKind::Literal(lit) => {
                let cond = self.literal_eq(scrut, lit, sty, span)?;
                Ok((Some(cond), vec![], false))
            }
            PatternKind::Variant { path, payload } => {
                let (tag, fields) = self.variant_fields(sty, path, span)?;
                let expanded = expand_rest(payload, fields.len());
                let payload = expanded.as_ref().unwrap_or(payload);
                if payload.len() != fields.len() {
                    return err(format!("variant `{}` has {} fields, pattern has {}", path.last(), fields.len(), payload.len()), span);
                }
                let subpatterns: Vec<&Pattern> = payload.iter().collect();
                self.variant_test(scrut, tag, &fields, &subpatterns, span)
            }
            PatternKind::Struct { path, fields, rest } => {
                if let Ty::Named { name, .. } = sty {
                    if self.ctx.enums.contains_key(name) {
                        // `Enum::Variant { a, b }`: a variant with named fields.
                        let (tag, vfields) = self.variant_fields(sty, path, span)?;
                        let names = self.variant_field_names(name, tag);
                        let mut subpatterns: Vec<&Pattern> = Vec::new();
                        let wildcard = Pattern { kind: PatternKind::Wildcard, span };
                        let mut picked: Vec<Option<&Pattern>> = vec![None; vfields.len()];
                        for (fname, fpat) in fields {
                            let i = names.iter().position(|n| n == fname).ok_or_else(|| LowerError {
                                msg: format!("variant `{}` has no field `{}`", path.last(), fname),
                                span: fpat.span,
                            })?;
                            picked[i] = Some(fpat);
                        }
                        if !*rest && picked.iter().any(|p| p.is_none()) {
                            return err(format!("pattern for `{}` does not mention every field; add `..`", path.last()), span);
                        }
                        for p in &picked { subpatterns.push(p.unwrap_or(&wildcard)); }
                        return self.variant_test(scrut, tag, &vfields, &subpatterns, span);
                    }
                }
                let Ty::Named { name, .. } = sty else {
                    return err(format!("struct pattern against `{}`", ty_display(sty)), span);
                };
                self.check_constructor_pattern(&path.segments.join("::"), sty, span)?;
                let def = self.ctx.structs.get(name).cloned()
                    .ok_or_else(|| LowerError { msg: format!("`{}` is not a struct", name), span })?;
                let StructBody::Named(def_fields) = &def.body else {
                    return err(format!("`{}` has no named fields", path.last()), span);
                };
                if !*rest && fields.len() != def_fields.len() {
                    return err(format!("pattern for `{}` does not mention every field; add `..`", path.last()), span);
                }
                let mut conds = Vec::new();
                let mut binds = Vec::new();
                let mut irrefutable = true;
                for (fname, fpat) in fields {
                    let (field, fty) = self.field_read(scrut.clone(), sty, fname, fpat.span)?;
                    let (c, b, irr) = self.pattern_test(fpat, &field, &fty, fpat.span)?;
                    conds.extend(c);
                    binds.extend(b);
                    irrefutable &= irr;
                }
                Ok((and_all(conds), binds, irrefutable))
            }
            PatternKind::Tuple(ps) => {
                let Ty::Tuple(elems) = sty else {
                    return err(format!("tuple pattern against `{}`", ty_display(sty)), span);
                };
                let expanded = expand_rest(ps, elems.len());
                let ps = expanded.as_ref().unwrap_or(ps);
                if ps.len() != elems.len() {
                    return err(format!("tuple pattern has {} elements, the value has {}", ps.len(), elems.len()), span);
                }
                let mut conds = Vec::new();
                let mut binds = Vec::new();
                let mut irrefutable = true;
                for (i, (p, ety)) in ps.iter().zip(elems).enumerate() {
                    let erepr = self.repr_of(ety, p.span)?;
                    let field = CoreExpr::new(
                        CoreExprKind::Field { base: Box::new(scrut.clone()), loc: FieldLoc::ValueField { index: i as u32 } },
                        erepr,
                    );
                    let (c, b, irr) = self.pattern_test(p, &field, ety, p.span)?;
                    conds.extend(c);
                    binds.extend(b);
                    irrefutable &= irr;
                }
                Ok((and_all(conds), binds, irrefutable))
            }
        }
    }

    /// The test for an enum variant with the given payload sub-patterns: the
    /// tag check first, then each payload's own test.
    fn variant_test(
        &mut self,
        scrut: &CoreExpr,
        tag: u32,
        fields: &[Ty],
        subpatterns: &[&Pattern],
        span: Span,
    ) -> LResult<(Option<CoreExpr>, Vec<(LocalId, CoreExpr)>, bool)> {
        let bool_repr = Repr::Scalar(ScalarRepr::Bool);
        let tag_read = CoreExpr::new(CoreExprKind::EnumTag(Box::new(scrut.clone())), Repr::Scalar(ScalarRepr::I32));
        let tag_const = CoreExpr::new(CoreExprKind::ConstInt(tag as u64, ScalarRepr::I32), Repr::Scalar(ScalarRepr::I32));
        let mut cond = CoreExpr::new(CoreExprKind::Bin(BinOp::Eq, Box::new(tag_read), Box::new(tag_const)), bool_repr.clone());
        // The physical slot of each payload field depends on the whole list.
        let payload_reprs: Vec<Repr> = fields.iter().map(|t| self.repr_of(t, span)).collect::<LResult<_>>()?;
        let mut binds = Vec::new();
        for (i, (p, fty)) in subpatterns.iter().zip(fields).enumerate() {
            let field = CoreExpr::new(
                CoreExprKind::EnumPayload {
                    scrutinee: Box::new(scrut.clone()),
                    field: i as u32,
                    repr: payload_reprs[i].clone(),
                    payload_reprs: payload_reprs.clone(),
                },
                payload_reprs[i].clone(),
            );
            let (c, b, _) = self.pattern_test(p, &field, fty, p.span)?;
            if let Some(c) = c {
                cond = CoreExpr::new(CoreExprKind::Bin(BinOp::And, Box::new(cond), Box::new(c)), bool_repr.clone());
            }
            binds.extend(b);
        }
        Ok((Some(cond), binds, false))
    }

    /// Is a match over `sty` with these unguarded arm patterns exhaustive?
    /// A wildcard row is "useful" against the arms exactly when some value
    /// matches none of them (Maranget's usefulness algorithm).
    fn match_exhaustive(&mut self, sty: &Ty, patterns: &[&Pattern], span: Span) -> LResult<bool> {
        let mut rows = Vec::new();
        for p in patterns {
            rows.push(vec![self.space_pat(p, sty, span)?]);
        }
        Ok(!self.useful(&rows, &[SpacePat::Wild], &[sty.clone()], span)?)
    }

    /// Normalize a pattern for the exhaustiveness check.
    fn space_pat(&mut self, p: &Pattern, ty: &Ty, span: Span) -> LResult<SpacePat> {
        Ok(match &p.kind {
            PatternKind::Wildcard | PatternKind::Binding { .. } => SpacePat::Wild,
            PatternKind::At { pat, .. } => self.space_pat(pat, ty, span)?,
            PatternKind::Literal(LitPattern::Bool(b)) => SpacePat::Ctor(*b as u32, vec![]),
            PatternKind::Literal(lit) => SpacePat::Lit(format!("{:?}", lit)),
            PatternKind::Variant { path, payload } => {
                let (tag, fields) = self.variant_fields(ty, path, span)?;
                let expanded = expand_rest(payload, fields.len());
                let payload = expanded.as_ref().unwrap_or(payload);
                let mut subs = Vec::new();
                for (sp, st) in payload.iter().zip(&fields) { subs.push(self.space_pat(sp, st, span)?); }
                SpacePat::Ctor(tag, subs)
            }
            PatternKind::Struct { path, fields, .. } => {
                let (tag, names, tys) = match ty {
                    Ty::Named { name, .. } if self.ctx.enums.contains_key(name) => {
                        let (tag, tys) = self.variant_fields(ty, path, span)?;
                        (tag, self.variant_field_names(name, tag), tys)
                    }
                    _ => {
                        let (names, tys) = self.struct_field_tys(ty, span)?;
                        (0, names, tys)
                    }
                };
                let mut subs = Vec::new();
                for (n, st) in names.iter().zip(&tys) {
                    subs.push(match fields.iter().find(|(f, _)| f == n) {
                        Some((_, sp)) => self.space_pat(sp, st, span)?,
                        None => SpacePat::Wild,
                    });
                }
                SpacePat::Ctor(tag, subs)
            }
            PatternKind::Tuple(ps) => {
                let Ty::Tuple(elems) = ty else { return err("tuple pattern against a non-tuple", span) };
                let expanded = expand_rest(ps, elems.len());
                let ps = expanded.as_ref().unwrap_or(ps);
                let mut subs = Vec::new();
                for (sp, st) in ps.iter().zip(elems) { subs.push(self.space_pat(sp, st, span)?); }
                SpacePat::Ctor(0, subs)
            }
        })
    }

    /// Every constructor of `ty` with its field types, or `None` when the
    /// type has unboundedly many values (integers, strings, ...).
    fn constructors(&mut self, ty: &Ty) -> LResult<Option<Vec<(u32, Vec<Ty>)>>> {
        Ok(match ty {
            Ty::Prim(Prim::Bool) => Some(vec![(0, vec![]), (1, vec![])]),
            Ty::Tuple(elems) => Some(vec![(0, elems.clone())]),
            Ty::Named { name, args } if self.ctx.enums.contains_key(name) => Some(self.enum_variants(name, args)?),
            Ty::Named { name, .. } if self.ctx.structs.contains_key(name) => {
                Some(vec![(0, self.struct_field_tys(ty, Span::new(0, 0))?.1)])
            }
            _ => None,
        })
    }

    /// Field names and types of a struct type, in declaration order.
    fn struct_field_tys(&mut self, ty: &Ty, span: Span) -> LResult<(Vec<String>, Vec<Ty>)> {
        let Ty::Named { name, args } = ty else { return err("struct pattern against a non-struct", span) };
        let s = self.ctx.structs.get(name).cloned()
            .ok_or_else(|| LowerError { msg: format!("`{}` is not a struct", name), span })?;
        let StructBody::Named(def_fields) = &s.body else { return err("struct pattern against a non-record struct", span) };
        let gparams: Vec<String> = s.generics.params.iter().map(|p| p.name.clone()).collect();
        let subst: HashMap<String, Ty> = gparams.iter().cloned().zip(args.iter().cloned()).collect();
        let mut names = Vec::new();
        let mut tys = Vec::new();
        for f in def_fields {
            names.push(f.name.clone());
            tys.push(apply_subst(&lower_type(&f.ty, &gparams, self.ctx).map_err(conv)?, &subst));
        }
        Ok((names, tys))
    }

    /// Is the pattern vector `q` (over column types `tys`) matched by some
    /// value that no row of `rows` matches?
    fn useful(&mut self, rows: &[Vec<SpacePat>], q: &[SpacePat], tys: &[Ty], span: Span) -> LResult<bool> {
        if q.is_empty() {
            return Ok(rows.is_empty());
        }
        let rest_tys = &tys[1..];
        match &q[0] {
            SpacePat::Ctor(c, subs) => {
                let sub_tys = self.ctor_field_tys(&tys[0], *c)?;
                let spec = specialize_ctor(rows, *c, subs.len());
                let mut q2 = subs.clone();
                q2.extend_from_slice(&q[1..]);
                let mut t2 = sub_tys;
                t2.extend_from_slice(rest_tys);
                self.useful(&spec, &q2, &t2, span)
            }
            SpacePat::Lit(l) => {
                let spec: Vec<Vec<SpacePat>> = rows.iter().filter_map(|r| match &r[0] {
                    SpacePat::Lit(m) if m == l => Some(r[1..].to_vec()),
                    SpacePat::Wild => Some(r[1..].to_vec()),
                    _ => None,
                }).collect();
                self.useful(&spec, &q[1..], rest_tys, span)
            }
            SpacePat::Wild => {
                let ctors = self.constructors(&tys[0])?;
                let heads: std::collections::HashSet<u32> = rows.iter().filter_map(|r| match &r[0] {
                    SpacePat::Ctor(c, _) => Some(*c),
                    _ => None,
                }).collect();
                if let Some(ctors) = ctors.filter(|cs| !cs.is_empty() && cs.iter().all(|(c, _)| heads.contains(c))) {
                    // Every constructor appears: the wildcard is useful if it
                    // is useful under some constructor.
                    for (c, sub_tys) in ctors {
                        let spec = specialize_ctor(rows, c, sub_tys.len());
                        let mut q2 = vec![SpacePat::Wild; sub_tys.len()];
                        q2.extend_from_slice(&q[1..]);
                        let mut t2 = sub_tys;
                        t2.extend_from_slice(rest_tys);
                        if self.useful(&spec, &q2, &t2, span)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                } else {
                    // Some constructor is missing: only wildcard rows cover it.
                    let default: Vec<Vec<SpacePat>> = rows.iter()
                        .filter(|r| matches!(r[0], SpacePat::Wild))
                        .map(|r| r[1..].to_vec())
                        .collect();
                    self.useful(&default, &q[1..], rest_tys, span)
                }
            }
        }
    }

    /// The field types of constructor `c` of `ty`.
    fn ctor_field_tys(&mut self, ty: &Ty, c: u32) -> LResult<Vec<Ty>> {
        Ok(self.constructors(ty)?
            .and_then(|cs| cs.into_iter().find(|(t, _)| *t == c).map(|(_, tys)| tys))
            .unwrap_or_default())
    }

    fn check_constructor_pattern(&self, written: &str, actual: &Ty, span: Span) -> LResult<()> {
        let canonical = self.ctx.canon(written).ok_or_else(|| LowerError {
            msg: format!("unknown or ambiguous type `{written}` in pattern"), span,
        })?;
        if let Some(alias) = self.ctx.aliases.get(&canonical) {
            let mut inferred = HashMap::new();
            unify_infer(&alias.target, actual, &mut inferred);
            check_assignable(actual, &apply_subst(&alias.target, &inferred), span)?;
            self.check_alias_bounds(&canonical, actual, span)
        } else if matches!(actual, Ty::Named { name, .. } if name == &canonical) {
            Ok(())
        } else {
            err(format!("pattern for `{written}` against `{}`", ty_display(actual)), span)
        }
    }

    /// The tag and payload types of the variant `path` of enum type `sty`.
    fn variant_fields(&mut self, sty: &Ty, path: &Path, span: Span) -> LResult<(u32, Vec<Ty>)> {
        let Ty::Named { name, args } = sty else {
            return err(format!("variant pattern against `{}`", ty_display(sty)), span);
        };
        if !self.ctx.enums.contains_key(name) {
            return err(format!("variant pattern against `{}`, which is not an enum", ty_display(sty)), span);
        }
        if path.segments.len() > 1 {
            self.check_constructor_pattern(&path.segments[..path.segments.len() - 1].join("::"), sty, span)?;
        }
        let variants = self.enum_variants(name, args)?;
        self.variant_in(name, path.last(), &variants, span)
    }

    /// The field names of variant `tag` of `enum_name` (empty for tuple or
    /// unit variants).
    fn variant_field_names(&self, enum_name: &str, tag: u32) -> Vec<String> {
        match self.ctx.enums.get(enum_name).and_then(|e| e.variants.get(tag as usize)).map(|v| &v.payload) {
            Some(VariantPayload::Named(fields)) => fields.iter().map(|f| f.name.clone()).collect(),
            _ => vec![],
        }
    }

    /// Build a boolean equality test `scrut == <literal>` for a literal pattern.
    fn literal_eq(&mut self, scrut: &CoreExpr, lit: &LitPattern, sty: &Ty, span: Span) -> LResult<CoreExpr> {
        let bool_repr = Repr::Scalar(ScalarRepr::Bool);
        match lit {
            LitPattern::Int(n) => {
                let sr = match &scrut.repr { Repr::Scalar(s) if !s.is_float() => *s, _ => return err("integer literal pattern against a non-integer scrutinee", span) };
                let k = CoreExpr::new(CoreExprKind::ConstInt(*n, sr), Repr::Scalar(sr));
                Ok(CoreExpr::new(CoreExprKind::Bin(BinOp::Eq, Box::new(scrut.clone()), Box::new(k)), bool_repr))
            }
            LitPattern::Bool(b) => {
                if !matches!(sty, Ty::Prim(Prim::Bool)) { return err("bool literal pattern against a non-bool scrutinee", span); }
                let k = CoreExpr::new(CoreExprKind::ConstBool(*b), bool_repr.clone());
                Ok(CoreExpr::new(CoreExprKind::Bin(BinOp::Eq, Box::new(scrut.clone()), Box::new(k)), bool_repr))
            }
            LitPattern::Char(c) => {
                if !matches!(sty, Ty::Prim(Prim::Char)) { return err("char literal pattern against a non-char scrutinee", span); }
                let k = CoreExpr::new(CoreExprKind::ConstChar(*c), Repr::Scalar(ScalarRepr::Char));
                Ok(CoreExpr::new(CoreExprKind::Bin(BinOp::Eq, Box::new(scrut.clone()), Box::new(k)), bool_repr))
            }
            LitPattern::Str(s) => {
                if !matches!(sty, Ty::Prim(Prim::Str)) { return err("string literal pattern against a non-string scrutinee", span); }
                let lid = match self.repr_of(&Ty::Prim(Prim::Str), span)? { Repr::Ref(l) => l, _ => return err("internal: String repr", span) };
                // This ConstStr is a real varlen heap alloc at codegen — span it so
                // string-literal match patterns get file:line:col like every other.
                let sid = self.mono.intern_span(span);
                let k = CoreExpr::new(CoreExprKind::ConstStr(s.clone()), Repr::Ref(lid)).at(sid);
                Ok(CoreExpr::new(CoreExprKind::StrEq(Box::new(scrut.clone()), Box::new(k)), bool_repr))
            }
        }
    }

    /// `array_new(len)` — allocate a varlen array. Element type from `expected`.
    fn array_new(&mut self, len: &Expr, expected: Option<&Ty>, span: Span) -> LResult<(CoreExpr, Ty)> {
        let elem_ty = match expected {
            Some(Ty::Named { name, args }) if name == "Array" && !args.is_empty() => args[0].clone(),
            _ => return err("`Array::new` requires an expected `Array<T>` type (annotate the let binding)", span),
        };
        let elem = self.repr_of(&elem_ty, span)?;
        let lid = self.reg.array_for(&elem).map_err(|e| LowerError { msg: e.0, span })?;
        let (clen, lt) = self.expr(len, Some(&Ty::i64()))?;
        self.check_assignable(&lt, &Ty::i64(), len.span)?;
        let ty = Ty::Named { name: "Array".into(), args: vec![elem_ty] };
        let sid = self.mono.intern_span(span);
        Ok((CoreExpr::new(CoreExprKind::ArrayNew { layout: lid, len: Box::new(clen), elem }, Repr::Ref(lid)).at(sid), ty))
    }

    fn array_len_op(&mut self, arr: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        let (ca, aty) = self.expr(arr, None)?;
        if array_elem_ty(&aty).is_none() {
            return err("array_len on a non-array", span);
        }
        Ok((CoreExpr::new(CoreExprKind::ArrayLen(Box::new(ca)), Repr::Scalar(ScalarRepr::I64)), Ty::i64()))
    }

    fn array_get_op(&mut self, arr: &Expr, idx: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        let (ca, aty) = self.expr(arr, None)?;
        let elem_ty = array_elem_ty(&aty).ok_or_else(|| LowerError { msg: "array_get on a non-array".into(), span })?;
        let elem = self.repr_of(&elem_ty, span)?;
        let (ci, it) = self.expr(idx, Some(&Ty::i64()))?;
        self.check_assignable(&it, &Ty::i64(), idx.span)?;
        // `array_get` yields `Option<T>` — `None` for an out-of-bounds index —
        // so the absence of a value is carried in the type rather than aborting.
        // The `elem` repr drives the in-bounds load; the CoreExpr repr is the
        // Option value enum that codegen constructs (Some/None).
        let opt_ty = Ty::Named { name: "Option".to_string(), args: vec![elem_ty] };
        let opt_repr = self.repr_of(&opt_ty, span)?;
        Ok((CoreExpr::new(CoreExprKind::ArrayGet { array: Box::new(ca), index: Box::new(ci), elem }, opt_repr), opt_ty))
    }

    /// The `a[i]` index operator: bounds-checked, yields `T` directly, aborts on
    /// out-of-bounds (Rust-like). The `array_get` function returns `Option<T>`.
    /// `base[idx]`: a bounds-checked element read for an `Array`, otherwise a
    /// call to the base type's `Index::index`.
    fn index_get(&mut self, base: &Expr, idx: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        if let ExprKind::Range { lo, hi, inclusive } = &*idx.kind {
            return self.index_range(base, lo, hi, *inclusive, span);
        }
        let (ca, aty) = self.expr(base, None)?;
        let Some(elem_ty) = array_elem_ty(&aty) else {
            return self.trait_method_call(ca, aty, Some(base), "Index", "index", "indexing", std::slice::from_ref(idx), span);
        };
        let elem = self.repr_of(&elem_ty, span)?;
        let (ci, it) = self.expr(idx, Some(&Ty::i64()))?;
        self.check_assignable(&it, &Ty::i64(), idx.span)?;
        Ok((CoreExpr::new(CoreExprKind::ArrayGetChecked { array: Box::new(ca), index: Box::new(ci), elem: elem.clone() }, elem), elem_ty))
    }

    /// Legacy `array_get_unchecked(a, i)` returns T directly, with backend
    /// validation of backing bounds and initialized reference/value storage.
    fn array_get_unchecked_op(&mut self, arr: &Expr, idx: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        let (ca, aty) = self.expr(arr, None)?;
        let elem_ty = array_elem_ty(&aty).ok_or_else(|| LowerError { msg: "array_get_unchecked on a non-array".into(), span })?;
        let elem = self.repr_of(&elem_ty, span)?;
        let (ci, it) = self.expr(idx, Some(&Ty::i64()))?;
        self.check_assignable(&it, &Ty::i64(), idx.span)?;
        Ok((CoreExpr::new(CoreExprKind::ArrayGetUnchecked { array: Box::new(ca), index: Box::new(ci), elem: elem.clone() }, elem), elem_ty))
    }

    /// `base[lo..hi]` (either bound may be omitted; `lo..=hi` includes `hi`):
    /// a call to the base type's `IndexRange::index_range(lo, hi)`, which
    /// returns a copy of that part. The base is evaluated once.
    fn index_range(&mut self, base: &Expr, lo: &Option<Expr>, hi: &Option<Expr>, inclusive: bool, span: Span) -> LResult<(CoreExpr, Ty)> {
        if inclusive && hi.is_none() {
            return err("an inclusive range needs an end (`lo..=hi`)", span);
        }
        let (cb, bty) = self.expr(base, None)?;
        if array_elem_ty(&bty).is_some() {
            return err("range indexing is not supported on `Array`; use a `Vec`", span);
        }
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let base_var = ex(ExprKind::Path(Path::single(RANGE_BASE.to_string(), span)));
        let lo = lo.clone().unwrap_or_else(|| ex(ExprKind::Int(0, crate::lexer::NumSuffix::None)));
        let hi = match hi {
            Some(h) if inclusive => ex(ExprKind::Binary(BinOp::Add, h.clone(), ex(ExprKind::Int(1, crate::lexer::NumSuffix::None)))),
            Some(h) => h.clone(),
            None => ex(ExprKind::MethodCall { recv: base_var.clone(), method: "len".into(), args: vec![], span }),
        };
        self.push_scope();
        let id = self.fresh_local(cb.repr.clone(), bty.clone());
        self.bind_mut(RANGE_BASE, id, bty.clone(), false);
        let recv = CoreExpr::new(CoreExprKind::Local(id), cb.repr.clone());
        let call = self.trait_method_call(recv, bty, Some(&base_var), "IndexRange", "index_range", "range indexing", &[lo, hi], span);
        self.pop_scope();
        let (call, ty) = call?;
        let repr = call.repr.clone();
        Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts: vec![CoreStmt::Let(id, cb)], tail: Some(call) })), repr), ty))
    }

    /// `base[idx] = val`: an element store for an `Array`, otherwise a call to
    /// the base type's `IndexSet::index_set`.
    fn index_set(&mut self, base: &Expr, idx: &Expr, val: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        self.check_mutable_root(base, "element of", span)?;
        let (ca, aty) = self.expr(base, None)?;
        if array_elem_ty(&aty).is_none() {
            let args = [idx.clone(), val.clone()];
            let (call, _) = self.trait_method_call(ca, aty, Some(base), "IndexSet", "index_set", "indexed assignment", &args, span)?;
            return Ok((call, Ty::unit()));
        }
        self.array_store(ca, aty, idx, val, span)
    }

    /// Capture the location, then the old value, then evaluate the RHS. In
    /// particular neither an indexer nor its receiver is replayed for the store.
    fn compound_index_set(&mut self, base: &Expr, idx: &Expr, op: BinOp, rhs: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        self.check_mutable_root(base, "element of", span)?;
        let (ca, aty) = self.expr(base, None)?;
        let aid = self.fresh_local(ca.repr.clone(), aty.clone());
        let array = CoreExpr::new(CoreExprKind::Local(aid), ca.repr.clone());
        let mut stmts = vec![CoreStmt::Let(aid, ca)];
        let elem_ty = array_elem_ty(&aty);
        let getter = if elem_ty.is_none() {
            Some(self.required_trait_method(&aty, "Index", "index", "indexing", span)?)
        } else { None };
        let index_ty = if let Some(entry) = &getter {
            let impl_self = lower_type(&entry.self_ty, &entry.impl_generics, self.ctx).map_err(conv)?;
            let mut subst = HashMap::new();
            unify_infer(&impl_self, &aty, &mut subst);
            subst.insert("Self".to_string(), aty.clone());
            let mut params = entry.impl_generics.clone();
            params.extend(entry.method.generics.params.iter().map(|p| p.name.clone()));
            params.push("Self".to_string());
            let param = entry.method.params.first().ok_or_else(|| LowerError {
                msg: "Index::index must accept an index argument".into(), span,
            })?;
            apply_subst(&lower_type(&param.ty, &params, self.ctx).map_err(conv)?, &subst)
        } else { Ty::i64() };
        let (ci, it) = self.expr(idx, hint(&index_ty))?;
        let iid = self.fresh_local(ci.repr.clone(), it.clone());
        let index = CoreExpr::new(CoreExprKind::Local(iid), ci.repr.clone());
        stmts.push(CoreStmt::Let(iid, ci));
        let (old, ty) = if let Some(entry) = getter {
            self.method_call_operands(array.clone(), aty.clone(), Some(base), entry,
                &[MethodArg::Lowered(index.clone(), it.clone(), idx.span)], span)?
        } else {
            self.check_assignable(&it, &Ty::i64(), idx.span)?;
            let ty = elem_ty.clone().unwrap();
            let elem = self.repr_of(&ty, span)?;
            (CoreExpr::new(CoreExprKind::ArrayGetChecked { array: Box::new(array.clone()), index: Box::new(index.clone()), elem: elem.clone() }, elem), ty)
        };
        let oid = self.fresh_local(old.repr.clone(), ty.clone());
        let current = CoreExpr::new(CoreExprKind::Local(oid), old.repr.clone());
        stmts.push(CoreStmt::Let(oid, old));
        let (value, vt) = if matches!(current.repr, Repr::Scalar(_)) {
            self.check_scalar_binary(op, &ty, span)?;
            let (right, rt) = self.expr(rhs, Some(&ty))?;
            self.check_assignable(&rt, &ty, rhs.span)?;
            let repr = current.repr.clone();
            (CoreExpr::new(CoreExprKind::Bin(op, Box::new(current), Box::new(right)), repr), ty.clone())
        } else {
            self.overloaded_binary(op, current, ty.clone(), base, rhs, span)?
        };
        self.check_assignable(&vt, &ty, span)?;
        let store = if elem_ty.is_some() {
            let elem = self.repr_of(&ty, span)?;
            CoreExpr::new(CoreExprKind::ArraySet { array: Box::new(array), index: Box::new(index), value: Box::new(value), elem }, Repr::Unit)
        } else {
            let entry = self.required_trait_method(&aty, "IndexSet", "index_set", "indexed assignment", span)?;
            let (call, ret) = self.method_call_operands(array, aty, Some(base), entry,
                &[MethodArg::Lowered(index, it, idx.span), MethodArg::Lowered(value, vt, rhs.span)], span)?;
            self.check_assignable(&ret, &Ty::unit(), span)?;
            call
        };
        Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts, tail: Some(store) })), Repr::Unit), Ty::unit()))
    }

    fn array_set_op(&mut self, arr: &Expr, idx: &Expr, val: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        // Mutating an array element (whether via `a[i] = v` or `array_set(a, ..)`)
        // requires the array's access-path root to be a `mut` binding.
        self.check_mutable_root(arr, "element of", span)?;
        let (ca, aty) = self.expr(arr, None)?;
        self.array_store(ca, aty, idx, val, span)
    }

    fn array_store(&mut self, ca: CoreExpr, aty: Ty, idx: &Expr, val: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        let elem_ty = array_elem_ty(&aty).ok_or_else(|| LowerError { msg: "array_set on a non-array".into(), span })?;
        let elem = self.repr_of(&elem_ty, span)?;
        let (ci, it) = self.expr(idx, Some(&Ty::i64()))?;
        self.check_assignable(&it, &Ty::i64(), idx.span)?;
        let (cv, vt) = self.expr(val, Some(&elem_ty))?;
        self.check_assignable(&vt, &elem_ty, val.span)?;
        Ok((CoreExpr::new(CoreExprKind::ArraySet { array: Box::new(ca), index: Box::new(ci), value: Box::new(cv), elem }, Repr::Unit), Ty::unit()))
    }

    /// Lower the built-in `String` intrinsics. Each argument must be a `String`.
    fn str_intrinsic(&mut self, name: &str, args: &[Expr], span: Span) -> LResult<(CoreExpr, Ty)> {
        let str_ty = Ty::Prim(Prim::Str);
        // Lower an argument and require it to be a String.
        let str_arg = |this: &mut Self, e: &Expr| -> LResult<CoreExpr> {
            let (ce, t) = this.expr(e, Some(&str_ty))?;
            this.check_assignable(&t, &str_ty, e.span)?;
            Ok(ce)
        };
        match name {
            "print_str" => {
                if args.len() != 1 { return err("`print_str` takes 1 argument", span); }
                let a = str_arg(self, &args[0])?;
                Ok((CoreExpr::new(CoreExprKind::PrintStr(Box::new(a)), Repr::Scalar(ScalarRepr::I64)), Ty::i64()))
            }
            "print" => {
                if args.len() != 1 { return err("`print` takes 1 argument", span); }
                let a = str_arg(self, &args[0])?;
                Ok((CoreExpr::new(CoreExprKind::PrintStrRaw(Box::new(a)), Repr::Scalar(ScalarRepr::I64)), Ty::i64()))
            }
            "char_to_str" => {
                if args.len() != 1 { return err("`char_to_str` takes 1 argument", span); }
                let (cp, cpt) = self.expr(&args[0], Some(&Ty::i64()))?;
                self.check_assignable(&cpt, &Ty::i64(), args[0].span)?;
                let layout = self.string_layout_id(span)?;
                Ok((
                    CoreExpr::new(CoreExprKind::StrFromChar { layout, cp: Box::new(cp) }, Repr::Ref(layout)),
                    str_ty,
                ))
            }
            "str_len" => {
                if args.len() != 1 { return err("`str_len` takes 1 argument", span); }
                let a = str_arg(self, &args[0])?;
                Ok((CoreExpr::new(CoreExprKind::StrLen(Box::new(a)), Repr::Scalar(ScalarRepr::I64)), Ty::i64()))
            }
            "str_eq" => {
                if args.len() != 2 { return err("`str_eq` takes 2 arguments", span); }
                let a = str_arg(self, &args[0])?;
                let b = str_arg(self, &args[1])?;
                Ok((CoreExpr::new(CoreExprKind::StrEq(Box::new(a), Box::new(b)), Repr::Scalar(ScalarRepr::Bool)), Ty::bool()))
            }
            "str_concat" => {
                if args.len() != 2 { return err("`str_concat` takes 2 arguments", span); }
                let a = str_arg(self, &args[0])?;
                let b = str_arg(self, &args[1])?;
                let layout = self.string_layout_id(span)?;
                Ok((
                    CoreExpr::new(CoreExprKind::StrConcat { layout, a: Box::new(a), b: Box::new(b) }, Repr::Ref(layout)),
                    str_ty,
                ))
            }
            "str_get" => {
                if args.len() != 2 { return err("`str_get` takes 2 arguments", span); }
                let s = str_arg(self, &args[0])?;
                let (i, it) = self.expr(&args[1], Some(&Ty::i64()))?;
                self.check_assignable(&it, &Ty::i64(), args[1].span)?;
                Ok((CoreExpr::new(CoreExprKind::StrGet(Box::new(s), Box::new(i)), Repr::Scalar(ScalarRepr::I64)), Ty::i64()))
            }
            "str_to_float" => {
                if args.len() != 1 { return err("`str_to_float` takes 1 argument", span); }
                let s = str_arg(self, &args[0])?;
                Ok((CoreExpr::new(CoreExprKind::StrToFloat(Box::new(s)), Repr::Scalar(ScalarRepr::F64)), Ty::Prim(Prim::F64)))
            }
            "float_bits" => {
                if args.len() != 1 { return err("`float_bits` takes 1 argument", span); }
                let (f, ft) = self.expr(&args[0], Some(&Ty::Prim(Prim::F64)))?;
                self.check_assignable(&ft, &Ty::Prim(Prim::F64), args[0].span)?;
                Ok((CoreExpr::new(CoreExprKind::FloatBits(Box::new(f)), Repr::Scalar(ScalarRepr::I64)), Ty::i64()))
            }
            "read_file" => {
                if args.len() != 1 { return err("`read_file` takes 1 argument", span); }
                let p = str_arg(self, &args[0])?;
                let layout = self.string_layout_id(span)?;
                Ok((
                    CoreExpr::new(CoreExprKind::ReadFile { layout, path: Box::new(p) }, Repr::Ref(layout)),
                    str_ty,
                ))
            }
            "fmt_f64" => {
                if args.len() != 3 { return err("`fmt_f64` takes a float, a precision, and a mode", span); }
                let (v, vt) = self.expr(&args[0], Some(&Ty::Prim(Prim::F64)))?;
                self.check_assignable(&vt, &Ty::Prim(Prim::F64), args[0].span)?;
                let mut ints = Vec::new();
                for a in &args[1..] {
                    let (ci, it) = self.expr(a, Some(&Ty::i64()))?;
                    self.check_assignable(&it, &Ty::i64(), a.span)?;
                    ints.push(ci);
                }
                let layout = self.string_layout_id(span)?;
                let tid = CoreExpr::new(CoreExprKind::ConstInt(layout as u64, ScalarRepr::I64), Repr::Scalar(ScalarRepr::I64));
                let mode = ints.pop().unwrap();
                let prec = ints.pop().unwrap();
                Ok((
                    CoreExpr::new(CoreExprKind::RuntimeCall { func: "ai_fmt_f64", args: vec![tid, v, prec, mode], ret: Repr::Ref(layout) }, Repr::Ref(layout)),
                    str_ty,
                ))
            }
            "str_hash" => {
                if args.len() != 1 { return err("`str_hash` takes 1 argument", span); }
                let s = str_arg(self, &args[0])?;
                Ok((CoreExpr::new(CoreExprKind::StrHash(Box::new(s)), Repr::Scalar(ScalarRepr::I64)), Ty::i64()))
            }
            "str_substring" => {
                if args.len() != 3 { return err("`str_substring` takes 3 arguments", span); }
                let s = str_arg(self, &args[0])?;
                let (st, stt) = self.expr(&args[1], Some(&Ty::i64()))?;
                self.check_assignable(&stt, &Ty::i64(), args[1].span)?;
                let (en, ent) = self.expr(&args[2], Some(&Ty::i64()))?;
                self.check_assignable(&ent, &Ty::i64(), args[2].span)?;
                let layout = self.string_layout_id(span)?;
                Ok((
                    CoreExpr::new(CoreExprKind::StrSubstring { layout, s: Box::new(s), start: Box::new(st), end: Box::new(en) }, Repr::Ref(layout)),
                    str_ty,
                ))
            }
            "to_string" => {
                if args.len() != 1 { return err("`to_string` takes 1 argument", span); }
                let (v, vt) = self.expr(&args[0], None)?;
                let is_float = match &vt {
                    Ty::Prim(Prim::F32 | Prim::F64) => true,
                    Ty::Prim(p) if matches!(p, Prim::I8 | Prim::I16 | Prim::I32 | Prim::I64
                        | Prim::U8 | Prim::U16 | Prim::U32 | Prim::U64 | Prim::Bool | Prim::Char) => false,
                    _ => return err("`to_string` requires an integer or float argument", args[0].span),
                };
                // Widen the numeric value so the runtime sees i64 / f64.
                let v = self.widen_to_string_arg(v, &vt, is_float);
                let layout = self.string_layout_id(span)?;
                Ok((
                    CoreExpr::new(CoreExprKind::StrFromNum { layout, is_float, v: Box::new(v) }, Repr::Ref(layout)),
                    str_ty,
                ))
            }
            _ => err(format!("unknown string intrinsic `{}`", name), span),
        }
    }

    /// Lower the built-in reflection intrinsics. The first argument is always a
    /// heap (`Ref`) value — reflection reads its object header (`type_id`) and the
    /// metadata table. Field intrinsics take a second `i64` field index and lower
    /// to `RuntimeCall`s into the `ai_reflect_*` externs.
    ///
    /// Surface:
    ///   `type_id_of(x) -> i64`, `type_name_of(x) -> String`
    ///   `field_count(x) -> i64`
    ///   `field_name(x, i) -> String`, `field_kind(x, i) -> i64`, `field_i64(x, i) -> i64`
    fn reflect_intrinsic(&mut self, name: &str, args: &[Expr], span: Span) -> LResult<(CoreExpr, Ty)> {
        let arity = if matches!(name, "type_id_of" | "type_name_of" | "field_count") { 1 } else { 2 };
        if args.len() != arity {
            return err(format!("`{}` takes {} argument(s)", name, arity), span);
        }
        let (obj, oty) = self.expr(&args[0], None)?;
        if !matches!(obj.repr, Repr::Ref(_)) {
            return err(
                format!("`{}` requires a heap (reference) value, got `{:?}`", name, oty),
                span,
            );
        }
        // Field intrinsics take an i64 index as the second argument.
        let idx = if arity == 2 {
            let (i, it) = self.expr(&args[1], Some(&Ty::i64()))?;
            self.check_assignable(&it, &Ty::i64(), args[1].span)?;
            Some(i)
        } else {
            None
        };
        let i64r = Repr::Scalar(ScalarRepr::I64);
        match name {
            "type_id_of" => Ok((
                CoreExpr::new(CoreExprKind::TypeIdOf(Box::new(obj)), i64r),
                Ty::i64(),
            )),
            "type_name_of" => {
                let layout = self.string_layout_id(span)?;
                Ok((
                    CoreExpr::new(
                        CoreExprKind::TypeNameOf { layout, obj: Box::new(obj) },
                        Repr::Ref(layout),
                    ),
                    Ty::Prim(Prim::Str),
                ))
            }
            "field_count" => Ok((
                CoreExpr::new(
                    CoreExprKind::RuntimeCall {
                        func: "ai_reflect_field_count",
                        args: vec![obj],
                        ret: i64r.clone(),
                    },
                    i64r,
                ),
                Ty::i64(),
            )),
            "field_kind" | "field_i64" => {
                let func = if name == "field_kind" { "ai_reflect_field_kind" } else { "ai_reflect_field_i64" };
                Ok((
                    CoreExpr::new(
                        CoreExprKind::RuntimeCall {
                            func,
                            args: vec![obj, idx.unwrap()],
                            ret: i64r.clone(),
                        },
                        i64r,
                    ),
                    Ty::i64(),
                ))
            }
            "field_name" => {
                let layout = self.string_layout_id(span)?;
                // ai_reflect_field_name(thread, i64 str_type_id, ptr obj, i64 i)
                let str_tid = CoreExpr::new(
                    CoreExprKind::ConstInt(layout as u64, ScalarRepr::I64),
                    Repr::Scalar(ScalarRepr::I64),
                );
                Ok((
                    CoreExpr::new(
                        CoreExprKind::RuntimeCall {
                            func: "ai_reflect_field_name",
                            args: vec![str_tid, obj, idx.unwrap()],
                            ret: Repr::Ref(layout),
                        },
                        Repr::Ref(layout),
                    ),
                    Ty::Prim(Prim::Str),
                ))
            }
            _ => err(format!("unknown reflection intrinsic `{}`", name), span),
        }
    }

    /// A zero/null value of the given repr, for deferred-init `let` slots.
    fn zero_value(&self, repr: &Repr) -> CoreExpr {
        match repr {
            Repr::Scalar(ScalarRepr::Ptr) => CoreExpr::new(CoreExprKind::ConstZero(repr.clone()), repr.clone()),
            Repr::Scalar(s) if s.is_float() => CoreExpr::new(CoreExprKind::ConstFloat(0.0, *s), repr.clone()),
            Repr::Scalar(s) => CoreExpr::new(CoreExprKind::ConstInt(0, *s), repr.clone()),
            Repr::Unit => CoreExpr::new(CoreExprKind::Unit, Repr::Unit),
            _ => CoreExpr::new(CoreExprKind::ConstZero(repr.clone()), repr.clone()),
        }
    }

    /// The layout id of the built-in `String` reference type.
    fn string_layout_id(&mut self, span: Span) -> LResult<LayoutId> {
        match self.repr_of(&Ty::Prim(Prim::Str), span)? {
            Repr::Ref(l) => Ok(l),
            _ => err("internal: String is not a reference type", span),
        }
    }

    /// Widen a numeric `to_string` argument to the i64/f64 the runtime expects.
    /// Codegen's `StrFromNum` passes the value straight through, so we coerce
    /// here via a `Cast` core node when the source repr is narrower.
    fn widen_to_string_arg(&self, v: CoreExpr, ty: &Ty, is_float: bool) -> CoreExpr {
        let target = if is_float { Ty::Prim(Prim::F64) } else { Ty::i64() };
        if ty == &target {
            return v;
        }
        let from = v.repr.clone();
        let to = if is_float { Repr::Scalar(ScalarRepr::F64) } else { Repr::Scalar(ScalarRepr::I64) };
        CoreExpr::new(CoreExprKind::Cast { value: Box::new(v), from, to: to.clone() }, to)
    }

    /// Lower a closure `|params| body`. Free variables that refer to enclosing
    /// locals are captured into a heap env object; the body is lifted into a
    /// top-level function `(env, params...) -> ret`. The closure value is the
    /// env reference, with the code pointer stored in its raw section.
    /// Send/Sync check for `Thread::spawn`: every value the closure `arg`
    /// captures must be `Sync` (it is shared with the parent thread). Non-closure
    /// args are left to the normal type check. See `docs/threads.md`.
    fn check_spawn_captures_sync(&self, arg: &Expr) -> LResult<()> {
        let ExprKind::Closure { params, body, .. } = &*arg.kind else { return Ok(()) };
        let param_names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        let mut free = Vec::new();
        collect_free_vars(body, &param_names, &mut Vec::new(), &mut free);
        for name in &free {
            if let Some((_, ty)) = self.lookup(name) {
                let is_sync = self.ctx.try_is_sync(&ty).map_err(|error| LowerError {
                    msg: error.msg, span: arg.span,
                })?;
                if !is_sync {
                    return err(
                        format!(
                            "cannot capture `{}: {}` in a spawned thread — it is not `Sync` \
                             (a shared mutable reference would race). Wrap it in `Atom<{}>` (or \
                             build it from `Atom`/`AtomicI64` fields), or capture an immutable value.",
                            name, ty_display(&ty), ty_display(&ty),
                        ),
                        arg.span,
                    );
                }
            }
        }
        Ok(())
    }

    fn function_value(&mut self, path: &Path, expected: Option<&Ty>, span: Span) -> LResult<(CoreExpr, Ty)> {
        let key = path.segments.join("::");
        let definition = self.ctx.fns.get(&key).cloned().ok_or_else(|| LowerError { msg: format!("unknown function `{key}`"), span })?;
        let generic = !definition.generics.params.is_empty();
        let params: Vec<ClosureParam> = definition.params.iter().map(|param| ClosureParam {
            name: param.name.clone(), ty: if generic { None } else { Some(param.ty.clone()) }, span,
        }).collect();
        let args = params.iter().map(|param| Expr { kind: std::sync::Arc::new(ExprKind::Path(Path::single(param.name.clone(), span))), span }).collect();
        let callee = Expr { kind: std::sync::Arc::new(ExprKind::Path(path.clone())), span };
        let body = Expr { kind: std::sync::Arc::new(ExprKind::Call(callee, args)), span };
        self.closure(&params, &if generic { None } else { definition.ret.clone() }, &body, expected, span)
    }

    fn closure(
        &mut self,
        params: &[ClosureParam],
        ret: &Option<Type>,
        body: &Expr,
        expected: Option<&Ty>,
        span: Span,
    ) -> LResult<(CoreExpr, Ty)> {
        // Param types: from annotations, else from the `expected` fn type.
        let exp_params: Vec<Ty> = match expected {
            Some(Ty::Fn { params: p, .. }) => p.clone(),
            _ => vec![],
        };
        let mut param_tys = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let ty = match &p.ty {
                Some(t) => ground_type(t, &self.subst.clone(), self.ctx)?,
                None => exp_params.get(i).cloned()
                    .ok_or_else(|| LowerError { msg: format!("cannot infer type of closure param `{}`", p.name), span: p.span })?,
            };
            param_tys.push(ty);
        }

        // Find captured free variables (names referenced in the body that are
        // bound in the current scope and not shadowed by a closure param).
        let mut free = Vec::new();
        let param_names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        collect_free_vars(body, &param_names, &mut Vec::new(), &mut free);
        // Resolve each free name to (LocalId, Ty); dedup, keep only locals.
        let mut captures: Vec<(String, LocalId, Ty)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for name in &free {
            if seen.contains(name) { continue; }
            if let Some((id, ty)) = self.lookup(name) {
                seen.insert(name.clone());
                captures.push((name.clone(), id, ty));
            }
        }

        // Capture reprs + the env layout.
        let mut cap_reprs = Vec::new();
        for (_, _, ty) in &captures {
            cap_reprs.push(self.repr_of(ty, span)?);
        }
        let key = format!("closure@{}.{}", span.start, span.end);
        let env_lid = self.reg.closure_env(&key, &cap_reprs).map_err(|e| LowerError { msg: e.0, span })?;

        // Build the capture value expressions (loads of the enclosing locals).
        let capture_exprs: Vec<CoreExpr> = captures.iter().zip(&cap_reprs)
            .map(|((_, id, _), r)| CoreExpr::new(CoreExprKind::Local(*id), r.clone()))
            .collect();

        // Determine the return type. `None` ⇒ infer it from the body.
        let declared_ret = match ret {
            Some(t) => Some(ground_type(t, &self.subst.clone(), self.ctx)?),
            None => match expected {
                Some(Ty::Fn { ret, .. }) if !ret.is_unit() && !has_var(ret) => Some((**ret).clone()),
                _ => None,
            },
        };

        // Lower the lifted function body in a fresh local/scope context that
        // binds the captures (loaded from env) and the params (from args).
        let code_id = self.mono.fresh_closure_id(self.count);
        // A captured `mut` binding stays mutable inside the closure (a
        // closure that assigns to it shares it through a `MutCell`).
        let cap_mut: Vec<bool> = captures.iter().map(|(name, _, _)| self.lookup_mut(name).unwrap_or(false)).collect();
        let (lifted, ret_ty) = self.lower_lifted(&captures, &cap_mut, &cap_reprs, params, &param_tys, body, declared_ret, env_lid, code_id, span)?;
        self.mono.closures.push((code_id, lifted));

        let fn_ty = Ty::Fn { params: param_tys, ret: Box::new(ret_ty) };
        let sid = self.mono.intern_span(span);
        Ok((
            CoreExpr::new(
                CoreExprKind::MakeClosure { code: code_id, env: env_lid, captures: capture_exprs },
                Repr::Ref(env_lid),
            )
            .at(sid),
            fn_ty,
        ))
    }

    /// Build the lifted `CoreFn` for a closure: params are the env (implicit,
    /// added by codegen) followed by the closure's value params. Captures are
    /// loaded from the env at the top.
    #[allow(clippy::too_many_arguments)]
    fn lower_lifted(
        &mut self,
        captures: &[(String, LocalId, Ty)],
        cap_mut: &[bool],
        cap_reprs: &[Repr],
        params: &[ClosureParam],
        param_tys: &[Ty],
        body: &Expr,
        declared_ret: Option<Ty>,
        env_lid: LayoutId,
        code_id: FuncId,
        span: Span,
    ) -> LResult<(CoreFn, Ty)> {
        // Use an independent lexical context even when lowering fails or an
        // inference probe retries. Unsafe/loop permissions and local slots
        // must never be inherited from, or overwrite, the enclosing function.
        let mut lifted = FnLowerer {
            ctx: self.ctx,
            reg: self.reg,
            mono: self.mono,
            count: self.count,
            subst: self.subst.clone(),
            scope: vec![HashMap::new()],
            locals: vec![],
            local_names: vec![],
            local_tys: vec![],
            ret_ty: declared_ret.clone().unwrap_or_else(Ty::unit),
            infer_ret: declared_ret.is_none(),
            infer: std::mem::take(&mut self.infer),
            unsafe_depth: 0,
            deferred_locals: vec![],
            loop_exits: vec![],
            type_probe: self.type_probe,
            probe_cache: self.probe_cache.clone(),
        };
        let result = lifted.lower_lifted_body(
            captures,
            cap_mut,
            cap_reprs,
            params,
            param_tys,
            body,
            declared_ret,
            env_lid,
            code_id,
            span,
        );
        self.infer = std::mem::take(&mut lifted.infer);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_lifted_body(
        &mut self,
        captures: &[(String, LocalId, Ty)],
        cap_mut: &[bool],
        cap_reprs: &[Repr],
        params: &[ClosureParam],
        param_tys: &[Ty],
        body: &Expr,
        declared_ret: Option<Ty>,
        env_lid: LayoutId,
        code_id: FuncId,
        span: Span,
    ) -> LResult<(CoreFn, Ty)> {
        // The closure literal's span = the lifted function's def location (DWARF).
        let fn_span = self.mono.intern_span(span);
        // Bind captures first as locals; codegen initializes these from the env.
        let mut cap_locals = Vec::new();
        for (((name, _, ty), repr), is_mut) in captures.iter().zip(cap_reprs).zip(cap_mut) {
            let id = self.fresh_local(repr.clone(), ty.clone());
            self.bind_mut(name, id, ty.clone(), *is_mut);
            cap_locals.push(id);
        }
        // Then params.
        let mut param_reprs = Vec::new();
        for (p, ty) in params.iter().zip(param_tys) {
            let repr = self.repr_of(ty, p.span)?;
            let id = self.fresh_local(repr.clone(), ty.clone());
            self.bind(&p.name, id, ty.clone());
            param_reprs.push(repr);
        }

        let (body_block, body_ty) = self.block_for_closure(body, declared_ret.as_ref())?;
        self.check_definite_init(&body_block, body.span)?;
        let ret_ty = match &declared_ret {
            Some(t) => {
                self.check_assignable(&body_ty, t, span)?;
                t.clone()
            }
            None if !self.infer_ret => {
                self.check_assignable(&body_ty, &self.ret_ty.clone(), span)?;
                self.ret_ty.clone()
            }
            None => body_ty,
        };
        self.ret_ty = ret_ty.clone();
        let ret_repr = self.repr_of(&ret_ty, span)?;

        let locals = std::mem::take(&mut self.locals);
        let local_names = std::mem::take(&mut self.local_names);

        let _ = code_id;
        // Compute each capture's ABSOLUTE byte offset within the env (past the
        // 16-byte Full header). Pointer captures occupy the leading pointer
        // slots; scalar captures sit in the raw section after the 8-byte code
        // pointer. This must mirror MakeClosure codegen exactly.
        const HDR: u64 = 16;
        let n_ptr = cap_reprs.iter().filter(|r| matches!(r, Repr::Ref(_))).count() as u64;
        let raw_base = HDR + n_ptr * 8;
        let mut closure_captures = Vec::new();
        let mut ptr_idx = 0u64;
        let mut raw_off = 8u64; // skip the code pointer at raw offset 0
        for (cap_local, repr) in cap_locals.iter().zip(cap_reprs) {
            let offset = match repr {
                Repr::Ref(_) => { let o = HDR + ptr_idx * 8; ptr_idx += 1; o }
                Repr::Scalar(s) => {
                    let sz = (s.bits().max(8) / 8) as u64;
                    raw_off = raw_off.div_ceil(sz) * sz;
                    let o = raw_base + raw_off;
                    raw_off += sz;
                    o
                }
                // An inline value-aggregate capture lives in the raw section like
                // a scalar; advance `raw_off` by its size so a following capture
                // doesn't alias it. (Must mirror MakeClosure codegen.)
                Repr::Value(vid) => {
                    let sz = self.reg.values[*vid as usize].size as u64;
                    raw_off = raw_off.div_ceil(8) * 8; // 8-align value aggregates
                    let o = raw_base + raw_off;
                    raw_off += sz;
                    o
                }
                Repr::Unit => raw_base + raw_off,
            };
            closure_captures.push(ClosureCapture { local: *cap_local, offset });
        }
        Ok((CoreFn {
            name: format!("__closure_{}", env_lid),
            params: param_reprs,
            extern_by_ref: vec![],
            ret: ret_repr,
            locals,
            local_names,
            body: CoreBlock { stmts: body_block.stmts, tail: body_block.tail },
            closure_captures, is_closure: true,
            is_extern: false,
            span: fn_span,
        }, ret_ty))
    }

    /// Lower a closure body (which is an arbitrary expression, possibly a block).
    fn block_for_closure(&mut self, body: &Expr, expected: Option<&Ty>) -> LResult<(CoreBlock, Ty)> {
        match &*body.kind {
            ExprKind::Block(b) => self.block_expected(b, expected),
            _ => {
                let (ce, t) = self.expr(body, expected)?;
                Ok((CoreBlock { stmts: vec![], tail: Some(ce) }, t))
            }
        }
    }

    /// Resolve and lower `recv.method(args)` to a concrete monomorphic call,
    /// with `recv` passed as the leading `self` argument.
    /// `format!` placeholders with a spec expand to `fmt#...` pseudo-methods
    /// (no source method can contain `#`). Each picks the rendering from the
    /// receiver's static type, then lowers ordinary source-level expressions.
    fn format_pseudo_method(&mut self, recv: &Expr, method: &str, args: &[Expr], span: Span) -> LResult<(CoreExpr, Ty)> {
        let (_, ty) = self.expr(recv, None)?;
        let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
        let call = |f: &str, args: Vec<Expr>| ex(ExprKind::Call(ex(ExprKind::Path(Path::single(f.to_string(), span))), args));
        let method_on = |r: Expr, m: &str, args: Vec<Expr>| ex(ExprKind::MethodCall { recv: r, method: m.into(), args, span });
        let int = |n: i64| ex(ExprKind::Int(n as u64, crate::lexer::NumSuffix::None));
        let cast = |e: Expr, p: &str| ex(ExprKind::Cast(e, Type { kind: TypeKind::Path(Path::single(p.to_string(), span), vec![]), span }));
        let is_float = matches!(ty, Ty::Prim(Prim::F32 | Prim::F64));
        let int_bits = match ty {
            Ty::Prim(Prim::I8 | Prim::U8) => Some(8),
            Ty::Prim(Prim::I16 | Prim::U16) => Some(16),
            Ty::Prim(Prim::I32 | Prim::U32) => Some(32),
            Ty::Prim(Prim::I64 | Prim::U64) => Some(64),
            _ => None,
        };
        let expr = match method {
            "fmt#numeric" => ex(ExprKind::Bool(is_float || int_bits.is_some())),
            "fmt#prec" if is_float => call("fmt_f64", vec![cast(recv.clone(), "f64"), args[0].clone(), int(0)]),
            "fmt#prec" if ty == Ty::Prim(Prim::Str) => method_on(recv.clone(), "fmt_truncate", vec![args[0].clone()]),
            "fmt#prec" => method_on(recv.clone(), "to_string", vec![]),
            "fmt#exp" if is_float || int_bits.is_some() => {
                let ExprKind::Bool(upper) = &*args[1].kind else { unreachable!() };
                call("fmt_f64", vec![cast(recv.clone(), "f64"), args[0].clone(), int(if *upper { 3 } else { 2 })])
            }
            "fmt#exp" => return err(format!("`{{:e}}` needs a number, not `{}`", ty_display(&ty)), span),
            "fmt#radix" => {
                let Some(bits) = int_bits else {
                    return err(format!("`{{:x}}`, `{{:o}}`, and `{{:b}}` need an integer, not `{}`", ty_display(&ty)), span);
                };
                call("fmt_radix", vec![cast(recv.clone(), "i64"), args[0].clone(), args[1].clone(), int(bits)])
            }
            "fmt#debug" | "fmt#debug_pretty" => {
                let pretty = method == "fmt#debug_pretty";
                match &ty {
                    _ if is_float => call("fmt_f64", vec![cast(recv.clone(), "f64"), args[0].clone(), int(1)]),
                    Ty::Prim(Prim::Unit) => ex(ExprKind::Str("()".into())),
                    Ty::Tuple(elems) => {
                        let parts = (0..elems.len()).map(|i| {
                            let field = ex(ExprKind::Field { base: recv.clone(), field: FieldAccess::Tuple(i as u32) });
                            method_on(field, method, vec![args[0].clone()])
                        }).collect();
                        let items = ex(ExprKind::Array(ArrayLit::Elems(parts)));
                        call("fmt_debug_seq", vec![ex(ExprKind::Str("(".into())), ex(ExprKind::Str(")".into())), items, ex(ExprKind::Bool(pretty)), ex(ExprKind::Bool(elems.len() == 1))])
                    }
                    Ty::Array(..) | Ty::Named { .. } if array_elem_ty(&ty).is_some() => call("fmt_debug_array", vec![recv.clone(), ex(ExprKind::Bool(pretty))]),
                    _ => method_on(recv.clone(), if pretty { "to_debug_string_pretty" } else { "to_debug_string" }, vec![]),
                }
            }
            _ => return err(format!("unknown format operation `{}`", method), span),
        };
        self.expr(&expr, None)
    }

    fn method_call(&mut self, recv: &Expr, method: &str, args: &[Expr], span: Span) -> LResult<(CoreExpr, Ty)> {
        if method.starts_with("fmt#") {
            return self.format_pseudo_method(recv, method, args, span);
        }
        // The receiver may contain calls of its own: hold this call's type
        // arguments until its operands are lowered.
        let turbofish = METHOD_TURBOFISH.with(|t| t.borrow_mut().take());
        let result = self.method_call_inner(recv, method, args, span, turbofish);
        result
    }

    fn method_call_inner(&mut self, recv: &Expr, method: &str, args: &[Expr], span: Span, turbofish: Option<Vec<Ty>>) -> LResult<(CoreExpr, Ty)> {

        self.infer.unresolved = None;
        let (crecv, recv_ty) = match self.expr(recv, None) {
            Ok(v) => v,
            Err(error) => {
                // A receiver whose type parameters are open (`None.unwrap_or(7)`):
                // take them from the arguments, then lower it with that type.
                let Some(partial) = self.infer.unresolved.take().filter(|_| error.span == recv.span) else { return Err(error) };
                let Some(expected) = self.receiver_type_from_args(&partial, method, args)? else { return Err(error) };
                self.expr(recv, Some(&expected))?
            }
        };
        // Tuples, arrays and `()` have no impls; their Debug output is built
        // structurally.
        if matches!(method, "to_debug_string" | "to_debug_string_pretty") && args.is_empty()
            && (matches!(recv_ty, Ty::Tuple(_) | Ty::Prim(Prim::Unit)) || array_elem_ty(&recv_ty).is_some())
        {
            let pseudo = if method == "to_debug_string" { "fmt#debug" } else { "fmt#debug_pretty" };
            let none = Expr { kind: std::sync::Arc::new(ExprKind::Unary(UnOp::Neg, Expr { kind: std::sync::Arc::new(ExprKind::Int(1, crate::lexer::NumSuffix::None)), span })), span };
            return self.format_pseudo_method(recv, pseudo, &[none], span);
        }
        // A user trait implemented for an array or tuple type comes before the
        // built-in array methods and structural tuple operations.
        let user_impl = self.lookup_method(&recv_ty, method).is_some();
        if let Some(elem_ty) = array_elem_ty(&recv_ty).filter(|_| !user_impl) {
            return self.array_method(crecv, elem_ty, method, args, span);
        }
        if !user_impl && matches!(recv_ty, Ty::Tuple(_)) && matches!(method, "eq" | "ne" | "cmp" | "hash" | "clone" | "lt" | "le" | "gt" | "ge") {
            return self.tuple_method(crecv, recv_ty, method, args, span);
        }
        if dyn_trait(&recv_ty).is_some() && self.lookup_method(&recv_ty, method).is_none() {
            return self.dyn_method_call(crecv, recv_ty, method, args, span);
        }
        let entry = self.lookup_method(&recv_ty, method)
            .ok_or_else(|| LowerError { msg: format!("no method `{}` on `{}`", method, ty_display(&recv_ty)), span })?;
        if let Some(base) = crate::types::receiver_base(&recv_ty) {
            if self.ctx.ambiguous_methods.contains(&(base, method.to_string())) {
                return err(format!("multiple traits define `{}` for `{}`; call it as `Trait::{}(x, ..)`", method, ty_display(&recv_ty), method), span);
            }
        }
        let entry = self.pick_overload(&recv_ty, entry, args)?;
        OPERAND_TURBOFISH.with(|t| *t.borrow_mut() = turbofish);
        self.method_call_lowered(crecv, recv_ty, Some(recv), entry, args, span)
    }

    /// `Array<T>` is built in: its methods lower directly to the array
    /// intrinsics, so the array analyses see `a.len()` exactly as they see
    /// `a[i]`.
    fn array_method(&mut self, ca: CoreExpr, elem_ty: Ty, method: &str, args: &[Expr], span: Span) -> LResult<(CoreExpr, Ty)> {
        let arity = |n: usize| -> LResult<()> {
            if args.len() == n { Ok(()) } else { err(format!("`Array::{}` expects {} args, got {}", method, n, args.len()), span) }
        };
        match method {
            "len" => {
                arity(0)?;
                Ok((CoreExpr::new(CoreExprKind::ArrayLen(Box::new(ca)), Repr::Scalar(ScalarRepr::I64)), Ty::i64()))
            }
            "is_empty" => {
                arity(0)?;
                let len = CoreExpr::new(CoreExprKind::ArrayLen(Box::new(ca)), Repr::Scalar(ScalarRepr::I64));
                let zero = CoreExpr::new(CoreExprKind::ConstInt(0, ScalarRepr::I64), Repr::Scalar(ScalarRepr::I64));
                Ok((CoreExpr::new(CoreExprKind::Bin(BinOp::Eq, Box::new(len), Box::new(zero)), Repr::Scalar(ScalarRepr::Bool)), Ty::bool()))
            }
            "get" => {
                arity(1)?;
                let elem = self.repr_of(&elem_ty, span)?;
                let (ci, it) = self.expr(&args[0], Some(&Ty::i64()))?;
                self.check_assignable(&it, &Ty::i64(), args[0].span)?;
                let opt_ty = Ty::Named { name: "Option".to_string(), args: vec![elem_ty] };
                let opt_repr = self.repr_of(&opt_ty, span)?;
                Ok((CoreExpr::new(CoreExprKind::ArrayGet { array: Box::new(ca), index: Box::new(ci), elem }, opt_repr), opt_ty))
            }
            _ => err(format!("no method `{}` on `Array<{}>`", method, ty_display(&elem_ty)), span),
        }
    }

    /// Complete a receiver type whose type parameters are open by typing the
    /// method's arguments. The arguments are lowered only to learn their
    /// types; that lowering is rolled back (the call lowers them again).
    fn receiver_type_from_args(&mut self, partial: &Ty, method: &str, args: &[Expr]) -> LResult<Option<Ty>> {
        let Some(entry) = self.lookup_method(partial, method) else { return Ok(None) };
        if entry.method.params.len() != args.len() {
            return Ok(None);
        }
        let impl_self = lower_type(&entry.self_ty, &entry.impl_generics, self.ctx).map_err(conv)?;
        let mut scope_params = entry.impl_generics.clone();
        scope_params.extend(entry.method.generics.params.iter().map(|p| p.name.clone()));
        scope_params.push("Self".to_string());
        let mut method_binders: HashMap<String, Ty> = entry.method.generics.params.iter().enumerate()
            .map(|(index, parameter)| (parameter.name.clone(), Ty::Var(format!("$receiver_method{index}")))).collect();
        method_binders.insert("Self".into(), impl_self.clone());
        let mut subst = HashMap::new();
        infer_concrete_parts(&impl_self, partial, &mut subst);
        for (a, p) in args.iter().zip(&entry.method.params) {
            let declared = lower_type(&p.ty, &scope_params, self.ctx).map_err(conv)?;
            let declared = apply_subst(&declared, &method_binders);
            if !has_var(&declared) {
                continue;
            }
            if let Ok(actual) = self.probe_expr_type(a, None) {
                if !has_var(&actual) && !is_never(&actual) {
                    unify_infer(&declared, &actual, &mut subst);
                }
            }
        }
        let receiver = apply_subst(&impl_self, &subst);
        Ok(if has_var(&receiver) { None } else { Some(receiver) })
    }

    /// The impl method `method` for a receiver of type `recv_ty`, if any.
    fn lookup_method(&self, recv_ty: &Ty, method: &str) -> Option<crate::types::MethodEntry> {
        if let Some(base) = crate::types::receiver_base(recv_ty) {
            if let Some(e) = self.ctx.methods.get(&(base, method.to_string())) { return Some(e.clone()); }
        }
        self.blanket_method(recv_ty, None, method)
    }

    /// A method from a blanket impl (`impl<T: Display> Shout for T`) whose
    /// bounds `recv_ty` meets, optionally of a given trait.
    fn blanket_method(&self, recv_ty: &Ty, trait_name: Option<&str>, method: &str) -> Option<crate::types::MethodEntry> {
        let mut found: Vec<&crate::types::MethodEntry> = self.ctx.trait_candidates.iter()
            .filter(|((base, tr, m), _)| base == "*" && m == method && trait_name.is_none_or(|t| t == tr))
            .flat_map(|(_, entries)| entries.iter()).collect();
        found.sort_by(|a, b| a.trait_name.cmp(&b.trait_name));
        found.into_iter().find(|e| {
            let tr = e.trait_name.as_deref().unwrap_or_default();
            // Trait arguments that are not the receiver parameter itself
            // would need inference; such impls apply when the trait holds.
            let receiver = match &e.self_ty.kind { TypeKind::Path(p, _) => p.last().to_string(), _ => return false };
            let at_receiver: HashMap<String, Ty> = [(receiver, recv_ty.clone())].into_iter().collect();
            let args: Option<Vec<Ty>> = e.trait_args.iter().map(|a| {
                let t = lower_type(a, &e.impl_generics, self.ctx).ok()?;
                let t = apply_subst(&t, &at_receiver);
                (!has_var(&t)).then_some(t)
            }).collect();
            match args {
                Some(args) => self.ctx.type_implements_trait_args(recv_ty, tr, &args),
                None => false,
            }
        }).cloned()
    }

    /// Call the method `method` of `impl trait_name for <recv_ty>` on an
    /// already-lowered receiver. Operators and indexing desugar through this,
    /// so the method must come from that trait's impl, not an inherent impl.
    fn trait_method_call(
        &mut self,
        crecv: CoreExpr,
        recv_ty: Ty,
        recv: Option<&Expr>,
        trait_name: &str,
        method: &str,
        what: &str,
        args: &[Expr],
        span: Span,
    ) -> LResult<(CoreExpr, Ty)> {
        let entry = self.required_trait_method(&recv_ty, trait_name, method, what, span)?;
        let entry = self.pick_overload(&recv_ty, entry, args)?;
        self.method_call_lowered(crecv, recv_ty, recv, entry, args, span)
    }

    /// `Self::X` in `entry`'s signature is the associated type its own impl
    /// defines (a type with several impls of one trait has several).
    fn entry_projections(&self, ty: Ty, entry: &crate::types::MethodEntry, scope: &[String]) -> LResult<Ty> {
        if entry.assoc_types.is_empty() { return Ok(ty); }
        Ok(match ty {
            Ty::Named { name, args } => {
                if let (Some(assoc), Some(Ty::Var(v))) = (name.strip_prefix("proj#"), args.first()) {
                    if v == "Self" {
                        if let Some((_, t)) = entry.assoc_types.iter().find(|(n, _)| n == assoc) {
                            return lower_type(t, scope, self.ctx).map_err(conv);
                        }
                    }
                }
                Ty::Named { name, args: args.into_iter().map(|a| self.entry_projections(a, entry, scope)).collect::<LResult<_>>()? }
            }
            Ty::Tuple(es) => Ty::Tuple(es.into_iter().map(|a| self.entry_projections(a, entry, scope)).collect::<LResult<_>>()?),
            Ty::Array(e, n) => Ty::Array(Box::new(self.entry_projections(*e, entry, scope)?), n),
            Ty::Fn { params, ret } => Ty::Fn {
                params: params.into_iter().map(|a| self.entry_projections(a, entry, scope)).collect::<LResult<_>>()?,
                ret: Box::new(self.entry_projections(*ret, entry, scope)?),
            },
            other => other,
        })
    }

    /// Choose among a type's impls of one generic trait (`Add<f64>` and
    /// `Add<V>` for `V`) by the arguments' types. With one impl, `primary`.
    fn pick_overload(&mut self, recv_ty: &Ty, primary: crate::types::MethodEntry, args: &[Expr]) -> LResult<crate::types::MethodEntry> {
        let Some(base) = crate::types::receiver_base(recv_ty) else { return Ok(primary) };
        self.pick_among(&base, Some(recv_ty), primary, args)
    }

    /// `pick_overload` for an associated function (no receiver value):
    /// `M::from(5)` with `impl From<i64> for M` and `impl From<bool> for M`.
    fn pick_among(&mut self, base: &str, recv_ty: Option<&Ty>, primary: crate::types::MethodEntry, args: &[Expr]) -> LResult<crate::types::MethodEntry> {
        let Some(tr) = primary.trait_name.clone() else { return Ok(primary) };
        let candidates = match self.ctx.trait_candidates.get(&(base.to_string(), tr, primary.method.name.clone())) {
            Some(c) if c.len() > 1 => c.clone(),
            _ => return Ok(primary),
        };
        let mut arg_tys = Vec::new();
        for a in args { arg_tys.push(self.probe_expr_type(a, None).ok()); }
        for entry in &candidates {
            if entry.method.params.len() != args.len() { continue; }
            let impl_self = lower_type(&entry.self_ty, &entry.impl_generics, self.ctx).map_err(conv)?;
            let mut isubst = HashMap::new();
            if let Some(recv_ty) = recv_ty {
                unify_infer(&impl_self, recv_ty, &mut isubst);
                if apply_subst(&impl_self, &isubst) != *recv_ty { continue; }
                isubst.insert("Self".to_string(), recv_ty.clone());
            } else {
                isubst.insert("Self".to_string(), impl_self.clone());
            }
            let mut scope = entry.impl_generics.clone();
            scope.extend(entry.method.generics.params.iter().map(|p| p.name.clone()));
            scope.push("Self".into());
            let mut fits = true;
            for (p, actual) in entry.method.params.iter().zip(&arg_tys) {
                let declared = lower_type(&p.ty, &scope, self.ctx).map_err(conv)?;
                let declared = apply_subst(&self.entry_projections(declared, entry, &scope)?, &isubst);
                match actual {
                    Some(a) if has_var(&declared) || *a == declared || is_never(a) => {}
                    // An unsuffixed literal takes the declared numeric type.
                    Some(Ty::Prim(Prim::I64)) if matches!(declared, Ty::Prim(p) if is_int_prim(p)) => {}
                    Some(Ty::Prim(Prim::F64)) if matches!(declared, Ty::Prim(Prim::F32)) => {}
                    _ => { fits = false; break; }
                }
            }
            if fits { return Ok(entry.clone()); }
        }
        Ok(primary)
    }

    /// The symbol part naming `entry`'s method: its bare name, or
    /// `Trait::name` when another method of that name on the same receiver
    /// would otherwise share the symbol.
    fn method_symbol(&self, recv_ty: &Ty, entry: &crate::types::MethodEntry) -> String {
        let name = &entry.method.name;
        let Some(tr) = &entry.trait_name else { return name.clone() };
        let primary = crate::types::receiver_base(recv_ty)
            .and_then(|base| self.ctx.methods.get(&(base, name.clone())));
        match primary {
            Some(p) if p.trait_name.as_ref() != Some(tr) => format!("{}::{}", tr, name),
            _ => name.clone(),
        }
    }

    /// The method `method` of `impl trait_name for <recv_ty>`, if any.
    fn lookup_trait_method(&self, recv_ty: &Ty, trait_name: &str, method: &str) -> Option<crate::types::MethodEntry> {
        if let Some(base) = crate::types::receiver_base(recv_ty) {
            if let Some(e) = self.ctx.trait_methods.get(&(base, trait_name.to_string(), method.to_string())) { return Some(e.clone()); }
        }
        self.blanket_method(recv_ty, Some(trait_name), method)
    }

    fn required_trait_method(&self, recv_ty: &Ty, trait_name: &str, method: &str, what: &str, span: Span) -> LResult<crate::types::MethodEntry> {
        self.lookup_trait_method(recv_ty, trait_name, method)
            .ok_or_else(|| LowerError {
                msg: format!("{} is not defined for `{}` (it needs `impl {} for {}`)", what, ty_display(recv_ty), trait_name, ty_display(recv_ty)),
                span,
            })
    }

    /// Lower a call to the impl method `entry` with an already-lowered
    /// receiver. `recv` is the receiver's source expression, used to check
    /// that a `mut self` receiver is rooted in a `mut` binding; `None` means
    /// the receiver is a temporary.
    fn method_call_lowered(
        &mut self,
        crecv: CoreExpr,
        recv_ty: Ty,
        recv: Option<&Expr>,
        entry: crate::types::MethodEntry,
        args: &[Expr],
        span: Span,
    ) -> LResult<(CoreExpr, Ty)> {
        let args: Vec<_> = args.iter().map(MethodArg::Source).collect();
        self.method_call_operands(crecv, recv_ty, recv, entry, &args, span)
    }

    fn method_call_operands(
        &mut self, crecv: CoreExpr, recv_ty: Ty, recv: Option<&Expr>,
        entry: crate::types::MethodEntry, args: &[MethodArg<'_>], span: Span,
    ) -> LResult<(CoreExpr, Ty)> {
        let method = entry.method.name.clone();
        let method = method.as_str();
        // Transitive immutability: calling a `mut self` method requires the
        // receiver to be rooted in a `mut` binding (see docs/mutability.md).
        if entry.method.self_is_mut {
            match recv {
                Some(recv) => self.check_mutable_root(recv, "the receiver of a `mut self` method through", span)?,
                None => return err(format!("cannot call `mut self` method `{}` on a temporary value", method), span),
            }
        }

        // Infer the impl's generic params + Self by unifying the impl's written
        // self type against the receiver's concrete type.
        let impl_self = lower_type(&entry.self_ty, &entry.impl_generics, self.ctx).map_err(conv)?;
        let mut isubst: HashMap<String, Ty> = HashMap::new();
        unify_infer(&impl_self, &recv_ty, &mut isubst);
        self.check_assignable(&recv_ty, &apply_subst(&impl_self, &isubst), span)?;
        // `Self` resolves to the concrete receiver type.
        isubst.insert("Self".to_string(), recv_ty.clone());

        let turbofish = OPERAND_TURBOFISH.with(|t| t.borrow_mut().take());
        let m = entry.method.clone();
        if m.is_unsafe {
            self.require_unsafe(&format!("calling `unsafe fn {}`", m.name), span)?;
        }
        // Method-level generics inferred from args.
        let mgparams: Vec<String> = m.generics.params.iter().map(|p| p.name.clone()).collect();
        if args.len() != m.params.len() {
            return err(format!("method `{}` expects {} args, got {}", method, m.params.len(), args.len()), span);
        }
        // Impl and method binders have separate scopes. A method binder
        // shadows a same-spelled impl binder in its signature and body, while
        // the impl's applicability conditions still refer to the receiver.
        let mut signature_subst = isubst.clone();
        for parameter in &mgparams { signature_subst.remove(parameter); }
        let mut method_subst = HashMap::new();
        // `x.m::<T>(..)`: explicit method type arguments.
        if let Some(tys) = &turbofish {
            for (g, t) in mgparams.iter().zip(tys) {
                if !matches!(t, Ty::Var(v) if v == "_") { method_subst.insert(g.clone(), t.clone()); }
            }
        }
        let mut scope_params = entry.impl_generics.clone();
        scope_params.extend(mgparams.clone());
        scope_params.push("Self".into());
        let mut cargs = vec![crecv];
        let mut checks = Vec::new();
        for (a, p) in args.iter().zip(&m.params) {
            let declared = lower_type(&p.ty, &scope_params, self.ctx).map_err(conv)?;
            let declared = self.entry_projections(declared, &entry, &scope_params)?;
            let declared = apply_subst(&declared, &signature_subst);
            let pre = apply_subst(&declared, &method_subst);
            let (ca, at, arg_span) = match a {
                MethodArg::Source(a) => {
                    let (ca, at) = self.expr(a, hint(&pre))?;
                    (ca, at, a.span)
                }
                MethodArg::Lowered(ca, at, span) => (ca.clone(), at.clone(), *span),
            };
            unify_infer(&declared, &at, &mut method_subst);
            checks.push((declared, at, arg_span));
            cargs.push(ca);
        }
        {
            let mut known = isubst.clone();
            known.extend(method_subst.clone());
            self.infer_from_bindings(&m.generics, &scope_params, &mut known)?;
            for name in &mgparams {
                if let Some(t) = known.get(name) { method_subst.entry(name.clone()).or_insert_with(|| t.clone()); }
            }
        }
        for (declared, actual, arg_span) in &checks {
            self.check_assignable(actual, &apply_subst(declared, &method_subst), *arg_span)?;
        }
        self.check_generic_bounds(&entry.impl_constraints, &entry.impl_generics, &isubst, method, span)?;
        let mut subst = isubst;
        subst.extend(method_subst);
        self.check_generic_bounds(&m.generics, &scope_params, &subst, method, span)?;

        // Mangle: <Self-type>::method$<args>.
        let self_mangle = ty_mangle(&recv_ty);
        let extra: Vec<Ty> = mgparams.iter().filter_map(|g| subst.get(g).cloned()).collect();
        let mangled = mangle(&format!("{}.{}", self_mangle, self.method_symbol(&recv_ty, &entry)), &extra);

        let ret_ty = match &m.ret {
            Some(t) => {
                let mut scope_params = entry.impl_generics.clone();
                scope_params.extend(mgparams.clone());
                scope_params.push("Self".to_string());
                let raw = lower_type(t, &scope_params, self.ctx).map_err(conv)?;
                let raw = self.entry_projections(raw, &entry, &scope_params)?;
                apply_subst(&raw, &subst)
            }
            None => Ty::unit(),
        };

        let job = Job { f: m, subst, mangled, self_ty: Some(recv_ty) };
        let fid = self.mono.intern(job.clone(), self.count);
        let ret_ty = self.opaque_result(&job, ret_ty, span)?;
        let repr = self.repr_of(&ret_ty, span)?;
        Ok((CoreExpr::new(CoreExprKind::Call(fid, cargs), repr), ret_ty))
    }

    /// Lower an associated (self-less) function call `Type::func(args)`. Like a
    /// method call but with no receiver: impl-level and method-level generics
    /// (and `Self`) are inferred from the expected return type and the arguments.
    fn assoc_call(
        &mut self,
        base: &str,
        alias_target: Option<&Ty>,
        alias_name: Option<&str>,
        entry: &crate::types::MethodEntry,
        args: &[Expr],
        expected: Option<&Ty>,
        span: Span,
    ) -> LResult<(CoreExpr, Ty)> {
        let m = entry.method.clone();
        if m.is_unsafe {
            self.require_unsafe(&format!("calling `unsafe fn {}`", m.name), span)?;
        }
        if args.len() != m.params.len() {
            return err(
                format!("`{}::{}` expects {} args, got {}", base, m.name, m.params.len(), args.len()),
                span,
            );
        }
        // SEND/SYNC: `Thread::spawn(closure)` shares the closure's captures with
        // the parent thread, so each must be `Sync`. We check at THIS call site
        // (the closure literal is the argument here; by the time the prelude
        // wrapper forwards it to the `thread_spawn` intrinsic it's just a param).
        // See `docs/threads.md`.
        if base == "Thread" && m.name == "spawn" && !args.is_empty() {
            self.check_spawn_captures_sync(&args[0])?;
        }
        let mgparams: Vec<String> = m.generics.params.iter().map(|p| p.name.clone()).collect();
        // All generic names in scope for the signature: impl generics + method
        // generics + Self.
        let mut scope_params = entry.impl_generics.clone();
        scope_params.extend(mgparams.clone());
        scope_params.push("Self".to_string());

        // Give receiver binders private semantic identities inside this
        // inference transaction. Method binders keep their own lexical scope.
        let impl_binders: HashMap<String, Ty> = entry.impl_generics.iter().enumerate()
            .map(|(index, name)| (name.clone(), Ty::Var(format!("$associated_impl{index}")))).collect();
        let impl_self = lower_type(&entry.self_ty, &entry.impl_generics, self.ctx).map_err(conv)?;
        let impl_self = apply_subst(&impl_self, &impl_binders);
        let mut signature_binders = impl_binders.clone();
        for name in &mgparams { signature_binders.remove(name); }
        signature_binders.insert("Self".into(), impl_self.clone());
        let normalize = |ty: &Type| -> LResult<Ty> {
            lower_type(ty, &scope_params, self.ctx).map(|ty| apply_subst(&ty, &signature_binders)).map_err(conv)
        };
        let declared_params = m.params.iter().map(|p| normalize(&p.ty)).collect::<LResult<Vec<_>>>()?;
        let declared_return = m.ret.as_ref().map(normalize).transpose()?.unwrap_or_else(Ty::unit);
        let mut inferred = HashMap::new();
        if let Some(target) = alias_target {
            infer_concrete_parts(&impl_self, target, &mut inferred);
        }
        if let Some(expected) = expected {
            let declared = apply_subst(&declared_return, &inferred);
            unify_infer(&declared, expected, &mut inferred);
        }
        let mut cargs = Vec::new();
        let mut checks = Vec::new();
        for (arg, declared) in args.iter().zip(&declared_params) {
            let expected = apply_subst(declared, &inferred);
            let (value, actual) = self.expr(arg, compound_hint(&expected))?;
            unify_infer(declared, &actual, &mut inferred);
            checks.push((declared, actual, arg.span));
            cargs.push(value);
        }
        // `fn from_iter<I: Iterator<Item = T>>(iter: I)`: `T` is `I`'s `Item`.
        for p in &m.generics.params {
            let Some(t) = inferred.get(&p.name).cloned().filter(|t| !has_var(t)) else { continue };
            for b in &p.bounds {
                for (assoc, written) in &b.bindings {
                    let declared = normalize(written)?;
                    let args: Vec<Ty> = b.args.iter().filter_map(|a| normalize(a).ok()).map(|t| apply_subst(&t, &inferred)).collect();
                    if let Some(actual) = self.ctx.associated_type_of(&t, &b.path.segments.join("::"), &args, assoc) {
                        unify_infer(&declared, &actual, &mut inferred);
                    }
                }
            }
        }
        for (declared, actual, arg_span) in &checks {
            self.check_assignable(actual, &apply_subst(declared, &inferred), *arg_span)?;
        }
        // Keep an open result shape available to let/receiver inference. A
        // later use can specialize it; no job is interned until every receiver
        // and method binder is ground and its constraints have been checked.
        let ret_ty = apply_subst(&declared_return, &inferred);
        let mut impl_subst = HashMap::new();
        for name in &entry.impl_generics {
            let actual = apply_subst(&impl_binders[name], &inferred);
            if has_var(&actual) {
                if has_var(&ret_ty) { self.infer.unresolved = Some(ret_ty.clone()); }
                return err(format!("cannot infer impl type parameter `{name}` for `{}`", m.name), span);
            }
            impl_subst.insert(name.clone(), actual);
        }
        let self_concrete = apply_subst(&impl_self, &inferred);
        impl_subst.insert("Self".into(), self_concrete.clone());
        self.check_generic_bounds(&entry.impl_constraints, &entry.impl_generics, &impl_subst, &m.name, span)?;
        let mut subst = impl_subst.clone();
        for name in &mgparams {
            let Some(actual) = inferred.get(name).filter(|ty| !has_var(ty)) else {
                if has_var(&ret_ty) { self.infer.unresolved = Some(ret_ty.clone()); }
                return err(format!("cannot infer type parameter `{name}` for `{}`", m.name), span);
            };
            subst.insert(name.clone(), actual.clone());
        }
        self.check_generic_bounds(&m.generics, &scope_params, &subst, &m.name, span)?;
        if let Some(target) = alias_target {
            let mut alias_args = HashMap::new();
            unify_infer(target, &self_concrete, &mut alias_args);
            self.check_assignable(&self_concrete, &apply_subst(target, &alias_args), span)?;
        }
        if let Some(alias) = alias_name { self.check_alias_bounds(alias, &self_concrete, span)?; }
        let extra: Vec<Ty> = entry.impl_generics.iter().map(|name| impl_subst[name].clone())
            .chain(mgparams.iter().map(|name| subst[name].clone())).collect();
        let mangled = mangle(&format!("{}.{}", ty_mangle(&self_concrete), self.method_symbol(&self_concrete, entry)), &extra);

        let job = Job { f: m, subst, mangled, self_ty: None };
        let fid = self.mono.intern(job.clone(), self.count);
        let ret_ty = self.opaque_result(&job, ret_ty, span)?;
        let repr = self.repr_of(&ret_ty, span)?;
        Ok((CoreExpr::new(CoreExprKind::Call(fid, cargs), repr), ret_ty))
    }

    /// The `?` operator. `inner` evaluates to a `Result<T,E>` or `Option<T>`;
    /// `Ok(v)`/`Some(v)` yields `v`, `Err(e)`/`None` early-returns the value
    /// re-wrapped in the enclosing function's return type.
    fn try_op(&mut self, inner: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        let (cinner, ity) = self.expr(inner, None)?;
        let Ty::Named { name, args } = &ity else {
            return err("`?` operand must be a Result or Option", span);
        };
        let enum_name = name.clone();
        let kind = enum_name.rsplit("::").next().unwrap();
        let (ok_name, err_name) = match kind {
            "Result" => ("Ok", "Err"),
            "Option" => ("Some", "None"),
            _ => return err("`?` operand must be a Result or Option", span),
        };
        let tag_of = |this: &Self, variant: &str| -> LResult<u32> {
            this.ctx.enums.get(&enum_name)
                .and_then(|e| e.variants.iter().position(|v| v.name == variant))
                .map(|i| i as u32)
                .ok_or_else(|| LowerError { msg: format!("`{}` has no variant `{}`", kind, variant), span })
        };
        let ok_tag = tag_of(self, ok_name)?;
        let err_tag = tag_of(self, err_name)?;
        // Determine ok-payload type from the scrutinee's args.
        let variants = self.enum_variants(&enum_name, args)?;
        let ok_payload = variants.iter().find(|(t, _)| *t == ok_tag).map(|(_, p)| p.clone()).unwrap_or_default();
        let ok_ty = ok_payload.first().cloned().unwrap_or_else(Ty::unit);
        let ok_repr = self.repr_of(&ok_ty, span)?;

        // The enclosing fn's return type must be the same enum (so we can build
        // the Err/None to early-return). Re-wrap the err payload as the fn ret.
        let ret_ty = self.ret_ty.clone();
        let Ty::Named { name: rname, args: rargs } = &ret_ty else {
            return err("`?` requires the function to return a Result/Option", span);
        };
        if rname.rsplit("::").next() != Some(kind) {
            return err("`?` return-type mismatch with the function", span);
        }
        let ret_repr = self.repr_of(&ret_ty, span)?;

        // Ok arm: bind payload to a local; body = that local.
        let ok_local = self.fresh_local(ok_repr.clone(), ok_ty.clone());
        let ok_body = CoreExpr::new(CoreExprKind::Local(ok_local), ok_repr.clone());

        // Err arm: bind the err payload, rebuild Err(e)/None for the fn ret, return.
        let err_payload = variants.iter().find(|(t, _)| *t == err_tag).map(|(_, p)| p.clone()).unwrap_or_default();
        let (err_binds, err_fields) = if err_payload.is_empty() {
            (vec![], vec![]) // None
        } else {
            let ety = err_payload[0].clone();
            let erepr = self.repr_of(&ety, span)?;
            let elocal = self.fresh_local(erepr.clone(), ety.clone());
            // A different error type converts through `From` (as Rust's `?`).
            let field = match rargs.get(1) {
                Some(want) if kind == "Result" && *want != ety && !has_var(want) => {
                    self.push_scope();
                    self.bind("try#err", elocal, ety.clone());
                    let ex = |k: ExprKind| Expr { kind: std::sync::Arc::new(k), span };
                    let call = ex(ExprKind::Call(
                        ex(ExprKind::Path(Path { segments: vec!["From".into(), "from".into()], span })),
                        vec![ex(ExprKind::Path(Path::single("try#err".into(), span)))],
                    ));
                    let converted = self.expr(&call, Some(want));
                    self.pop_scope();
                    let (value, vt) = converted.map_err(|e| LowerError {
                        msg: format!("`?` cannot convert the error `{}` into `{}` (it needs `impl From<{}> for {}`): {}", ty_display(&ety), ty_display(want), ty_display(&ety), ty_display(want), e.msg),
                        span,
                    })?;
                    self.check_assignable(&vt, want, span)?;
                    value
                }
                _ => CoreExpr::new(CoreExprKind::Local(elocal), erepr),
            };
            (vec![elocal], vec![field])
        };
        // Build the return value: the fn's return enum, err_tag. For a
        // reference enum this is a heap alloc — span it with the `?` location so
        // `?`-induced allocations carry a file:line:col (best available for desugar).
        let _ = rargs;
        let sid = self.mono.intern_span(span);
        let rebuilt = match ret_repr {
            Repr::Ref(ret_lid) => CoreExpr::new(
                CoreExprKind::MakeVariant { layout: ret_lid, tag: err_tag, fields: err_fields },
                Repr::Ref(ret_lid),
            )
            .at(sid),
            Repr::Value(ret_vid) => CoreExpr::new(
                CoreExprKind::MakeValueVariant { value: ret_vid, tag: err_tag, fields: err_fields },
                Repr::Value(ret_vid),
            ),
            _ => return err("`?` requires the function to return a Result/Option", span),
        };
        let err_body = CoreExpr::new(CoreExprKind::Return(Some(Box::new(rebuilt))), Repr::Unit);

        let arms = vec![
            CoreArm { tag: ok_tag, binds: vec![ok_local], body: ok_body },
            CoreArm { tag: err_tag, binds: err_binds, body: err_body },
        ];
        // Value enums match on the inline aggregate's tag; reference enums on
        // the heap object's tag word.
        let node = match &cinner.repr {
            Repr::Value(_) => CoreExprKind::ValueMatch { scrutinee: Box::new(cinner), arms },
            _ => CoreExprKind::Match { scrutinee: Box::new(cinner), arms },
        };
        Ok((CoreExpr::new(node, ok_repr), ok_ty))
    }

    /// All (tag, payload-types) of an enum, with the enum's concrete type args
    /// substituted in (so a `match` on `Option<i64>` sees `Some(i64)`).
    fn enum_variants(&mut self, enum_name: &str, enum_args: &[Ty]) -> LResult<Vec<(u32, Vec<Ty>)>> {
        if let Some(e) = self.ctx.enums.get(enum_name).cloned() {
            let gparams: Vec<String> = e.generics.params.iter().map(|p| p.name.clone()).collect();
            let subst: HashMap<String, Ty> = gparams.iter().cloned().zip(enum_args.iter().cloned()).collect();
            let mut out = Vec::new();
            for (i, v) in e.variants.iter().enumerate() {
                let tys: Vec<Ty> = match &v.payload {
                    VariantPayload::None => vec![],
                    VariantPayload::Tuple(t) => t.iter().map(|t| {
                        lower_type(t, &gparams, self.ctx).map(|ty| apply_subst(&ty, &subst))
                    }).collect::<Result<_, _>>().map_err(conv)?,
                    VariantPayload::Named(f) => f.iter().map(|f| {
                        lower_type(&f.ty, &gparams, self.ctx).map(|ty| apply_subst(&ty, &subst))
                    }).collect::<Result<_, _>>().map_err(conv)?,
                };
                out.push((i as u32, tys));
            }
            Ok(out)
        } else {
            err(format!("unknown enum `{}` in match", enum_name), Span::new(0, 0))
        }
    }

    fn variant_in(&self, enum_name: &str, vlast: &str, variants: &[(u32, Vec<Ty>)], span: Span) -> LResult<(u32, Vec<Ty>)> {
        // Resolve the variant by name WITHIN this enum — variant names are
        // per-enum, not global (two enums may share a variant name, e.g.
        // `TokKind::Not` and `UnOp::Not`). Looking it up in the global
        // `ctx.variants` map (keyed partly by bare name) conflated same-named
        // variants across enums; scope it to `enum_name`'s own declaration.
        if let Some(edef) = self.ctx.enums.get(enum_name) {
            if let Some(idx) = edef.variants.iter().position(|v| v.name == vlast) {
                let tag = idx as u32;
                if let Some((t, tys)) = variants.iter().find(|(i, _)| *i == tag) {
                    return Ok((*t, tys.clone()));
                }
            }
        }
        err(format!("unknown variant `{}` of `{}`", vlast, enum_name), span)
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        use BinOp::*;
        let (cl, lt) = self.expr(l, None)?;
        if !matches!(cl.repr, Repr::Scalar(_)) && !matches!(op, And | Or) {
            return self.overloaded_binary(op, cl, lt, l, r, span);
        }
        // Right side expects the left's type for arithmetic/compare.
        let (cr, rt) = self.expr(r, Some(&lt))?;
        if matches!(cl.repr, Repr::Scalar(_)) { self.check_scalar_binary(op, &lt, span)?; }
        match op {
            Add | Sub | Mul | Div | Rem | BitAnd | BitOr | BitXor | Shl | Shr => {
                self.check_assignable(&rt, &lt, span)?;
                if !is_numeric(&lt) {
                    return err("arithmetic on non-numeric type", span);
                }
                let repr = cl.repr.clone();
                Ok((CoreExpr::new(CoreExprKind::Bin(op, Box::new(cl), Box::new(cr)), repr), lt))
            }
            Eq | Ne | Lt | Le | Gt | Ge => {
                self.check_assignable(&rt, &lt, span)?;
                Ok((
                    CoreExpr::new(
                        CoreExprKind::Bin(op, Box::new(cl), Box::new(cr)),
                        Repr::Scalar(ScalarRepr::Bool),
                    ),
                    Ty::bool(),
                ))
            }
            And | Or => {
                self.check_assignable(&lt, &Ty::bool(), span)?;
                self.check_assignable(&rt, &Ty::bool(), span)?;
                Ok((
                    CoreExpr::new(
                        CoreExprKind::Bin(op, Box::new(cl), Box::new(cr)),
                        Repr::Scalar(ScalarRepr::Bool),
                    ),
                    Ty::bool(),
                ))
            }
        }
    }

    fn check_scalar_binary(&self, op: BinOp, ty: &Ty, span: Span) -> LResult<()> {
        use BinOp::*;
        let valid = match op {
            Add | Sub | Mul | Div | Rem => is_numeric(ty),
            BitAnd | BitOr | BitXor | Shl | Shr => matches!(ty, Ty::Prim(p) if is_int_prim(*p)),
            Eq | Ne => true,
            Lt | Le | Gt | Ge => !matches!(ty, Ty::Prim(Prim::RawPtr)),
            And | Or => *ty == Ty::bool(),
        };
        if !valid {
            return err(
                format!("invalid binary operator for `{}`", ty_display(ty)),
                span,
            );
        }
        Ok(())
    }

    /// A binary operator on a non-scalar operand desugars to its trait method:
    /// `a + b` → `Add::add`, `a == b` → `Eq::eq` (`!=` negates it), and
    /// `a < b` → `Ord::cmp(a, b).is_lt()` (likewise `is_le`/`is_gt`/`is_ge`).
    fn overloaded_binary(&mut self, op: BinOp, cl: CoreExpr, lt: Ty, l: &Expr, r: &Expr, span: Span) -> LResult<(CoreExpr, Ty)> {
        use BinOp::*;
        let (trait_name, method, sym) = match op {
            Add => ("Add", "add", "+"),
            Sub => ("Sub", "sub", "-"),
            Mul => ("Mul", "mul", "*"),
            Div => ("Div", "div", "/"),
            Rem => ("Rem", "rem", "%"),
            Eq => ("Eq", "eq", "=="),
            Ne => ("Eq", "eq", "!="),
            Lt => ("Ord", "cmp", "<"),
            Le => ("Ord", "cmp", "<="),
            Gt => ("Ord", "cmp", ">"),
            Ge => ("Ord", "cmp", ">="),
            BitAnd => ("BitAnd", "bitand", "&"),
            BitOr => ("BitOr", "bitor", "|"),
            BitXor => ("BitXor", "bitxor", "^"),
            Shl | Shr => {
                return err(format!("shift operator on non-scalar type `{}`", ty_display(&lt)), span);
            }
            And | Or => unreachable!("logical operators are not overloadable"),
        };
        let what = format!("`{}`", sym);
        // A type that is only `PartialOrd` compares through `partial_cmp`;
        // an unordered pair (`None`) makes every comparison false.
        if trait_name == "Ord" && !matches!(lt, Ty::Tuple(_)) && self.lookup_trait_method(&lt, "Ord", "cmp").is_none()
            && self.lookup_trait_method(&lt, "PartialOrd", "partial_cmp").is_some()
        {
            let (call, ret) = self.trait_method_call(cl, lt, Some(l), "PartialOrd", "partial_cmp", &what, std::slice::from_ref(r), span)?;
            let test = match op { Lt => "is_lt", Le => "is_le", Gt => "is_gt", _ => "is_ge" };
            let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
            let var = |n: &str| ex(ExprKind::Path(Path::single(n.into(), span)));
            let id = self.fresh_local(call.repr.clone(), ret.clone());
            self.push_scope();
            self.bind("cmp#partial", id, ret.clone());
            let some = Pattern { kind: PatternKind::Variant { path: Path { segments: vec!["Option".into(), "Some".into()], span }, payload: vec![Pattern { kind: PatternKind::Binding { is_mut: false, name: "cmp#o".into() }, span }] }, span };
            let m = ex(ExprKind::Match { scrutinee: var("cmp#partial"), arms: vec![
                MatchArm { pattern: some, guard: None, body: ex(ExprKind::MethodCall { recv: var("cmp#o"), method: test.into(), args: vec![], span }), span },
                MatchArm { pattern: Pattern { kind: PatternKind::Wildcard, span }, guard: None, body: ex(ExprKind::Bool(false)), span },
            ] });
            let result = self.expr(&m, None);
            self.pop_scope();
            let (value, ty) = result?;
            let repr = value.repr.clone();
            return Ok((CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts: vec![CoreStmt::Let(id, call)], tail: Some(value) })), repr), ty));
        }
        let (call, ret) = if matches!(lt, Ty::Tuple(_)) {
            self.tuple_method(cl, lt, method, std::slice::from_ref(r), span)?
        } else if method == "eq" && array_elem_ty(&lt).is_some() {
            self.array_eq(cl, lt, r, span)?
        } else {
            self.trait_method_call(cl, lt, Some(l), trait_name, method, &what, std::slice::from_ref(r), span)?
        };
        match op {
            Ne => Ok((CoreExpr::new(CoreExprKind::Un(UnOp::Not, Box::new(call)), Repr::Scalar(ScalarRepr::Bool)), Ty::bool())),
            Lt | Le | Gt | Ge => {
                let test = match op { Lt => "is_lt", Le => "is_le", Gt => "is_gt", _ => "is_ge" };
                let entry = self.lookup_method(&ret, test).ok_or_else(|| LowerError {
                    msg: format!("`Ord::cmp` must return `Ordering` (found `{}`)", ty_display(&ret)),
                    span,
                })?;
                self.method_call_lowered(call, ret, None, entry, &[], span)
            }
            _ => Ok((call, ret)),
        }
    }

    /// Whether an argument's own type can only be inferred from sibling
    /// arguments — currently: a bare enum-variant constructor (`None`, `Ok(x)`,
    /// `Some(x)`), whose enum type args may not be pinnable from itself. Such
    /// args are lowered in phase 2 after concrete args pin the type vars.
    /// If `a` is an `as_c_bytes(x)` call, lower it to an `AsCBytes` node (a
    /// `RawPtr` to a scoped native copy of `x`'s contents). `x` may be a `String` or a
    /// scalar `Array<T>`. Returns `Ok(None)` if `a` is not `as_c_bytes`. Errors
    /// if the declared parameter is not `RawPtr` or if `x` is not a String/scalar
    /// array. When the extern parameter is `mut`, the copy is also written BACK
    /// into the array after the call (copy-out, e.g. `read(fd, buf, n)`). Only
    /// called for direct extern-call arguments — see the caller and `docs/ffi.md`.
    fn try_as_c_bytes_arg(&mut self, a: &Expr, declared: &Ty, param_is_mut: bool) -> LResult<Option<(CoreExpr, Ty)>> {
        let ExprKind::Call(callee, cargs) = &*a.kind else { return Ok(None) };
        let ExprKind::Path(p) = &*callee.kind else { return Ok(None) };
        if p.last() != "as_c_bytes" { return Ok(None); }
        // Don't shadow a user function of the same name.
        if self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "as_c_bytes") {
            return Ok(None);
        }
        if cargs.len() != 1 {
            return err("`as_c_bytes` takes 1 argument (a String or scalar Array)", a.span);
        }
        if !matches!(declared, Ty::Prim(Prim::RawPtr)) {
            return err(
                "`as_c_bytes` produces a `RawPtr`; the extern parameter here is not `RawPtr`",
                a.span,
            );
        }
        let (cs, sty) = self.expr(&cargs[0], None)?;
        // String: bytes (+ NUL); element stride 1.
        if matches!(sty, Ty::Prim(Prim::Str)) {
            return Ok(Some((
                CoreExpr::new(
                    CoreExprKind::AsCBytes {
                        src: Box::new(cs), elem: ScalarRepr::U8, is_string: true, copy_out: false,
                    },
                    Repr::Scalar(ScalarRepr::Ptr),
                ),
                Ty::Prim(Prim::RawPtr),
            )));
        }
        // Scalar Array<T>: contiguous T elements, stride = sizeof(T).
        if let Some(elem_ty) = array_elem_ty(&sty) {
            let elem_repr = self.repr_of(&elem_ty, cargs[0].span)?;
            let Repr::Scalar(elem) = elem_repr else {
                return err(
                    format!("`as_c_bytes` requires a String or an array of scalars, found `Array<{}>`",
                        ty_display(&elem_ty)),
                    cargs[0].span,
                );
            };
            return Ok(Some((
                CoreExpr::new(
                    CoreExprKind::AsCBytes {
                        src: Box::new(cs), elem, is_string: false, copy_out: param_is_mut,
                    },
                    Repr::Scalar(ScalarRepr::Ptr),
                ),
                Ty::Prim(Prim::RawPtr),
            )));
        }
        err(
            format!("`as_c_bytes` requires a `String` or a scalar `Array`, found `{}`", ty_display(&sty)),
            cargs[0].span,
        )
    }

    /// If `declared` is an `extern fn(..)` callback type and `a` names a (non-
    /// generic) gc-rust function, resolve it and emit a `CallbackPtr` (a `RawPtr`
    /// to a synthesized C trampoline). Returns `Ok(None)` if `declared` is not a
    /// callback type. Errors if `a` is not a plain function name, the function is
    /// generic/unknown, or its signature doesn't match. See `docs/ffi.md`.
    fn try_callback_arg(&mut self, a: &Expr, declared: &Ty) -> LResult<Option<(CoreExpr, Ty)>> {
        let Ty::ExternFn { params: cb_params, ret: cb_ret } = declared else { return Ok(None) };
        // The argument must be a bare path naming a function.
        let ExprKind::Path(path) = &*a.kind else {
            return err("a callback argument must be a named function", a.span);
        };
        let name = path.last();
        let fq = if !path.is_single() {
            let qualified = path.segments.join("::");
            self.ctx.fns.contains_key(&qualified).then_some(qualified)
        } else {
            self.ctx.use_aliases.get(name)
                .filter(|fq| self.ctx.fns.contains_key(*fq))
                .cloned()
                .or_else(|| {
                    let mut matches = self.ctx.fns.keys().filter(|k| k.rsplit("::").next() == Some(name));
                    let first = matches.next()?.clone();
                    matches.next().is_none().then_some(first)
                })
        };
        let Some(fq) = fq else {
            return err(format!("unknown function `{}` for callback", name), a.span);
        };
        let f = self.ctx.fns[&fq].clone();
        if !f.generics.params.is_empty() {
            return err(format!("callback function `{}` cannot be generic", name), a.span);
        }
        // Check the function's signature matches the expected callback type.
        let gparams: Vec<String> = Vec::new();
        let f_params: Vec<Ty> = f.params.iter()
            .map(|p| lower_type(&p.ty, &gparams, self.ctx).map_err(conv))
            .collect::<Result<_, _>>()?;
        let f_ret = match &f.ret {
            Some(t) => lower_type(t, &gparams, self.ctx).map_err(conv)?,
            None => Ty::unit(),
        };
        if f_params.len() != cb_params.len() {
            return err(
                format!("callback `{}` takes {} argument(s), but the expected type takes {}",
                    name, f_params.len(), cb_params.len()),
                a.span,
            );
        }
        for (fp, cp) in f_params.iter().zip(cb_params.iter()) {
            self.check_assignable(fp, cp, a.span)?;
        }
        self.check_assignable(&f_ret, cb_ret, a.span)?;
        for ty in f_params.iter().chain(std::iter::once(&f_ret)) {
            if !matches!(self.repr_of(ty, a.span)?, Repr::Scalar(_) | Repr::Unit) {
                return err("callbacks support only scalar parameters and scalar or unit returns; aggregate callback ABI is not implemented", a.span);
            }
        }
        let mangled = mangle(&fq, &[]);
        let job = Job { f, subst: HashMap::new(), mangled, self_ty: None };
        let fid = self.mono.intern(job, self.count);
        Ok(Some((
            CoreExpr::new(CoreExprKind::CallbackPtr(fid), Repr::Scalar(ScalarRepr::Ptr)),
            declared.clone(),
        )))
    }

    fn is_inference_dependent_arg(&self, a: &Expr) -> bool {
        let path = match &*a.kind {
            ExprKind::Path(p) => p,
            ExprKind::Call(c, _) => match &*c.kind { ExprKind::Path(p) => p, _ => return false },
            _ => return false,
        };
        let key = path.segments.join("::");
        self.ctx.variants.contains_key(&key) || self.ctx.variants.contains_key(path.last())
    }

    /// Call an evaluated function value. Callee and arguments stay in their
    /// written order; ANF roots the callee before any argument can allocate.
    fn call_value(&mut self, callee: &Expr, args: &[Expr], expected: Option<&Ty>, span: Span) -> LResult<(CoreExpr, Ty)> {
        // Argument types provide a bidirectional hint for unannotated lambdas
        // and generic factories. Failed/unknown probes provide no constraint;
        // an explicitly typed callee can still supply its argument types.
        let hint_params = args.iter().map(|arg| self.probe_expr_type(arg, None)
            .ok().filter(|ty| !has_var(ty) && !is_never(ty))).collect::<Option<Vec<_>>>();
        let hint = hint_params.map(|params| Ty::Fn {
            params, ret: Box::new(expected.cloned().unwrap_or_else(Ty::unit)),
        });
        let (callee, signature) = self.expr(callee, hint.as_ref())?;
        self.invoke_value(callee, signature, args, span)
    }

    fn invoke_value(&mut self, callee: CoreExpr, signature: Ty, args: &[Expr], span: Span) -> LResult<(CoreExpr, Ty)> {
        let Ty::Fn { params, ret } = signature else {
            return err(format!("cannot call a value of type `{}`", ty_display(&signature)), span);
        };
        if args.len() != params.len() {
            return err(format!("closure expects {} args, got {}", params.len(), args.len()), span);
        }
        let mut lowered = Vec::with_capacity(args.len());
        for (arg, parameter) in args.iter().zip(&params) {
            let (core, actual) = self.expr(arg, Some(parameter))?;
            self.check_assignable(&actual, parameter, arg.span)?;
            lowered.push(core);
        }
        let repr = self.repr_of(&ret, span)?;
        Ok((CoreExpr::new(CoreExprKind::CallClosure { callee: Box::new(callee), args: lowered }, repr), *ret))
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], expected: Option<&Ty>, span: Span) -> LResult<(CoreExpr, Ty)> {
        // Explicit type arguments set by the caller for this call only.
        let turbofish = TURBOFISH.with(|t| t.borrow_mut().take());
        let ExprKind::Path(path) = &*callee.kind else {
            return self.call_value(callee, args, expected, span);
        };
        let name = path.last();
        // Lexical bindings shadow global functions, even when non-callable.
        if path.is_single() {
            if let Some((_, signature)) = self.lookup(name) {
                let (value, _) = self.expr(callee, None)?;
                return self.invoke_value(value, signature, args, span);
            }
        }
        // Associated (self-less) function call: `Type::func(args)`. The path's
        // second-to-last segment names the type; look up an impl item of that
        // name with NO `self`. (Methods with `self` go through `method_call`.)
        // `Box`, `Rc`, and `Arc` are the type they point to (every heap value
        // is a GC reference): `Box::new(x)` is `x`, and `Rc::clone(x)` shares it.
        if path.segments.len() == 2 && matches!(path.segments[0].as_str(), "Box" | "Rc" | "Arc")
            && matches!(name, "new" | "clone") && args.len() == 1
            && self.ctx.canon(&path.segments[0]).is_none()
        {
            return self.expr(&args[0], expected);
        }
        if path.segments.len() == 2 && path.segments[0] == "Array" && name == "new"
            && !self.ctx.structs.contains_key("Array")
        {
            if args.len() != 1 { return err("`Array::new` takes a length", span); }
            return self.array_new(&args[0], expected, span);
        }
        // `T::f(args)` on a type parameter: `T`'s own associated function.
        if path.segments.len() == 2 {
            if let Some(t) = self.subst.get(&path.segments[0]).cloned() {
                if let Some(base) = crate::types::receiver_base(&t) {
                    if let Some(entry) = self.ctx.methods.get(&(base.clone(), name.to_string())).cloned() {
                        if !entry.method.has_self {
                            return self.assoc_call(&base, Some(&t), None, &entry, args, expected, span);
                        }
                    }
                }
            }
        }
        if path.segments.len() >= 2 {
            let written = path.segments[..path.segments.len() - 1].join("::");
            let canonical = self.ctx.canon(&written).unwrap_or(written);
            let mut alias_target = self.ctx.aliases.get(&canonical).map(|alias| alias.target.clone());
            // `Vec::<i64>::new()`: the type's arguments are given.
            if let Some((true, tys)) = &turbofish {
                if alias_target.is_none() && self.ctx.structs.contains_key(&canonical) || self.ctx.enums.contains_key(&canonical) {
                    alias_target = Some(Ty::Named { name: canonical.clone(), args: tys.clone() });
                }
            }
            if name == "new" {
                if let Some(target) = alias_target.as_ref().filter(|ty| matches!(ty, Ty::Named { name, .. } if name == "Array")) {
                    if args.len() != 1 { return err(format!("`{}::new` takes a length", path.segments[..path.segments.len() - 1].join("::")), span); }
                    let mut arguments = HashMap::new();
                    if let Some(expected) = expected { unify_infer(target, expected, &mut arguments); }
                    let concrete = apply_subst(target, &arguments);
                    if has_var(&concrete) {
                        self.infer.unresolved = Some(concrete);
                        return err("cannot infer the array alias element type; annotate the binding or constrain a later use", span);
                    }
                    if let Some(expected) = expected { self.check_assignable(&concrete, expected, span)?; }
                    self.check_alias_bounds(&canonical, &concrete, span)?;
                    return self.array_new(&args[0], Some(&concrete), span);
                }
            }
            let base = alias_target.as_ref().and_then(crate::types::receiver_base).unwrap_or_else(|| canonical.clone());
            if let Some(entry) = self.ctx.methods.get(&(base.clone(), name.to_string())).cloned() {
                if !entry.method.has_self {
                    let entry = self.pick_among(&base, alias_target.as_ref(), entry, args)?;
                    return self.assoc_call(&base, alias_target.as_ref(), alias_target.as_ref().map(|_| canonical.as_str()), &entry, args, expected, span);
                }
                // `Type::method(x, args)`: a method called with its receiver
                // as the first argument.
                if !args.is_empty() {
                    let (crecv, recv_ty) = self.expr(&args[0], None)?;
                    return self.method_call_lowered(crecv, recv_ty, Some(&args[0]), entry, &args[1..], span);
                }
            }
            // `Trait::method(x, args)`: the method of `impl Trait for <x's type>`.
            let trait_name = self.ctx.traits.keys()
                .find(|t| **t == canonical || (!canonical.contains("::") && t.rsplit("::").next() == Some(canonical.as_str())))
                .cloned();
            if let Some(trait_name) = trait_name {
                let def = self.ctx.traits[&trait_name].clone();
                let declared = def.items.iter().any(|item| match item {
                    TraitItem::Required(sig) => sig.name == name && sig.has_self,
                    TraitItem::Provided(f) => f.name == name && f.has_self,
                    TraitItem::AssocType(_) | TraitItem::Const(_) => false,
                });
                // `Default::default()`: a function without `self`, taken from
                // the impl for the expected type.
                let static_fn = def.items.iter().any(|item| match item {
                    TraitItem::Required(sig) => sig.name == name && !sig.has_self,
                    TraitItem::Provided(f) => f.name == name && !f.has_self,
                    TraitItem::AssocType(_) | TraitItem::Const(_) => false,
                });
                if static_fn {
                    let short = trait_name.rsplit("::").next().unwrap().to_string();
                    let Some(target) = expected.filter(|t| !has_var(t)).cloned() else {
                        return err(format!("cannot tell which type's `{}::{}` to call; annotate the expected type", short, name), span);
                    };
                    // `From<T> for T`: converting a value to its own type.
                    if short == "From" && name == "from" && args.len() == 1 {
                        if let Ok(t) = self.probe_expr_type(&args[0], Some(&target)) {
                            if t == target { return self.expr(&args[0], Some(&target)); }
                        }
                    }
                    let base = crate::types::receiver_base(&target).ok_or_else(|| LowerError { msg: format!("`{}` does not implement `{}`", ty_display(&target), short), span })?;
                    let entry = self.lookup_trait_method(&target, &trait_name, name)
                        .or_else(|| self.lookup_trait_method(&target, &short, name))
                        .ok_or_else(|| LowerError { msg: format!("`{}` does not implement `{}`", ty_display(&target), short), span })?;
                    let entry = self.pick_among(&base, Some(&target), entry, args)?;
                    return self.assoc_call(&base, Some(&target), None, &entry, args, expected, span);
                }
                if declared && !args.is_empty() {
                    let (crecv, recv_ty) = self.expr(&args[0], None)?;
                    if dyn_trait(&recv_ty).is_some() && self.lookup_trait_method(&recv_ty, &trait_name, name).is_none() {
                        return self.dyn_method_call(crecv, recv_ty, name, &args[1..], span);
                    }
                    let short = trait_name.rsplit("::").next().unwrap().to_string();
                    let entry = self.lookup_trait_method(&recv_ty, &trait_name, name)
                        .or_else(|| self.lookup_trait_method(&recv_ty, &short, name))
                        .ok_or_else(|| LowerError { msg: format!("`{}` does not implement `{}`", ty_display(&recv_ty), short), span })?;
                    let entry = self.pick_overload(&recv_ty, entry, &args[1..])?;
                    return self.method_call_lowered(crecv, recv_ty, Some(&args[0]), entry, &args[1..], span);
                }
            }
        }
        // Array intrinsics — built-in varlen arrays. The element type comes
        // from `expected` (for new) or the array's own type (get/set/len).
        if native_intrinsic_requires_unsafe(name)
            && !self.ctx.fns.keys().any(|key| key.rsplit("::").next() == Some(name))
        {
            self.require_unsafe(&format!("native primitive `{name}`"), span)?;
        }
        if !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name) {
            match name {
                "array_new" if args.len() == 1 => return self.array_new(&args[0], expected, span),
                "array_len" if args.len() == 1 => return self.array_len_op(&args[0], span),
                "array_get" if args.len() == 2 => return self.array_get_op(&args[0], &args[1], span),
                "array_get_unchecked" if args.len() == 2 => return self.array_get_unchecked_op(&args[0], &args[1], span),
                "array_set" if args.len() == 3 => return self.array_set_op(&args[0], &args[1], &args[2], span),
                _ => {}
            }
        }
        if name == "panic" && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name) {
            if args.len() != 1 { return err("panic takes one String", span); }
            let (message, actual) = self.expr(&args[0], Some(&Ty::Prim(Prim::Str)))?;
            self.check_assignable(&actual, &Ty::Prim(Prim::Str), span)?;
            return Ok((CoreExpr::new(CoreExprKind::Panic(Box::new(message)), Repr::Unit), never_ty()));
        }
        if name == "str_join_array" && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name) {
            if args.len() != 3 { return err("str_join_array takes array, count, separator", span); }
            let str_ty = Ty::Prim(Prim::Str);
            let array_ty = Ty::Named { name: "Array".into(), args: vec![str_ty.clone()] };
            let mut lowered = Vec::new();
            for (arg, ty) in args.iter().zip([array_ty, Ty::i64(), str_ty.clone()]) {
                let (expr, actual) = self.expr(arg, Some(&ty))?;
                self.check_assignable(&actual, &ty, arg.span)?;
                lowered.push(expr);
            }
            let layout = self.string_layout_id(span)?;
            return Ok((CoreExpr::new(CoreExprKind::StrJoin { layout, args: lowered }, Repr::Ref(layout)), str_ty));
        }
        if name == "host_io" && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name) {
            if args.len() != 4 { return err("host_io takes operation, path, text, and limit", span); }
            let ExprKind::Int(op, _) = args[0].kind.as_ref() else {
                return err("host_io operation must be a literal integer", span);
            };
            let op = u32::try_from(*op).map_err(|_| LowerError { msg: "invalid host operation".into(), span })?;
            if op > crate::runtime::io::STDERR_RAW { return err("unknown host operation", span); }
            let str_ty = Ty::Prim(Prim::Str);
            let array_ty = Ty::Named { name: "Array".into(), args: vec![str_ty.clone()] };
            let response_ty = Ty::Named { name: "IoResponse".into(), args: vec![] };
            let Some(def) = self.ctx.structs.get("IoResponse") else { return err("host_io requires standard IoResponse", span); };
            let StructBody::Named(fields) = &def.body else { return err("invalid IoResponse representation", span); };
            let expected_fields = [("text", str_ty.clone()), ("entries", array_ty.clone()), ("code", Ty::i64()), ("kind", Ty::i64())];
            if def.is_value || !def.generics.params.is_empty() || fields.len() != 4 {
                return err("invalid IoResponse representation", span);
            }
            for (field, (name, ty)) in fields.iter().zip(expected_fields) {
                if field.name != name || lower_type(&field.ty, &[], self.ctx).map_err(conv)? != ty {
                    return err("invalid IoResponse field layout", span);
                }
            }
            let mut lowered = Vec::new();
            for (arg, ty) in args[1..].iter().zip([str_ty.clone(), str_ty.clone(), Ty::i64()]) {
                let (expr, actual) = self.expr(arg, Some(&ty))?;
                self.check_assignable(&actual, &ty, arg.span)?;
                lowered.push(expr);
            }
            let Repr::Ref(response) = self.repr_of(&response_ty, span)? else { unreachable!() };
            let Repr::Ref(string) = self.repr_of(&str_ty, span)? else { unreachable!() };
            let Repr::Ref(entries) = self.repr_of(&array_ty, span)? else { unreachable!() };
            return Ok((CoreExpr::new(CoreExprKind::HostCall { op, response, string, entries, args: lowered }, Repr::Ref(response)), response_ty));
        }
        // String intrinsics — built-in `String` ops backed by runtime externs.
        // Skipped if shadowed by a user-defined function of the same name.
        if !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name) {
            match name {
                "print_str" | "print" | "str_len" | "str_eq" | "str_concat"
                | "str_get" | "str_substring" | "to_string" | "str_to_float" | "str_hash"
                | "read_file" | "float_bits" | "char_to_str" | "fmt_f64" => {
                    return self.str_intrinsic(name, args, span);
                }
                "type_id_of" | "type_name_of" | "field_count" | "field_name" | "field_kind"
                | "field_i64" => {
                    return self.reflect_intrinsic(name, args, span);
                }
                // Heap-explorer P2: on-demand mid-execution heap snapshot. Called
                // from managed code → at a safepoint by construction (signal-safe).
                "heap_snapshot" => {
                    if !args.is_empty() {
                        return err("heap_snapshot() takes no arguments", span);
                    }
                    return Ok((
                        CoreExpr::new(
                            CoreExprKind::RuntimeCall {
                                func: "ai_heap_snapshot",
                                args: vec![],
                                ret: Repr::Unit,
                            },
                            Repr::Unit,
                        ),
                        Ty::unit(),
                    ));
                }
                _ => {}
            }
        }
        // `as_c_bytes` is only valid as a DIRECT argument to an extern call (it's
        // intercepted there). Reaching it through the general expression path
        // means it was used somewhere else — reject it, because the native copy it
        // produces is only valid for the duration of that one call.
        if name == "as_c_bytes"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "as_c_bytes")
        {
            return err(
                "`as_c_bytes(s)` may only appear as a direct argument to an `extern \"C\"` call \
                 (its pointer is valid only for that call — see docs/ffi.md)",
                span,
            );
        }
        // Atom<T> intrinsics. `atom_load(a) -> T` (atomic), `atom_cas(a, old,
        // new) -> bool` (atomic CAS + write barrier). The atom is an ordinary
        // heap object `{ value: T }`; swap!'s retry loop is in the prelude over
        // these (so old/new/atom are frame roots → GC-safe). See docs/threads.md.
        if (name == "atom_load" || name == "atom_cas")
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name)
        {
            let arity = if name == "atom_load" { 1 } else { 3 };
            if args.len() != arity {
                return err(format!("`{name}` takes {arity} argument(s)"), span);
            }
            // First arg is the atom; recover its element type T from `Atom<T>`.
            let (catom, atom_ty) = self.expr(&args[0], None)?;
            let elem_ty = match &atom_ty {
                Ty::Named { name: n, args: ta } if n.rsplit("::").next() == Some("Atom") && !ta.is_empty() => ta[0].clone(),
                _ => return err(format!("`{}` requires an `Atom<T>`, found `{}`", name, ty_display(&atom_ty)), args[0].span),
            };
            let elem = self.repr_of(&elem_ty, args[0].span)?;
            if !matches!(elem, Repr::Scalar(_) | Repr::Ref(_)) {
                return err("Atom operations require a scalar or managed reference; inline aggregates have no CAS equality contract", span);
            }
            if name == "atom_load" {
                return Ok((
                    CoreExpr::new(CoreExprKind::AtomLoad { atom: Box::new(catom), elem: elem.clone() }, elem),
                    elem_ty,
                ));
            }
            // atom_cas(a, old, new) -> bool
            let (cold, oldt) = self.expr(&args[1], Some(&elem_ty))?;
            self.check_assignable(&oldt, &elem_ty, args[1].span)?;
            let (cnew, newt) = self.expr(&args[2], Some(&elem_ty))?;
            self.check_assignable(&newt, &elem_ty, args[2].span)?;
            return Ok((
                CoreExpr::new(
                    CoreExprKind::AtomCas { atom: Box::new(catom), old: Box::new(cold), new: Box::new(cnew) },
                    Repr::Scalar(ScalarRepr::Bool),
                ),
                Ty::bool(),
            ));
        }
        // Native channel intrinsics operate on initialized reference message
        // boxes. Witness arrays bind the generic type, not queue storage.
        // Public Channel<T> boxes/unboxes arbitrary managed representations.
        if name == "chan_new"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "chan_new")
        {
            if args.len() != 2 { return err("`chan_new` takes capacity and message array", span); }
            let (ccap, actual) = self.expr(&args[0], Some(&Ty::i64()))?;
            self.check_assignable(&actual, &Ty::i64(), span)?;
            let (buffer, buffer_ty) = self.expr(&args[1], None)?;
            let message_ty = array_elem_ty(&buffer_ty).ok_or_else(|| LowerError { msg: "channel witness must be an Array".into(), span })?;
            let message_repr = self.repr_of(&message_ty, span)?;
            let Repr::Ref(layout) = message_repr else { return err("channel intrinsic requires a reference message box", span); };
            let layout_expr = CoreExpr::new(CoreExprKind::ConstInt(layout as u64, ScalarRepr::I32), Repr::Scalar(ScalarRepr::I32));
            return Ok((
                CoreExpr::new(CoreExprKind::RuntimeCall { func: "ai_chan_new", args: vec![ccap, buffer, layout_expr], ret: Repr::Scalar(ScalarRepr::Ptr) }, Repr::Scalar(ScalarRepr::Ptr)),
                Ty::Prim(Prim::RawPtr),
            ));
        }
        if name == "chan_send"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "chan_send")
        {
            if args.len() != 3 { return err("`chan_send` takes 3 arguments (buf, ctrl, value)", span); }
            let (cbuf, buffer_ty) = self.expr(&args[0], None)?;
            let expected = array_elem_ty(&buffer_ty).ok_or_else(|| LowerError { msg: "channel witness must be an Array".into(), span })?;
            if !matches!(self.repr_of(&expected, span)?, Repr::Ref(_)) { return err("channel intrinsic requires a reference message box", span); }
            let (cctrl, ctrl_ty) = self.expr(&args[1], Some(&Ty::Prim(Prim::RawPtr)))?;
            self.check_assignable(&ctrl_ty, &Ty::Prim(Prim::RawPtr), span)?;
            let (cval, actual) = self.expr(&args[2], Some(&expected))?;
            self.check_assignable(&actual, &expected, span)?;
            return Ok((
                CoreExpr::new(CoreExprKind::ChanSend { buf: Box::new(cbuf), ctrl: Box::new(cctrl), value: Box::new(cval) }, Repr::Scalar(ScalarRepr::I64)),
                Ty::i64(),
            ));
        }
        if name == "chan_recv"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "chan_recv")
        {
            if args.len() != 2 { return err("`chan_recv` takes 2 arguments (buf, ctrl)", span); }
            let (cbuf, bty) = self.expr(&args[0], None)?;
            let (cctrl, ctrl_ty) = self.expr(&args[1], Some(&Ty::Prim(Prim::RawPtr)))?;
            self.check_assignable(&ctrl_ty, &Ty::Prim(Prim::RawPtr), span)?;
            let elem_ty = array_elem_ty(&bty)
                .ok_or_else(|| LowerError { msg: format!("`chan_recv` buffer must be an Array, found `{}`", ty_display(&bty)), span: args[0].span })?;
            let elem = self.repr_of(&elem_ty, span)?;
            if !matches!(elem, Repr::Ref(_)) { return err("channel intrinsic requires a reference message box", span); }
            let option_ty = Ty::Named { name: "Option".into(), args: vec![elem_ty] };
            let option_repr = self.repr_of(&option_ty, span)?;
            return Ok((
                CoreExpr::new(CoreExprKind::ChanRecv { buf: Box::new(cbuf), ctrl: Box::new(cctrl), elem: elem.clone() }, option_repr),
                option_ty,
            ));
        }
        if (name == "chan_waiting_senders" || name == "chan_waiting_receivers")
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name)
        {
            if args.len() != 1 { return err("channel waiter count takes one control handle", span); }
            let (ctrl, actual) = self.expr(&args[0], Some(&Ty::Prim(Prim::RawPtr)))?;
            self.check_assignable(&actual, &Ty::Prim(Prim::RawPtr), span)?;
            let func = if name == "chan_waiting_senders" { "ai_chan_waiting_senders" } else { "ai_chan_waiting_receivers" };
            return Ok((CoreExpr::new(CoreExprKind::RuntimeCall { func, args: vec![ctrl], ret: Repr::Scalar(ScalarRepr::I64) }, Repr::Scalar(ScalarRepr::I64)), Ty::i64()));
        }
        if (name == "chan_sender_clone" || name == "chan_sender_drop" || name == "chan_close")
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name)
        {
            if args.len() != 1 { return err(format!("`{}` takes 1 argument (ctrl)", name), span); }
            let (cctrl, ctrl_ty) = self.expr(&args[0], Some(&Ty::Prim(Prim::RawPtr)))?;
            self.check_assignable(&ctrl_ty, &Ty::Prim(Prim::RawPtr), span)?;
            let func = match name { "chan_sender_clone" => "ai_chan_sender_clone", "chan_close" => "ai_chan_close", _ => "ai_chan_sender_drop" };
            return Ok((
                CoreExpr::new(CoreExprKind::RuntimeCall { func, args: vec![cctrl], ret: Repr::Unit }, Repr::Unit),
                Ty::unit(),
            ));
        }
        // AtomicI64 intrinsics → runtime externs. The handle is a RawPtr.
        // `atomic_i64_new(v)->RawPtr`, `..._load(a)->i64`, `..._store(a,v)->i64`,
        // `..._fetch_add(a,d)->i64`, `..._cas(a,expected,new)->i64` (1/0).
        {
            let atomic_intr: Option<(&'static str, &[ArgKind], Repr, Ty)> = match name {
                "atomic_i64_new" => Some(("ai_atomic_i64_new", &[ArgKind::Int], Repr::Scalar(ScalarRepr::Ptr), Ty::Prim(Prim::RawPtr))),
                "atomic_i64_load" => Some(("ai_atomic_i64_load", &[ArgKind::Ptr], Repr::Scalar(ScalarRepr::I64), Ty::i64())),
                "atomic_i64_store" => Some(("ai_atomic_i64_store", &[ArgKind::Ptr, ArgKind::Int], Repr::Scalar(ScalarRepr::I64), Ty::i64())),
                "atomic_i64_fetch_add" => Some(("ai_atomic_i64_fetch_add", &[ArgKind::Ptr, ArgKind::Int], Repr::Scalar(ScalarRepr::I64), Ty::i64())),
                "atomic_i64_cas" => Some(("ai_atomic_i64_cas_marker", &[ArgKind::Ptr, ArgKind::Int, ArgKind::Int], Repr::Scalar(ScalarRepr::I64), Ty::i64())),
                _ => None,
            };
            if let Some((func, kinds, ret, ret_ty)) = atomic_intr {
                if !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name) {
                    let func = if func == "ai_atomic_i64_cas_marker" { "ai_atomic_i64_compare_and_set" } else { func };
                    if args.len() != kinds.len() {
                        return err(format!("`{}` takes {} argument(s)", name, kinds.len()), span);
                    }
                    let mut cargs = Vec::new();
                    for (a, k) in args.iter().zip(kinds) {
                        let (ca, at) = self.expr(a, None)?;
                        match k {
                            ArgKind::Int if !matches!(at, Ty::Prim(Prim::I64)) =>
                                return err(format!("`{}` argument must be i64, found `{}`", name, ty_display(&at)), a.span),
                            ArgKind::Ptr if !matches!(at, Ty::Prim(Prim::RawPtr)) =>
                                return err(format!("`{}` argument must be a RawPtr handle, found `{}`", name, ty_display(&at)), a.span),
                            _ => {}
                        }
                        cargs.push(ca);
                    }
                    return Ok((CoreExpr::new(CoreExprKind::RuntimeCall { func, args: cargs, ret: ret.clone() }, ret), ret_ty));
                }
            }
        }
        if matches!(name, "thread_bind_owner" | "atomic_i64_bind_owner" | "chan_bind_owner")
            && !self.ctx.fns.keys().any(|key| key.rsplit("::").next().unwrap() == name)
        {
            if args.len() != 2 { return err(format!("`{name}` takes a handle and managed owner"), span); }
            let (handle, handle_ty) = self.expr(&args[0], None)?;
            if !matches!(handle_ty, Ty::Prim(Prim::RawPtr)) {
                return err(format!("`{name}` handle must be RawPtr"), args[0].span);
            }
            let (owner, _) = self.expr(&args[1], None)?;
            if !matches!(&owner.repr, Repr::Ref(_)) {
                return err(format!("`{name}` owner must be a managed reference"), args[1].span);
            }
            return Ok((CoreExpr::new(CoreExprKind::RuntimeCall {
                func: match name { "thread_bind_owner" => "ai_thread_bind_owner", "atomic_i64_bind_owner" => "ai_atomic_i64_bind_owner", _ => "ai_chan_bind_owner" }, args: vec![handle, owner], ret: Repr::Unit,
            }, Repr::Unit), Ty::unit()));
        }
        // `thread_spawn(closure)` → spawn an OS thread; returns a RawPtr handle.
        // The closure must be a no-arg `fn() -> i64` (M2 restricts results to i64).
        if name == "thread_spawn"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "thread_spawn")
        {
            if args.len() != 1 {
                return err("`thread_spawn` takes 1 argument (a `fn() -> i64` closure)", span);
            }
            // SEND/SYNC: a spawned closure shares its captures with the parent
            // thread, so every captured value must be `Sync` (safe to share). A
            // captured mutable reference type is NOT Sync → reject it, pointing at
            // a concrete fix (use Atom/AtomicI64 or build from Sync parts). See
            // `docs/threads.md`. (Deeply-immutable values are Sync automatically,
            // so the common case is unaffected.)
            self.check_spawn_captures_sync(&args[0])?;
            let (cf, ft) = self.expr(&args[0], None)?;
            match &ft {
                Ty::Fn { params, ret } if params.is_empty() && matches!(**ret, Ty::Prim(Prim::I64)) => {}
                _ => return err(
                    format!("`thread_spawn` requires a `fn() -> i64` closure, found `{}`", ty_display(&ft)),
                    args[0].span,
                ),
            }
            return Ok((
                CoreExpr::new(CoreExprKind::ThreadSpawn(Box::new(cf)), Repr::Scalar(ScalarRepr::Ptr)),
                Ty::Prim(Prim::RawPtr),
            ));
        }
        // `thread_join(handle)` → block; returns the thread's i64 result.
        if name == "thread_join"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "thread_join")
        {
            if args.len() != 1 {
                return err("`thread_join` takes 1 argument (a thread handle)", span);
            }
            let (ch, ht) = self.expr(&args[0], None)?;
            if !matches!(ht, Ty::Prim(Prim::RawPtr)) {
                return err(
                    format!("`thread_join` requires a thread handle (RawPtr), found `{}`", ty_display(&ht)),
                    args[0].span,
                );
            }
            return Ok((
                CoreExpr::new(CoreExprKind::ThreadJoin(Box::new(ch)), Repr::Scalar(ScalarRepr::I64)),
                Ty::i64(),
            ));
        }
        // `thread_sleep(ms)` / `thread_yield()` / `thread_current_id()`.
        if name == "thread_sleep"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "thread_sleep")
        {
            if args.len() != 1 { return err("`thread_sleep` takes 1 argument (millis)", span); }
            let (cm, _) = self.expr(&args[0], Some(&Ty::i64()))?;
            return Ok((
                CoreExpr::new(CoreExprKind::ThreadSleep(Box::new(cm)), Repr::Scalar(ScalarRepr::I64)),
                Ty::i64(),
            ));
        }
        if name == "thread_yield"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "thread_yield")
        {
            if !args.is_empty() { return err("`thread_yield` takes no arguments", span); }
            return Ok((
                CoreExpr::new(CoreExprKind::ThreadYield, Repr::Scalar(ScalarRepr::I64)),
                Ty::i64(),
            ));
        }
        if name == "thread_current_id"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "thread_current_id")
        {
            if !args.is_empty() { return err("`thread_current_id` takes no arguments", span); }
            return Ok((
                CoreExpr::new(CoreExprKind::ThreadCurrentId, Repr::Scalar(ScalarRepr::I64)),
                Ty::i64(),
            ));
        }
        // `ptr_read_i64(p)` → load an i64 through a RawPtr (for FFI callbacks).
        if name == "ptr_read_i64"
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == "ptr_read_i64")
        {
            if args.len() != 1 {
                return err("`ptr_read_i64` takes 1 argument (a RawPtr)", span);
            }
            let (cp, pt) = self.expr(&args[0], None)?;
            if !matches!(pt, Ty::Prim(Prim::RawPtr)) {
                return err(
                    format!("`ptr_read_i64` requires a `RawPtr`, found `{}`", ty_display(&pt)),
                    args[0].span,
                );
            }
            return Ok((
                CoreExpr::new(CoreExprKind::PtrReadI64(Box::new(cp)), Repr::Scalar(ScalarRepr::I64)),
                Ty::i64(),
            ));
        }
        // print_int / print_float intrinsics → runtime extern, return i64.
        if (name == "print_int" || name == "print_float")
            && !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name)
        {
            if args.len() != 1 {
                return err(format!("`{}` takes 1 argument", name), span);
            }
            let (ca, _) = self.expr(&args[0], None)?;
            return Ok((
                CoreExpr::new(CoreExprKind::Print(Box::new(ca)), Repr::Scalar(ScalarRepr::I64)),
                Ty::i64(),
            ));
        }
        // Float intrinsics — only when the name isn't shadowed by a user fn.
        if let Some(intr) = float_intrinsic(name) {
            if !self.ctx.fns.keys().any(|k| k.rsplit("::").next().unwrap() == name) {
                if args.len() != 1 {
                    return err(format!("`{}` takes 1 argument", name), span);
                }
                let (ca, at) = self.expr(&args[0], None)?;
                if !matches!(at, Ty::Prim(Prim::F32 | Prim::F64)) {
                    return err(format!("`{}` requires a float argument", name), span);
                }
                let repr = ca.repr.clone();
                return Ok((CoreExpr::new(CoreExprKind::FloatIntrinsic(intr, Box::new(ca)), repr), at));
            }
        }
        // An enum variant constructor (`Option::Some(x)`, `Ok(v)`) is a call
        // syntactically; route it to variant construction.
        if self.ctx.variant_by_path(path).is_some() {
            return self.variant_ctor(path, args, expected, span);
        }
        // Resolution order: a `use` alias for this name wins (it disambiguates
        // same-named items across modules); otherwise fall back to the unique
        // last-segment match.
        let fq = if !path.is_single() {
            let qualified = path.segments.join("::");
            self.ctx.fns.contains_key(&qualified).then_some(qualified)
        } else {
            self.ctx.use_aliases.get(name)
                .filter(|fq| self.ctx.fns.contains_key(*fq))
                .cloned()
                .or_else(|| {
                    let mut matches = self.ctx.fns.keys().filter(|k| k.rsplit("::").next() == Some(name));
                    let first = matches.next()?.clone();
                    matches.next().is_none().then_some(first)
                })
        };
        let Some(fq) = fq else {
            let names: Vec<&str> = self.ctx.fns.keys().map(|k| k.rsplit("::").next().unwrap()).collect();
            let hint = closest(name, names.into_iter())
                .map(|s| format!(" (did you mean `{}`?)", s))
                .unwrap_or_default();
            return err(format!("unknown function `{}`{}", name, hint), span);
        };
        let f = self.ctx.fns[&fq].clone();
        if f.is_unsafe {
            self.require_unsafe(&format!("calling `unsafe fn {name}`"), span)?;
        }
        if args.len() != f.params.len() {
            return err(format!("`{}` expects {} args, got {}", name, f.params.len(), args.len()), span);
        }

        if f.is_extern {
            self.require_unsafe(&format!("foreign call `{name}`"), span)?;
        }

        // Lower the argument expressions, learning each one's concrete type.
        // For a generic callee we infer the type-parameter assignment by
        // unifying declared param types (which mention `T`) against the actual
        // argument types.
        let gparams: Vec<String> = f.generics.params.iter().map(|p| p.name.clone()).collect();
        let mut targ: HashMap<String, Ty> = HashMap::new();
        // `f::<A, B>(..)`: explicit type arguments, in declaration order.
        if let Some((false, tys)) = &turbofish {
            if tys.len() > gparams.len() {
                return err(format!("`{}` takes {} type argument(s), {} given", name, gparams.len(), tys.len()), span);
            }
            for (g, t) in gparams.iter().zip(tys) {
                if !matches!(t, Ty::Var(v) if v == "_") { targ.insert(g.clone(), t.clone()); }
            }
        }
        // Seed inference from the EXPECTED return type, if any. This lets
        // `let v: Vec<i64> = Vec::new()` infer `T=i64` from the annotation when no
        // argument constrains it (the only way to type a no-arg generic ctor).
        if !gparams.is_empty() {
            if let Some(exp) = expected {
                let ret_decl = match &f.ret {
                    Some(t) => lower_type(t, &gparams, self.ctx).map_err(conv)?,
                    None => Ty::unit(),
                };
                unify_infer(&ret_decl, exp, &mut targ);
            }
        }
        // Two-pass argument lowering so type variables inferred from CONCRETE
        // arguments guide inference for var-dependent ones. Phase 1 lowers args
        // whose declared type has no free var (and args of a non-generic callee).
        // Phase 2 lowers the rest with the resolved subst as the expected hint —
        // this is what makes `unwrap_or(None, x)` infer `T` from `x`.
        let declared_tys: Vec<Ty> = f.params.iter()
            .map(|p| lower_type(&p.ty, &gparams, self.ctx).map_err(conv))
            .collect::<Result<_, _>>()?;
        // Collect independent ground constraints before real lowering so a
        // later argument can type an earlier empty array or generic call.
        // Speculative lowering rolls back all state; emitted argument effects
        // still execute once in their written order.
        if !gparams.is_empty() && self.infer.arguments {
            self.infer_argument_types(args.iter().zip(&declared_tys), &mut targ);
        }
        let mut lowered: Vec<Option<(CoreExpr, Ty)>> = (0..args.len()).map(|_| None).collect();
        // Phase 1: lower every argument that is NOT a bare enum-variant
        // constructor. Their concrete types pin type variables. (A variant
        // constructor like `None` may need a type var that only another argument
        // can supply, so it's deferred to phase 2.)
        for (i, (a, declared)) in args.iter().zip(&declared_tys).enumerate() {
            if self.is_inference_dependent_arg(a) { continue; }
            // FFI `as_c_bytes(s)` is legal ONLY as a direct argument to an extern
            // call whose corresponding parameter is `RawPtr`. Intercept it here;
            // any other use falls through to `self.expr`, which rejects it.
            if f.is_extern {
                let param_is_mut = f.params.get(i).map(|p| p.is_mut).unwrap_or(false);
                if let Some((ca, at)) = self.try_as_c_bytes_arg(a, declared, param_is_mut)? {
                    unify_infer(declared, &at, &mut targ);
                    lowered[i] = Some((ca, at));
                    continue;
                }
                if let Some((ca, at)) = self.try_callback_arg(a, declared)? {
                    lowered[i] = Some((ca, at));
                    continue;
                }
            }
            let hinted = apply_subst(declared, &targ);
            let (ca, at) = self.expr(a, hint(&hinted))?;
            unify_infer(declared, &at, &mut targ);
            lowered[i] = Some((ca, at));
        }
        // Phase 2: the deferred (variant-constructor) args, now with the type
        // vars inferred from phase 1 substituted into their expected type.
        for (i, (a, declared)) in args.iter().zip(&declared_tys).enumerate() {
            if lowered[i].is_some() { continue; }
            let hinted = apply_subst(declared, &targ);
            let (ca, at) = self.expr(a, hint(&hinted))?;
            unify_infer(declared, &at, &mut targ);
            lowered[i] = Some((ca, at));
        }
        let mut cargs = Vec::new();
        let mut arg_tys = Vec::new();
        for (i, slot) in lowered.into_iter().enumerate() {
            let (ca, at) = slot.expect("every argument lowered");
            // Carry each argument's own span so a type error underlines that
            // argument, not the whole call.
            arg_tys.push((/* declared placeholder */ at.clone(), at, args[i].span));
            cargs.push(ca);
        }
        // Re-pair arg_tys with their declared types for the assignability check.
        for (slot, declared) in arg_tys.iter_mut().zip(&declared_tys) {
            slot.0 = declared.clone();
        }

        self.infer_from_bindings(&f.generics, &gparams, &mut targ)?;
        // Resolve the instantiation's concrete type args (in declared order).
        let mut inst_args = Vec::new();
        for gp in &gparams {
            match targ.get(gp) {
                Some(t) => inst_args.push(t.clone()),
                None => {
                    if let Some(ret) = &f.ret {
                        let declared = lower_type(ret, &gparams, self.ctx).map_err(conv)?;
                        self.infer.unresolved = Some(apply_subst(&declared, &targ));
                    }
                    return err(format!("cannot infer type parameter `{}` for `{}`", gp, name), span);
                }
            }
        }

        self.check_generic_bounds(&f.generics, &gparams, &targ, name, span)?;

        // Check each argument against its substituted declared type, pointing the
        // error at the argument's own span.
        let subst: HashMap<String, Ty> = gparams.iter().cloned().zip(inst_args.iter().cloned()).collect();
        for (declared, actual, arg_span) in &arg_tys {
            self.check_assignable(actual, &apply_subst(declared, &subst), *arg_span)?;
        }

        let mangled = mangle(&fq, &inst_args);
        let job = Job { f: f.clone(), subst: subst.clone(), mangled, self_ty: None };
        let fid = self.mono.intern(job.clone(), self.count);

        let ret_ty = match &f.ret {
            Some(t) => {
                let raw = lower_type(t, &gparams, self.ctx).map_err(conv)?;
                apply_subst(&raw, &subst)
            }
            None => Ty::unit(),
        };
        let ret_ty = self.opaque_result(&job, ret_ty, span)?;
        let repr = self.repr_of(&ret_ty, span)?;
        Ok((CoreExpr::new(CoreExprKind::Call(fid, cargs), repr), ret_ty))
    }
}

/// A `Some(ty)` hint only when the declared type is already concrete (no free
/// type variables) — otherwise literal defaulting shouldn't be guided by `T`.
/// Compound literals can infer their variables while retaining array shape.
fn compound_hint(declared: &Ty) -> Option<&Ty> {
    match declared {
        Ty::Tuple(_) | Ty::Array(..) => Some(declared),
        Ty::Named { name, .. } if name == "Array" => Some(declared),
        _ => hint(declared),
    }
}

fn hint(declared: &Ty) -> Option<&Ty> {
    if let Ty::Fn { params, .. } = declared {
        if params.iter().all(|param| !has_var(param)) { return Some(declared); }
    }
    if has_var(declared) { None } else { Some(declared) }
}

pub(crate) fn has_var(t: &Ty) -> bool {
    match t {
        Ty::Var(_) => true,
        Ty::Named { args, .. } | Ty::Tuple(args) => args.iter().any(has_var),
        Ty::Array(e, _) => has_var(e),
        Ty::Fn { params, ret } | Ty::ExternFn { params, ret } => params.iter().any(has_var) || has_var(ret),
        Ty::Prim(_) => false,
        // A `let` placeholder is an unknown; the never type is not.
        Ty::Infer(n) => *n != u32::MAX,
    }
}

/// Unify a declared type (which may contain `Ty::Var`) against a concrete
/// actual type, recording type-variable assignments into `out`.
/// Seed only known parts of an alias receiver. Its free parameters belong to
/// the alias, so they must never enter the callee's substitution map.
fn infer_concrete_parts(declared: &Ty, target: &Ty, out: &mut HashMap<String, Ty>) {
    if !has_var(target) {
        unify_infer(declared, target, out);
        return;
    }
    match (declared, target) {
        (Ty::Named { name: left, args: da }, Ty::Named { name: right, args: ta }) if left == right => {
            for (d, t) in da.iter().zip(ta) { infer_concrete_parts(d, t, out); }
        }
        (Ty::Tuple(da), Ty::Tuple(ta)) => {
            for (d, t) in da.iter().zip(ta) { infer_concrete_parts(d, t, out); }
        }
        (Ty::Array(d, dn), Ty::Array(t, tn)) if dn == tn => infer_concrete_parts(d, t, out),
        (Ty::Fn { params: dp, ret: dr }, Ty::Fn { params: tp, ret: tr })
        | (Ty::ExternFn { params: dp, ret: dr }, Ty::ExternFn { params: tp, ret: tr }) => {
            for (d, t) in dp.iter().zip(tp) { infer_concrete_parts(d, t, out); }
            infer_concrete_parts(dr, tr, out);
        }
        _ => {}
    }
}

fn unify_infer(declared: &Ty, actual: &Ty, out: &mut HashMap<String, Ty>) {
    match (declared, actual) {
        (Ty::Var(v), _) => { out.entry(v.clone()).or_insert_with(|| actual.clone()); }
        (Ty::Named { args: da, .. }, Ty::Named { args: aa, .. }) => {
            for (d, a) in da.iter().zip(aa) { unify_infer(d, a, out); }
        }
        (Ty::Tuple(de), Ty::Tuple(ae)) => {
            for (d, a) in de.iter().zip(ae) { unify_infer(d, a, out); }
        }
        (Ty::Array(de, _), Ty::Array(ae, _)) => unify_infer(de, ae, out),
        // `[T; N]` (const `N`) against a concrete array binds both.
        (Ty::Named { name, args }, Ty::Array(ae, n)) if name == "array#" && args.len() == 2 => {
            unify_infer(&args[0], ae, out);
            unify_infer(&args[1], &Ty::Named { name: format!("const#{n}"), args: vec![] }, out);
        }
        (Ty::Fn { params: dp, ret: dr }, Ty::Fn { params: ap, ret: ar }) => {
            for (d, a) in dp.iter().zip(ap) { unify_infer(d, a, out); }
            unify_infer(dr, ar, out);
        }
        _ => {}
    }
}

// ---- helpers ---------------------------------------------------------------

/// A pattern normalized for the exhaustiveness check: a wildcard, a
/// constructor (enum variant tag, `false`/`true`, or the single constructor
/// of a tuple or struct) with sub-patterns, or a literal of an unbounded type.
#[derive(Clone)]
enum SpacePat {
    Wild,
    Ctor(u32, Vec<SpacePat>),
    Lit(String),
}

/// Rows whose head matches constructor `c`, with the head replaced by its
/// `arity` sub-patterns (a wildcard head expands to wildcards).
fn specialize_ctor(rows: &[Vec<SpacePat>], c: u32, arity: usize) -> Vec<Vec<SpacePat>> {
    rows.iter().filter_map(|r| {
        let mut out = match &r[0] {
            SpacePat::Ctor(d, subs) if *d == c => subs.clone(),
            SpacePat::Wild => vec![SpacePat::Wild; arity],
            _ => return None,
        };
        out.extend_from_slice(&r[1..]);
        Some(out)
    }).collect()
}

/// Names for a `for` loop's hidden state. `#` cannot appear in a source
/// identifier, so these never collide with (or are visible to) user code.
const FOR_INDEX: &str = "for#index";
const FOR_END: &str = "for#end";
const FOR_DONE: &str = "for#done";
const FOR_ITEMS: &str = "for#items";
/// The evaluated base of a range index `base[lo..hi]`.
const RANGE_BASE: &str = "range#base";

/// Desugar `for pat in iter { body }` to a `while` loop.
///
/// - `lo..hi` / `lo..=hi`: the bounds are evaluated once. The loop variable is
///   a fresh binding per iteration, and the counter advances before the body
///   runs, so `continue` and a body that assigns the variable are both safe.
///   An inclusive range stops after `hi` without computing `hi + 1`.
/// - anything else is a collection with `len()` and indexing (`Vec`, `Array`):
///   it is evaluated once and its elements are visited in index order.
fn desugar_for(pat: &Pattern, iter: &Expr, body: &Block, sp: Span) -> LResult<Expr> {
    let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span: sp };
    let var = |name: &str| ex(ExprKind::Path(Path::single(name.to_string(), sp)));
    let int = |n: u64| ex(ExprKind::Int(n, crate::lexer::NumSuffix::None));
    let let_ = |name: &str, is_mut: bool, init: Expr| Stmt::Let {
        pattern: Pattern { kind: PatternKind::Binding { is_mut, name: name.to_string() }, span: sp },
        ty: None,
        init: Some(init),
        span: sp,
    };
    let assign = |name: &str, value: Expr| Stmt::Expr(ex(ExprKind::Assign { target: var(name), op: None, value }));
    let binding = pat.clone();
    let bind_item = |value: Expr| Stmt::Let { pattern: binding.clone(), ty: None, init: Some(value), span: sp };
    // The user's body runs as its own block, so its tail value is discarded
    // and its bindings stay local to one iteration.
    let user_body = Stmt::Expr(ex(ExprKind::Block(body.clone())));
    let block = |stmts: Vec<Stmt>| Block { stmts, tail: None, span: sp };

    let stmts = match &*iter.kind {
        ExprKind::Range { lo: Some(lo), hi: Some(hi), inclusive: false } => vec![
            let_(FOR_INDEX, true, lo.clone()),
            let_(FOR_END, false, hi.clone()),
            Stmt::Expr(ex(ExprKind::While {
                cond: ex(ExprKind::Binary(BinOp::Lt, var(FOR_INDEX), var(FOR_END))),
                body: block(vec![
                    bind_item(var(FOR_INDEX)),
                    assign(FOR_INDEX, ex(ExprKind::Binary(BinOp::Add, var(FOR_INDEX), int(1)))),
                    user_body,
                ]),
            })),
        ],
        ExprKind::Range { lo: Some(lo), hi: Some(hi), inclusive: true } => vec![
            let_(FOR_INDEX, true, lo.clone()),
            let_(FOR_END, false, hi.clone()),
            let_(FOR_DONE, true, ex(ExprKind::Binary(BinOp::Gt, var(FOR_INDEX), var(FOR_END)))),
            Stmt::Expr(ex(ExprKind::While {
                cond: ex(ExprKind::Unary(UnOp::Not, var(FOR_DONE))),
                body: block(vec![
                    bind_item(var(FOR_INDEX)),
                    Stmt::Expr(ex(ExprKind::If {
                        cond: ex(ExprKind::Binary(BinOp::Eq, var(FOR_INDEX), var(FOR_END))),
                        then_branch: block(vec![assign(FOR_DONE, ex(ExprKind::Bool(true)))]),
                        else_branch: Some(ex(ExprKind::Block(block(vec![
                            assign(FOR_INDEX, ex(ExprKind::Binary(BinOp::Add, var(FOR_INDEX), int(1)))),
                        ])))),
                    })),
                    user_body,
                ]),
            })),
        ],
        ExprKind::Range { .. } => return err("a `for` range needs both bounds (`lo..hi` or `lo..=hi`)", iter.span),
        _ => vec![
            let_(FOR_ITEMS, false, iter.clone()),
            let_(FOR_INDEX, true, int(0)),
            Stmt::Expr(ex(ExprKind::While {
                cond: ex(ExprKind::Binary(
                    BinOp::Lt,
                    var(FOR_INDEX),
                    ex(ExprKind::MethodCall { recv: var(FOR_ITEMS), method: "len".into(), args: vec![], span: sp }),
                )),
                body: block(vec![
                    bind_item(ex(ExprKind::Index { base: var(FOR_ITEMS), index: var(FOR_INDEX) })),
                    assign(FOR_INDEX, ex(ExprKind::Binary(BinOp::Add, var(FOR_INDEX), int(1)))),
                    user_body,
                ]),
            })),
        ],
    };
    Ok(ex(ExprKind::Block(block(stmts))))
}

enum PrimConst { Int(i128), Float(f64) }

/// The associated constants of the primitive types (`i64::MAX`,
/// `u8::MIN`, `f64::EPSILON`, `f64::consts::PI`), with the literal suffix
/// that gives them their type.
fn primitive_const(segments: &[String]) -> Option<(crate::lexer::NumSuffix, PrimConst)> {
    use crate::lexer::NumSuffix as S;
    let (ty, name) = match segments {
        [ty, name] => (ty.as_str(), name.as_str()),
        [ty, consts, name] if consts == "consts" => (ty.as_str(), name.as_str()),
        _ => return None,
    };
    let int = |suffix: S, min: i128, max: i128| -> Option<(S, PrimConst)> {
        Some((suffix, PrimConst::Int(match name { "MIN" => min, "MAX" => max, "BITS" => return Some((S::U32, PrimConst::Int((max - min + 1).ilog2() as i128))), _ => return None })))
    };
    match ty {
        "i8" => int(S::I8, i8::MIN as i128, i8::MAX as i128),
        "i16" => int(S::I16, i16::MIN as i128, i16::MAX as i128),
        "i32" => int(S::I32, i32::MIN as i128, i32::MAX as i128),
        "i64" | "isize" => int(S::I64, i64::MIN as i128, i64::MAX as i128),
        "usize" => int(S::I64, 0, i64::MAX as i128),
        "u8" => int(S::U8, 0, u8::MAX as i128),
        "u16" => int(S::U16, 0, u16::MAX as i128),
        "u32" => int(S::U32, 0, u32::MAX as i128),
        "u64" => int(S::U64, 0, u64::MAX as i128),
        "f32" | "f64" => {
            let suffix = if ty == "f32" { S::F32 } else { S::F64 };
            let wide = ty == "f64";
            let v = match name {
                "INFINITY" => f64::INFINITY,
                "NEG_INFINITY" => f64::NEG_INFINITY,
                "NAN" => f64::NAN,
                "MAX" => if wide { f64::MAX } else { f32::MAX as f64 },
                "MIN" => if wide { f64::MIN } else { f32::MIN as f64 },
                "MIN_POSITIVE" => if wide { f64::MIN_POSITIVE } else { f32::MIN_POSITIVE as f64 },
                "EPSILON" => if wide { f64::EPSILON } else { f32::EPSILON as f64 },
                "PI" => std::f64::consts::PI,
                "TAU" => std::f64::consts::TAU,
                "E" => std::f64::consts::E,
                "SQRT_2" => std::f64::consts::SQRT_2,
                "LN_2" => std::f64::consts::LN_2,
                "LN_10" => std::f64::consts::LN_10,
                "FRAC_PI_2" => std::f64::consts::FRAC_PI_2,
                _ => return None,
            };
            Some((suffix, PrimConst::Float(v)))
        }
        _ => None,
    }
}

/// `for pat in it { body }` over an iterator:
/// `{ let mut for#iter = it; loop { match for#iter.next() { Some(pat) => { body } _ => break } } }`.
fn desugar_for_iterator(pat: &Pattern, iter: &Expr, body: &Block, sp: Span) -> Expr {
    let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span: sp };
    let var = || ex(ExprKind::Path(Path::single("for#iter".to_string(), sp)));
    let next = ex(ExprKind::MethodCall { recv: var(), method: "next".into(), args: vec![], span: sp });
    let some = Pattern { kind: PatternKind::Variant { path: Path { segments: vec!["Option".into(), "Some".into()], span: sp }, payload: vec![pat.clone()] }, span: sp };
    let arms = vec![
        MatchArm { pattern: some, guard: None, body: ex(ExprKind::Block(body.clone())), span: sp },
        MatchArm { pattern: Pattern { kind: PatternKind::Wildcard, span: sp }, guard: None, body: ex(ExprKind::Break(None)), span: sp },
    ];
    let lp = ex(ExprKind::Loop { body: Block { stmts: vec![Stmt::Expr(ex(ExprKind::Match { scrutinee: next, arms }))], tail: None, span: sp } });
    let init = Stmt::Let {
        pattern: Pattern { kind: PatternKind::Binding { is_mut: true, name: "for#iter".to_string() }, span: sp },
        ty: None,
        init: Some(iter.clone()),
        span: sp,
    };
    ex(ExprKind::Block(Block { stmts: vec![init, Stmt::Expr(lp)], tail: None, span: sp }))
}

fn block_of(e: CoreExpr) -> CoreBlock {
    CoreBlock { stmts: vec![], tail: Some(e) }
}

/// Generic parameter names of the built-in `Option`/`Result` enums (used only
/// when they aren't user-declared; example programs declare their own).
fn builtin_enum_params(name: &str) -> Vec<String> {
    match name {
        "Option" => vec!["T".into()],
        "Result" => vec!["T".into(), "E".into()],
        _ => vec![],
    }
}

fn negate(e: CoreExpr) -> CoreExpr {
    let repr = e.repr.clone();
    CoreExpr::new(CoreExprKind::Un(UnOp::Not, Box::new(e)), repr)
}

fn scalar_of(p: Prim) -> Option<ScalarRepr> {
    Some(match p {
        Prim::I8 => ScalarRepr::I8, Prim::I16 => ScalarRepr::I16,
        Prim::I32 => ScalarRepr::I32, Prim::I64 => ScalarRepr::I64,
        Prim::U8 => ScalarRepr::U8, Prim::U16 => ScalarRepr::U16,
        Prim::U32 => ScalarRepr::U32, Prim::U64 => ScalarRepr::U64,
        Prim::F32 => ScalarRepr::F32, Prim::F64 => ScalarRepr::F64,
        Prim::Bool => ScalarRepr::Bool, Prim::Char => ScalarRepr::Char,
        Prim::RawPtr => ScalarRepr::Ptr,
        Prim::Str | Prim::Unit => return None,
    })
}

fn is_int_prim(p: Prim) -> bool {
    matches!(p, Prim::I8 | Prim::I16 | Prim::I32 | Prim::I64 | Prim::U8 | Prim::U16 | Prim::U32 | Prim::U64)
}
fn is_numeric(ty: &Ty) -> bool {
    matches!(ty, Ty::Prim(p) if is_int_prim(*p) || matches!(p, Prim::F32 | Prim::F64))
}

/// The "never" type, produced by diverging expressions (`return`, and a `loop`
/// with no value-carrying `break`). It is assignable to ANY expected type — a
/// `loop {}` whose only exits are `return` can stand as a function body of any
/// return type. Encoded as a reserved `Ty::Infer` sentinel to avoid a new `Ty`
/// variant rippling through every match. See `check_assignable` / `is_never`.
fn never_ty() -> Ty { Ty::Infer(u32::MAX) }
fn is_never(t: &Ty) -> bool { matches!(t, Ty::Infer(n) if *n == u32::MAX) }

fn check_assignable(actual: &Ty, expected: &Ty, span: Span) -> LResult<()> {
    if is_never(actual) || actual == expected || actual.is_unit() && expected.is_unit() {
        Ok(())
    } else {
        err(format!("type mismatch: expected `{}`, found `{}`", ty_display(expected), ty_display(actual)), span)
    }
}

fn disc(e: &ExprKind) -> &'static str {
    match e {
        ExprKind::MethodCall { .. } => "method-call",
        ExprKind::Field { .. } => "field",
        ExprKind::Index { .. } => "index",
        ExprKind::StructLit { .. } => "struct-literal",
        ExprKind::Match { .. } => "match",
        ExprKind::While { .. } => "while",
        ExprKind::Loop { .. } => "loop",
        ExprKind::For { .. } => "for",
        ExprKind::Closure { .. } => "closure",
        ExprKind::Try(_) => "try",
        _ => "expr",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse_module;
    use crate::resolve::resolve_module;

    fn lower(src: &str) -> Result<CoreProgram, LowerError> {
        let m = parse_module(&lex(src).unwrap()).unwrap();
        let r = resolve_module(m).unwrap();
        lower_program(&r.globals)
    }

    #[test]
    fn type_probe_cache_respects_context_identity_and_strict_errors() {
        let module = parse_module(&lex("extern \"C\" fn native() -> i64; fn main()->i64{0}").unwrap()).unwrap();
        let resolved = resolve_module(module).unwrap();
        let ctx = TyCtx::from_globals(&resolved.globals);
        let mut reg = LayoutRegistry::new(&ctx);
        let mut mono = Mono::new();
        let mut count = 0;
        let mut lo = FnLowerer {
            ctx: &ctx, reg: &mut reg, mono: &mut mono, count: &mut count,
            subst: HashMap::new(), scope: vec![HashMap::new()],
            locals: vec![], local_names: vec![], local_tys: vec![],
            ret_ty: Ty::Prim(Prim::I64), infer_ret: false,
            infer: LetInference::default(), unsafe_depth: 0,
            deferred_locals: vec![], loop_exits: vec![], type_probe: false,
            probe_cache: Rc::new(std::cell::RefCell::new(HashMap::new())),
        };
        let span = Span::new(0, 1);
        let expr = |kind| Expr { kind: std::sync::Arc::new(kind), span };
        let variable = expr(ExprKind::Path(Path::single("x".into(), span)));
        reset_probe_work();
        assert!(lo.probe_expr_type(&variable, None).is_err());
        let misses = probe_work();
        assert!(lo.probe_expr_type(&variable.clone(), None).is_err());
        assert_eq!(probe_work(), misses, "identical failed probes should be reused");
        let id = lo.fresh_local(Repr::Scalar(ScalarRepr::Bool), Ty::Prim(Prim::Bool));
        lo.bind("x", id, Ty::Prim(Prim::Bool));
        assert_eq!(lo.probe_expr_type(&variable, None).unwrap(), Ty::Prim(Prim::Bool));
        lo.push_scope();
        let shadow = lo.fresh_local(Repr::Scalar(ScalarRepr::I64), Ty::Prim(Prim::I64));
        lo.bind("x", shadow, Ty::Prim(Prim::I64));
        assert_eq!(lo.probe_expr_type(&variable, None).unwrap(), Ty::Prim(Prim::I64));
        lo.pop_scope();
        assert_eq!(lo.probe_expr_type(&variable, None).unwrap(), Ty::Prim(Prim::Bool));

        let empty = expr(ExprKind::Array(ArrayLit::Elems(vec![])));
        for element in [Prim::Bool, Prim::I64] {
            let expected = Ty::Named { name: "Array".into(), args: vec![Ty::Prim(element)] };
            assert_eq!(lo.probe_expr_type(&empty, Some(&expected)).unwrap(), expected);
        }
        let assign = expr(ExprKind::Assign { target: variable.clone(), op: None,
            value: expr(ExprKind::Bool(true)) });
        assert!(lo.probe_expr_type(&assign, None).is_err());
        lo.bind_mut("x", id, Ty::Prim(Prim::Bool), true);
        assert!(lo.probe_expr_type(&assign, None).is_ok());

        let call = expr(ExprKind::Call(expr(ExprKind::Path(Path::single("native".into(), span))), vec![]));
        assert!(lo.probe_expr_type(&call, None).unwrap_err().msg.contains("unsafe"));
        lo.unsafe_depth = 1;
        assert_eq!(lo.probe_expr_type(&call, None).unwrap(), Ty::Prim(Prim::I64));
        lo.unsafe_depth = 0;
        assert!(lo.probe_expr_type(&call, None).is_err());
        assert_eq!(*lo.count, 0, "probing must restore monomorphization state");
        assert!(lo.mono.ids.is_empty());

        let mut literal = expr(ExprKind::Bool(true));
        let original = literal.clone();
        assert_eq!(lo.probe_expr_type(&literal, None).unwrap(), Ty::Prim(Prim::Bool));
        *std::sync::Arc::make_mut(&mut literal.kind) = ExprKind::Str("text".into());
        assert_eq!(lo.probe_expr_type(&literal, None).unwrap(), Ty::Prim(Prim::Str));
        assert_eq!(lo.probe_expr_type(&original, None).unwrap(), Ty::Prim(Prim::Bool));
        assert!(lo.reg.layouts.is_empty(), "string layout created by a probe must be rolled back");

        lo.infer.probing = true;
        let unknown = expr(ExprKind::Path(Path::single("unknown".into(), span)));
        let block = expr(ExprKind::Block(Block { stmts: vec![Stmt::Expr(unknown)],
            tail: Some(expr(ExprKind::Bool(true))), span }));
        assert!(lo.probe_tolerates(), "outer let-discovery still tolerates unrelated errors");
        assert!(lo.probe_expr_type(&block, None).is_err(), "type-only probing must not skip an error");
        assert!(lo.probe_tolerates(), "strictness must be restored after a probe");
    }

    #[test]
    fn lowers_fib() {
        let prog = lower(include_str!("../examples/fib.gcr")).unwrap();
        assert!(prog.entry.is_some());
        // fib + main
        assert_eq!(prog.funcs.len(), 2);
        let main = &prog.funcs[prog.entry.unwrap() as usize];
        assert_eq!(main.ret, Repr::Scalar(ScalarRepr::I64));
    }

    #[test]
    fn arithmetic_and_if() {
        let prog = lower("fn main() -> i64 { let x = 3; if x < 5 { x * 2 } else { 0 } }").unwrap();
        assert_eq!(prog.funcs.len(), 1);
    }

    #[test]
    fn type_mismatch_caught() {
        let e = lower("fn main() -> i64 { true }");
        assert!(e.is_err());
    }

    #[test]
    fn unsigned_literal() {
        let prog = lower("fn unsigned() -> u32 { let x = 5u32; x + 1 } fn main() -> i64 { unsigned() as i64 }").unwrap();
        let unsigned = prog.funcs.iter().find(|f| f.name == "unsigned").unwrap();
        assert_eq!(unsigned.ret, Repr::Scalar(ScalarRepr::U32));
    }

    // ---- Mutability discipline (docs/mutability.md) ----------------------
    // Immutable by default for ALL values; `mut` is required at every binder.

    fn err_msg(src: &str) -> String {
        lower(src).err().expect("expected a lowering error").msg
    }

    #[test]
    fn associated_fn_resolves() {
        // `Type::func` with no `self` resolves to the impl's associated function.
        assert!(lower(
            "struct Foo { x: i64 } \
             impl Foo { fn make(n: i64) -> Foo { Foo { x: n } } fn get(self) -> i64 { self.x } } \
             fn main() -> i64 { let f = Foo::make(42); f.get() }",
        ).is_ok());
    }

    #[test]
    fn generic_associated_fn_infers_from_arg() {
        // `Box::wrap(99)` infers `T = i64` from the argument.
        assert!(lower(
            "struct Box<T> { val: T } \
             impl<T> Box<T> { fn wrap(x: T) -> Box<T> { Box { val: x } } fn get(self) -> T { self.val } } \
             fn main() -> i64 { let b = Box::wrap(99); b.get() }",
        ).is_ok());
    }

    #[test]
    fn reassign_immutable_let_rejected() {
        let m = err_msg("fn main() -> i64 { let x = 5; x = 6; x }");
        assert!(m.contains("immutable"), "{m}");
    }

    #[test]
    fn reassign_let_mut_ok() {
        assert!(lower("fn main() -> i64 { let mut x = 5; x = 6; x }").is_ok());
    }

    #[test]
    fn field_assign_through_immutable_rejected() {
        let m = err_msg(
            "struct P { x: i64 } fn main() -> i64 { let p = P { x: 1 }; p.x = 9; p.x }",
        );
        assert!(m.contains("immutable"), "{m}");
    }

    #[test]
    fn field_assign_through_mut_ok() {
        assert!(lower(
            "struct P { x: i64 } fn main() -> i64 { let mut p = P { x: 1 }; p.x = 9; p.x }",
        )
        .is_ok());
    }

    #[test]
    fn mut_self_method_on_immutable_receiver_rejected() {
        let m = err_msg(
            "struct C { n: i64 } \
             impl C { fn bump(mut self) -> i64 { self.n = self.n + 1; self.n } } \
             fn main() -> i64 { let c = C { n: 0 }; c.bump() }",
        );
        assert!(m.contains("immutable"), "{m}");
    }

    #[test]
    fn mut_self_method_on_mut_receiver_ok() {
        assert!(lower(
            "struct C { n: i64 } \
             impl C { fn bump(mut self) -> i64 { self.n = self.n + 1; self.n } } \
             fn main() -> i64 { let mut c = C { n: 0 }; c.bump() }",
        )
        .is_ok());
    }

    #[test]
    fn plain_self_cannot_mutate_fields() {
        let m = err_msg(
            "struct C { n: i64 } \
             impl C { fn bad(self) -> i64 { self.n = self.n + 1; self.n } } \
             fn main() -> i64 { let mut c = C { n: 0 }; c.bad() }",
        );
        assert!(m.contains("mut self"), "{m}");
    }

    #[test]
    fn assign_through_immutable_param_rejected() {
        let m = err_msg(
            "struct P { x: i64 } fn set(p: P) -> i64 { p.x = 5; p.x } \
             fn main() -> i64 { let q = P { x: 1 }; set(q) }",
        );
        assert!(m.contains("immutable"), "{m}");
    }

    #[test]
    fn assign_through_mut_param_ok() {
        assert!(lower(
            "struct P { x: i64 } fn set(mut p: P) -> i64 { p.x = 5; p.x } \
             fn main() -> i64 { let q = P { x: 1 }; set(q) }",
        )
        .is_ok());
    }

    // ---- Phase 2: match completion, deferred let, tuple immutability ------

    #[test]
    fn scalar_match_without_wildcard_rejected() {
        let m = err_msg("fn f(n: i64) -> i64 { match n { 0 => 1, 1 => 2 } } fn main() -> i64 { f(0) }");
        assert!(m.contains("non-exhaustive"), "{m}");
    }

    #[test]
    fn enum_missing_variant_rejected() {
        let m = err_msg("enum E { A, B, C } fn f(e: E) -> i64 { match e { E::A => 1, E::B => 2 } } fn main() -> i64 { f(E::A) }");
        assert!(m.contains("non-exhaustive"), "{m}");
    }

    #[test]
    fn guarded_variant_does_not_cover() {
        // `E::B if ...` is guarded, so it doesn't make B covered: still non-exhaustive.
        let m = err_msg("enum E { A, B } fn f(e: E) -> i64 { match e { E::A => 1, E::B if true => 2 } } fn main() -> i64 { f(E::A) }");
        assert!(m.contains("non-exhaustive"), "{m}");
    }

    #[test]
    fn immutable_deferred_let_accepted() {
        // Rust's `let x; ... x = v;` (the binding is assigned later).
        assert!(lower("fn main() -> i64 { let x: i64; x = 1; x }").is_ok());
        assert!(lower("fn main() -> i64 { let x; if true { x = 1; } else { x = 2; } x }").is_ok());
    }

    #[test]
    fn deferred_let_needs_an_assignment_to_infer_from() {
        let m = err_msg("fn main() -> i64 { let mut x; 0 }");
        assert!(m.contains("annotation"), "{m}");
    }

    #[test]
    fn tuple_field_assignment_rejected() {
        let m = err_msg("fn main() -> i64 { let mut t = (3, 4); t.0 = 9; t.0 }");
        assert!(m.contains("immutable") || m.contains("rebuild"), "{m}");
    }

    // ---- Phase 5: diagnostics quality ------------------------------------

    #[test]
    fn unknown_variable_suggests_close_name() {
        let m = err_msg("fn main() -> i64 { let count = 5; cont + 1 }");
        assert!(m.contains("did you mean `count`?"), "{m}");
    }

    #[test]
    fn unknown_function_suggests_close_name() {
        let m = err_msg("fn compute() -> i64 { 5 } fn main() -> i64 { comput() }");
        assert!(m.contains("did you mean `compute`?"), "{m}");
    }

    #[test]
    fn no_suggestion_when_nothing_close() {
        // A wildly different name shouldn't get a bogus suggestion.
        let m = err_msg("fn main() -> i64 { let count = 5; zzzzzzzz + 1 }");
        assert!(!m.contains("did you mean"), "{m}");
    }

    #[test]
    fn arg_type_error_points_at_argument() {
        // The error span should be the argument `true`, not the whole call. We
        // can't see the span text here, but lower returns the span; verify the
        // message is the arg type mismatch (the span fix is covered in tests/).
        let e = lower("fn f(x: i64) -> i64 { x } fn main() -> i64 { f(true) }").err().unwrap();
        assert!(e.msg.contains("expected `i64`, found `bool`"), "{}", e.msg);
        // The span must be inside the `true` token region, not span 0.
        assert!(e.span.start > 0);
    }

    #[test]
    fn edit_distance_works() {
        assert_eq!(edit_distance("count", "cont"), 1);
        assert_eq!(edit_distance("vec_push", "vec_pushh"), 1);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "xyz"), 3);
    }

    // ---- multiple-error reporting (check_program) ------------------------

    fn check_errs(src: &str) -> Vec<LowerError> {
        let m = parse_module(&lex(src).unwrap()).unwrap();
        let r = resolve_module(m).unwrap();
        check_program(&r.globals).err().unwrap_or_default()
    }

    #[test]
    fn check_program_collects_all_errors() {
        // Three independently-broken functions — all three should be reported,
        // not just the first.
        let src = "fn a() -> i64 { true } \
                   fn b() -> i64 { nope + 1 } \
                   fn c() -> i64 { let x = 5; x = 6; x } \
                   fn main() -> i64 { 0 }";
        let errs = check_errs(src);
        assert_eq!(errs.len(), 3, "expected 3 errors, got: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>());
    }

    #[test]
    fn check_program_ok_when_clean() {
        let src = "fn a() -> i64 { 1 } fn b() -> i64 { a() + 1 } fn main() -> i64 { b() }";
        assert!(check_errs(src).is_empty());
    }

    #[test]
    fn check_program_skips_generics_but_catches_concrete() {
        // A generic fn is skipped by the eager pass (needs instantiation), but a
        // broken concrete fn is still caught.
        let src = "fn id<T>(x: T) -> T { x } \
                   fn broken() -> i64 { true } \
                   fn main() -> i64 { id(0) }";
        let errs = check_errs(src);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].msg.contains("found `bool`"));
    }
}
