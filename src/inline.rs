//! Inlining over Core IR.
//!
//! A call to a small function, a `#[inline]` function, a function called
//! from exactly one place, or one with few call sites called from a small
//! function's own loop (a dispatch loop) is replaced by a copy of the callee's body: its
//! parameters become locals bound to the arguments, its locals are renumbered
//! into the caller's, and each `return` becomes a `Leave` of the `Once` block
//! that holds the copy. A recursive function is inlined only at its one call
//! site, never into itself. `#[inline(never)]` and `#[cold]` functions,
//! closures and foreign functions are never inlined.
//!
//! Functions are processed callees first, so a body is copied with its own
//! calls already inlined. Inlining exposes allocations that never leave the
//! caller to scalar replacement (`crate::sroa`).

use crate::ast::InlineHint;
use crate::core::*;
use std::cell::Cell;
use std::collections::HashMap;

/// Always inline a callee of at most this many nodes.
const SMALL: usize = 40;
/// Inline a callee given a known enum variant if, with its matches on that
/// variant decided, it is at most this many nodes (`a == Tok::Eof` calls
/// `Tok::eq`, which then compares one tag).
const FOLDED: usize = 24;
/// The size limit for a `#[inline]` callee, or one given an object the
/// caller allocated (which inlining may let `crate::sroa` replace).
const HINTED: usize = 400;
/// The size limit for a callee with one call site (inlining it copies
/// nothing, so this only bounds the caller's size).
const SINGLE_SITE: usize = 20000;
/// Stop growing a caller past this many nodes (except `#[inline(always)]`).
const CALLER_LIMIT: usize = 60000;
/// A caller of at most this many nodes whose own loop calls a function with
/// at most `LOOP_SITES` call sites gets a copy of it, however large: a
/// dispatch loop (an interpreter's `loop { step() }`) then skips a call,
/// its frame and its result's encoding on every iteration.
const LOOP_CALLER: usize = 400;
const LOOP_SITES: usize = 4;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(true) };
}

/// Enable or disable inlining and scalar replacement (off for full-debug
/// builds, whose debug info describes each source function as its own frame
/// and each object as a heap object).
pub fn set_enabled(on: bool) {
    ENABLED.with(|e| e.set(on));
}

pub(crate) fn enabled() -> bool {
    ENABLED.with(|e| e.get())
}

pub fn inline_program(prog: &mut CoreProgram) {
    if !ENABLED.with(|e| e.get()) { return; }
    let n = prog.funcs.len();
    let mut calls: Vec<Vec<FuncId>> = vec![Vec::new(); n];
    let mut sites = vec![0usize; n];
    for (id, f) in prog.funcs.iter().enumerate() {
        if f.is_extern { continue; }
        visit_block(&f.body, &mut |e| if let CoreExprKind::Call(g, _) = e.kind.as_ref() {
            calls[id].push(*g);
            sites[*g as usize] += 1;
        });
    }
    let recursive = recursive_functions(&calls);
    let entry = prog.entry;
    let candidate: Vec<bool> = prog.funcs.iter().enumerate().map(|(id, f)| {
        !f.is_extern && !f.is_closure && f.closure_captures.is_empty()
            && f.hints.inline != InlineHint::Never && !f.hints.cold
            && Some(id as FuncId) != entry
    }).collect();
    let mut sizes: Vec<usize> = prog.funcs.iter().map(|f| block_size(&f.body)).collect();
    for id in postorder(&calls) {
        if prog.funcs[id].is_extern { continue; }
        let mut caller = std::mem::replace(&mut prog.funcs[id], placeholder());
        let local_objects = local_objects(&caller.body, caller.locals.len());
        let known = known_variants(&caller);
        let closures = known_closures(&caller);
        let defs = definition_counts(&caller);
        let mut cx = Inliner { funcs: &prog.funcs, candidate: &candidate, recursive: &recursive, caller: id, sizes: &sizes, sites: &sites, size: sizes[id], caller_size: sizes[id], loops: 0, label: max_label(&caller.body) + 1, local_objects, known, closures, defs, chain: Vec::new() };
        let mut body = std::mem::replace(&mut caller.body, CoreBlock { stmts: vec![], tail: None });
        cx.block(&mut body, &mut caller);
        caller.body = body;
        drop_dead_values(&mut caller);
        sizes[id] = block_size(&caller.body);
        prog.funcs[id] = caller;
    }
}

struct Inliner<'a> {
    funcs: &'a [CoreFn],
    candidate: &'a [bool],
    recursive: &'a [bool],
    /// The function being inlined into.
    caller: usize,
    sizes: &'a [usize],
    sites: &'a [usize],
    /// The caller's current size.
    size: usize,
    /// The caller's size before inlining.
    caller_size: usize,
    /// How many of the caller's loops enclose the current position.
    loops: u32,
    /// The caller's next unused `Once` label.
    label: u32,
    /// Caller locals bound to an object allocated in the caller.
    local_objects: Vec<bool>,
    /// The functions whose copies enclose the current position.
    chain: Vec<usize>,
    /// Caller locals bound once to a payload-less enum variant: its tag.
    known: HashMap<LocalId, u32>,
    /// Caller locals bound once to a closure of known code and captures.
    closures: HashMap<LocalId, (FuncId, Vec<CoreExpr>)>,
    /// How many times each caller local is defined.
    defs: Vec<u32>,
}

impl Inliner<'_> {
    fn wants(&self, g: FuncId, args: &[CoreExpr]) -> bool {
        let g = g as usize;
        if !self.candidate[g] || g == self.caller || self.chain.contains(&g) { return false; }
        let size = self.sizes[g];
        // In the caller's own loop (not a body copied into it).
        let dispatch = self.loops > 0 && self.chain.is_empty() && self.caller_size <= LOOP_CALLER
            && self.sites[g] <= LOOP_SITES && self.size + size <= CALLER_LIMIT;
        if dispatch { return true; }
        if self.recursive[g] {
            // Copying a recursive function is bounded by `chain`: only its
            // one call site, or a copy specialized down to a few nodes.
            return (self.sites[g] == 1 && size <= SINGLE_SITE && self.size + size <= CALLER_LIMIT)
                || self.folded_size(g, args).is_some_and(|folded| folded <= FOLDED);
        }
        let hint = self.funcs[g].hints.inline;
        if hint == InlineHint::Always { return true; }
        if self.size + size > CALLER_LIMIT { return false; }
        size <= SMALL
            || ((hint == InlineHint::Hint || args.iter().any(|a| self.is_local_object(a))) && size <= HINTED)
            || (self.sites[g] == 1 && size <= SINGLE_SITE)
            || self.folded_size(g, args).is_some_and(|folded| folded <= FOLDED)
    }

    /// The size of `g`'s body once its matches on the arguments that are
    /// known variants are decided, if any argument is one.
    fn folded_size(&self, g: usize, args: &[CoreExpr]) -> Option<usize> {
        let callee = &self.funcs[g];
        let assigned = assigned_locals(callee);
        let known: HashMap<LocalId, u32> = args.iter().enumerate().filter_map(|(i, a)| match a.kind.as_ref() {
            CoreExprKind::Local(l) if !assigned[i] => self.known.get(l).map(|tag| (i as LocalId, *tag)),
            _ => None,
        }).collect();
        if known.is_empty() || self.sizes[g] > SINGLE_SITE { return None; }
        let mut body = callee.body.clone();
        fold_known_block(&mut body, &known);
        Some(block_size(&body))
    }

    fn is_local_object(&self, e: &CoreExpr) -> bool {
        matches!(e.kind.as_ref(), CoreExprKind::Local(l) if self.local_objects.get(*l as usize).copied().unwrap_or(false))
    }

    fn block(&mut self, b: &mut CoreBlock, caller: &mut CoreFn) {
        for s in &mut b.stmts {
            match s {
                CoreStmt::Let(l, e) => {
                    self.expr(e, caller);
                    // Inlining may have revealed a new object (`s.chars()`).
                    if new_object(e) {
                        self.local_objects.resize(caller.locals.len(), false);
                        self.local_objects[*l as usize] = true;
                    }
                }
                CoreStmt::Expr(e) => self.expr(e, caller),
            }
        }
        if let Some(t) = &mut b.tail { self.expr(t, caller); }
    }

    fn expr(&mut self, e: &mut CoreExpr, caller: &mut CoreFn) {
        match e.kind.as_mut() {
            CoreExprKind::If(c, y, n) => { self.expr(c, caller); self.block(y, caller); self.block(n, caller); }
            CoreExprKind::Loop(b) => { self.loops += 1; self.block(b, caller); self.loops -= 1; }
            CoreExprKind::Block(b) | CoreExprKind::Once { body: b, .. } => self.block(b, caller),
            CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
                self.expr(scrutinee, caller);
                for a in arms { self.expr(&mut a.body, caller); }
            }
            _ => for c in crate::copyprop::children_mut(e) { self.expr(c, caller) },
        }
        if matches!(e.kind.as_ref(), CoreExprKind::CallClosure { .. }) { return self.closure_call(e, caller); }
        let CoreExprKind::Call(g, args) = e.kind.as_ref() else { return };
        if !self.wants(*g, args) { return; }
        let g = *g as usize;
        let CoreExprKind::Call(_, args) = std::mem::replace(e.kind.as_mut(), CoreExprKind::Unit) else { unreachable!() };
        let funcs = self.funcs;
        *e = self.instantiate(&funcs[g], args, e.repr.clone(), e.span, caller);
        self.learn(e, caller);
        self.size += self.sizes[g];
        // In the caller its arguments may be objects the caller allocated,
        // so its calls are reconsidered (never re-entering this chain).
        self.chain.push(g);
        self.expr(e, caller);
        self.chain.pop();
    }

    /// A copy of `callee`'s body for the call `callee(args)`, as a caller
    /// expression of `repr`.
    fn instantiate(&mut self, callee: &CoreFn, args: Vec<CoreExpr>, repr: Repr, span: SpanId, caller: &mut CoreFn) -> CoreExpr {
        let bindings = args.into_iter().enumerate().map(|(i, a)| (i as LocalId, a)).collect();
        self.instantiate_with(callee, bindings, repr, span, caller)
    }

    /// A copy of `callee`'s body with each `(callee local, value)` binding
    /// (parameters, and a closure's captures) in place.
    fn instantiate_with(&mut self, callee: &CoreFn, bindings: Vec<(LocalId, CoreExpr)>, repr: Repr, span: SpanId, caller: &mut CoreFn) -> CoreExpr {
        let base = caller.locals.len() as LocalId;
        caller.local_names.resize(caller.locals.len(), None);
        caller.locals.extend(callee.locals.iter().cloned());
        let names = callee.local_names.iter().cloned().chain(std::iter::repeat(None)).take(callee.locals.len());
        caller.local_names.extend(names);
        let label_base = self.label;
        let label = label_base + max_label(&callee.body) + 1;
        self.label = label + 1;
        let mut body = callee.body.clone();
        // A bound local the callee never assigns is replaced by its value (a
        // local or constant): callee code cannot change caller locals.
        let assigned = assigned_locals(callee);
        let mut substitute: HashMap<LocalId, CoreExpr> = HashMap::new();
        let mut bound = Vec::new();
        for (local, a) in bindings {
            if !assigned[local as usize] && matches!(a.kind.as_ref(), CoreExprKind::Local(_) | CoreExprKind::ConstInt(..)
                | CoreExprKind::ConstBool(_) | CoreExprKind::ConstChar(_) | CoreExprKind::ConstFloat(..) | CoreExprKind::Unit) {
                substitute.insert(local, a);
                caller.locals[(base + local) as usize] = Repr::Unit;
            } else {
                bound.push(CoreStmt::Let(base + local, a));
            }
        }
        let mut returns = false;
        rename_block(&mut body, &mut |e| match e.kind.as_mut() {
            CoreExprKind::Local(l) => match substitute.get(l) {
                Some(arg) => *e = arg.clone(),
                None => *l += base,
            },
            CoreExprKind::Assign { local, .. } => *local += base,
            CoreExprKind::Once { label, .. } | CoreExprKind::Leave { label, .. } => *label += label_base,
            CoreExprKind::Match { arms, .. } | CoreExprKind::ValueMatch { arms, .. } => {
                for a in arms { for b in &mut a.binds { *b += base; } }
            }
            CoreExprKind::Return(_) => {
                let CoreExprKind::Return(value) = std::mem::replace(e.kind.as_mut(), CoreExprKind::Unit) else { unreachable!() };
                *e.kind = CoreExprKind::Leave { label, value };
                returns = true;
            }
            _ => {}
        }, &mut |l| *l += base);
        body.stmts.splice(0..0, bound);
        fold_known_block(&mut body, &self.known);
        let kind = if returns { CoreExprKind::Once { label, body: Box::new(body) } } else { CoreExprKind::Block(Box::new(body)) };
        CoreExpr { kind: Box::new(kind), repr, span }
    }

    /// Record the definitions, known variants and closures of a copied body.
    fn learn(&mut self, e: &CoreExpr, caller: &CoreFn) {
        enum Value { Variant(u32), Closure(FuncId, Vec<CoreExpr>) }
        self.defs.resize(caller.locals.len(), 0);
        let defs = &mut self.defs;
        let mut values: Vec<(LocalId, Value)> = Vec::new();
        let block_lets = |b: &CoreBlock, defs: &mut Vec<u32>, values: &mut Vec<(LocalId, Value)>| for s in &b.stmts {
            let CoreStmt::Let(l, v) = s else { continue };
            defs[*l as usize] += 1;
            match v.kind.as_ref() {
                CoreExprKind::MakeValueVariant { tag, fields, .. } if fields.is_empty() => values.push((*l, Value::Variant(*tag))),
                CoreExprKind::MakeClosure { code, captures, .. } => values.push((*l, Value::Closure(*code, captures.clone()))),
                _ => {}
            }
        };
        visit_expr(e, &mut |x| match x.kind.as_ref() {
            CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => block_lets(b, defs, &mut values),
            CoreExprKind::If(_, y, n) => { block_lets(y, defs, &mut values); block_lets(n, defs, &mut values); }
            CoreExprKind::Assign { local, .. } => defs[*local as usize] += 1,
            CoreExprKind::Match { arms, .. } | CoreExprKind::ValueMatch { arms, .. } => {
                for a in arms { for b in &a.binds { defs[*b as usize] += 1; } }
            }
            _ => {}
        });
        for (l, v) in values {
            if self.defs[l as usize] != 1 { continue; }
            match v {
                Value::Variant(tag) => { self.known.insert(l, tag); }
                Value::Closure(code, captures) => {
                    let stable = captures.iter().all(|c| match c.kind.as_ref() {
                        CoreExprKind::Local(x) => self.defs.get(*x as usize) == Some(&1),
                        k => matches!(k, CoreExprKind::ConstInt(..) | CoreExprKind::ConstBool(_) | CoreExprKind::ConstChar(_) | CoreExprKind::ConstFloat(..) | CoreExprKind::Unit),
                    });
                    if stable { self.closures.insert(l, (code, captures)); }
                }
            }
        }
    }

    /// Inline `f(args)` for a closure `f` the caller built from a known
    /// function and stable captures.
    fn closure_call(&mut self, e: &mut CoreExpr, caller: &mut CoreFn) {
        let CoreExprKind::CallClosure { callee, args } = e.kind.as_ref() else { return };
        let CoreExprKind::Local(f) = callee.kind.as_ref() else { return };
        let Some((code, captures)) = self.closures.get(f).cloned() else { return };
        let g = code as usize;
        let function = &self.funcs[g];
        if self.chain.contains(&g) || function.closure_captures.len() != captures.len()
            || self.sizes[g] > HINTED || self.size + self.sizes[g] > CALLER_LIMIT { return; }
        let ncaptures = captures.len() as LocalId;
        let mut bindings: Vec<(LocalId, CoreExpr)> = function.closure_captures.iter().map(|c| c.local).zip(captures).collect();
        bindings.extend(args.iter().cloned().enumerate().map(|(i, a)| (ncaptures + i as LocalId, a)));
        let funcs = self.funcs;
        *e = self.instantiate_with(&funcs[g], bindings, e.repr.clone(), e.span, caller);
        self.learn(e, caller);
        self.size += self.sizes[g];
        self.chain.push(g);
        self.expr(e, caller);
        self.chain.pop();
    }
}

/// Does `e` evaluate to a newly allocated struct object?
fn new_object(e: &CoreExpr) -> bool {
    match e.kind.as_ref() {
        CoreExprKind::New { .. } => true,
        CoreExprKind::Block(b) => b.tail.as_ref().is_some_and(new_object),
        _ => false,
    }
}

/// Which locals some `let` binds to a newly allocated struct object.
fn local_objects(body: &CoreBlock, count: usize) -> Vec<bool> {
    let is_new = new_object;
    let mut objects = vec![false; count];
    let mut mark = |b: &CoreBlock| for s in &b.stmts {
        if let CoreStmt::Let(l, v) = s { if is_new(v) { objects[*l as usize] = true; } }
    };
    mark(body);
    visit_block(body, &mut |e| match e.kind.as_ref() {
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => mark(b),
        CoreExprKind::If(_, y, n) => { mark(y); mark(n); }
        _ => {}
    });
    objects
}

/// Remove `let`s of unused locals whose values are pure constructions of
/// locals and constants (a closure whose calls were all inlined, say). Each
/// removal may leave more unused.
fn drop_dead_values(f: &mut CoreFn) {
    fn atomic(e: &CoreExpr) -> bool {
        matches!(e.kind.as_ref(), CoreExprKind::Local(_) | CoreExprKind::ConstInt(..) | CoreExprKind::ConstBool(_)
            | CoreExprKind::ConstChar(_) | CoreExprKind::ConstFloat(..) | CoreExprKind::ConstZero(_) | CoreExprKind::Unit)
    }
    fn pure(e: &CoreExpr) -> bool {
        match e.kind.as_ref() {
            CoreExprKind::MakeClosure { captures: parts, .. } | CoreExprKind::New { fields: parts, .. }
            | CoreExprKind::MakeValue { fields: parts, .. } | CoreExprKind::MakeValueVariant { fields: parts, .. }
            | CoreExprKind::MakeVariant { fields: parts, .. } => parts.iter().all(atomic),
            _ => atomic(e),
        }
    }
    fn prune(b: &mut CoreBlock, reads: &[u32], removed: &mut Vec<LocalId>) {
        b.stmts.retain(|s| match s {
            CoreStmt::Let(l, v) if reads[*l as usize] == 0 && pure(v) => { removed.push(*l); false }
            _ => true,
        });
        for s in &mut b.stmts {
            let (CoreStmt::Let(_, e) | CoreStmt::Expr(e)) = s;
            prune_expr(e, reads, removed);
        }
        if let Some(t) = &mut b.tail { prune_expr(t, reads, removed); }
    }
    fn prune_expr(e: &mut CoreExpr, reads: &[u32], removed: &mut Vec<LocalId>) {
        match e.kind.as_mut() {
            CoreExprKind::If(c, y, n) => { prune_expr(c, reads, removed); prune(y, reads, removed); prune(n, reads, removed); }
            CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => prune(b, reads, removed),
            CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
                prune_expr(scrutinee, reads, removed);
                for a in arms { prune_expr(&mut a.body, reads, removed); }
            }
            _ => for c in crate::copyprop::children_mut(e) { prune_expr(c, reads, removed) },
        }
    }
    let defs = definition_counts(f);
    loop {
        let mut reads = vec![0u32; f.locals.len()];
        visit_block(&f.body, &mut |e| if let CoreExprKind::Local(l) = e.kind.as_ref() { reads[*l as usize] += 1 });
        // A local also defined elsewhere keeps its `let`s.
        for (r, d) in reads.iter_mut().zip(&defs) { if *d != 1 { *r += 1; } }
        let mut removed = Vec::new();
        prune(&mut f.body, &reads, &mut removed);
        if removed.is_empty() { return; }
        for l in removed { f.locals[l as usize] = Repr::Unit; }
    }
}

/// Which locals `f` assigns.
fn assigned_locals(f: &CoreFn) -> Vec<bool> {
    let mut assigned = vec![false; f.locals.len()];
    visit_block(&f.body, &mut |e| if let CoreExprKind::Assign { local, .. } = e.kind.as_ref() {
        assigned[*local as usize] = true;
    });
    assigned
}

/// Locals of `f` bound once to a closure whose captures are constants or
/// locals bound once (so they hold the captured values at every call).
fn known_closures(f: &CoreFn) -> HashMap<LocalId, (FuncId, Vec<CoreExpr>)> {
    let defs = definition_counts(f);
    let mut out = HashMap::new();
    let mut lets = |b: &CoreBlock| for s in &b.stmts {
        if let CoreStmt::Let(l, v) = s {
            if let CoreExprKind::MakeClosure { code, captures, .. } = v.kind.as_ref() {
                let stable = captures.iter().all(|c| match c.kind.as_ref() {
                    CoreExprKind::Local(x) => defs[*x as usize] == 1,
                    k => matches!(k, CoreExprKind::ConstInt(..) | CoreExprKind::ConstBool(_) | CoreExprKind::ConstChar(_) | CoreExprKind::ConstFloat(..) | CoreExprKind::Unit),
                });
                if stable && defs[*l as usize] == 1 { out.insert(*l, (*code, captures.clone())); }
            }
        }
    };
    lets(&f.body);
    visit_block(&f.body, &mut |e| match e.kind.as_ref() {
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => lets(b),
        CoreExprKind::If(_, y, n) => { lets(y); lets(n); }
        _ => {}
    });
    out
}

/// How many times each local of `f` is defined (parameters and captures
/// once on entry, then `let`s, assignments and match bindings).
fn definition_counts(f: &CoreFn) -> Vec<u32> {
    let mut defs = vec![0u32; f.locals.len()];
    for d in defs.iter_mut().take(f.closure_captures.len() + f.params.len()) { *d += 1; }
    let lets = |b: &CoreBlock, defs: &mut Vec<u32>| for s in &b.stmts {
        if let CoreStmt::Let(l, _) = s { defs[*l as usize] += 1; }
    };
    lets(&f.body, &mut defs);
    visit_block(&f.body, &mut |e| match e.kind.as_ref() {
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => lets(b, &mut defs),
        CoreExprKind::If(_, y, n) => { lets(y, &mut defs); lets(n, &mut defs); }
        CoreExprKind::Assign { local, .. } => defs[*local as usize] += 1,
        CoreExprKind::Match { arms, .. } | CoreExprKind::ValueMatch { arms, .. } => {
            for a in arms { for b in &a.binds { defs[*b as usize] += 1; } }
        }
        _ => {}
    });
    defs
}

/// Locals of `f` bound once, by `let`, to a payload-less enum variant.
fn known_variants(f: &CoreFn) -> HashMap<LocalId, u32> {
    let defs = definition_counts(f);
    let mut tags = HashMap::new();
    let mut lets = |b: &CoreBlock| for s in &b.stmts {
        if let CoreStmt::Let(l, v) = s {
            if let CoreExprKind::MakeValueVariant { tag, fields, .. } = v.kind.as_ref() {
                if fields.is_empty() && defs[*l as usize] == 1 { tags.insert(*l, *tag); }
            }
        }
    };
    lets(&f.body);
    visit_block(&f.body, &mut |e| match e.kind.as_ref() {
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => lets(b),
        CoreExprKind::If(_, y, n) => { lets(y); lets(n); }
        _ => {}
    });
    tags
}

/// The wildcard arm of a `ValueMatch`.
const ANY_TAG: u32 = u32::MAX;

/// Decide each `ValueMatch` on a local of known variant, then merge arms
/// that yield one constant into a wildcard arm.
fn fold_known_block(b: &mut CoreBlock, known: &HashMap<LocalId, u32>) {
    for s in &mut b.stmts {
        match s {
            CoreStmt::Let(_, e) | CoreStmt::Expr(e) => fold_known_expr(e, known),
        }
    }
    if let Some(t) = &mut b.tail { fold_known_expr(t, known); }
}

fn fold_known_expr(e: &mut CoreExpr, known: &HashMap<LocalId, u32>) {
    match e.kind.as_mut() {
        CoreExprKind::If(c, y, n) => { fold_known_expr(c, known); fold_known_block(y, known); fold_known_block(n, known); }
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => fold_known_block(b, known),
        CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
            fold_known_expr(scrutinee, known);
            for a in arms { fold_known_expr(&mut a.body, known); }
        }
        _ => for c in crate::copyprop::children_mut(e) { fold_known_expr(c, known) },
    }
    // `{ => v }` is `v`.
    if let CoreExprKind::Block(b) = e.kind.as_mut() {
        if b.stmts.is_empty() && b.tail.as_ref().is_some_and(|t| t.repr == e.repr) {
            *e = b.tail.take().unwrap();
            return;
        }
    }
    let CoreExprKind::ValueMatch { scrutinee, arms } = e.kind.as_mut() else { return };
    if let CoreExprKind::Local(x) = scrutinee.kind.as_ref() {
        if let Some(&tag) = known.get(x) {
            let arm = arms.iter().position(|a| a.tag == tag).or_else(|| arms.iter().position(|a| a.tag == ANY_TAG));
            if let Some(i) = arm.filter(|&i| arms[i].binds.is_empty() && arms[i].body.repr == e.repr) {
                *e = arms.swap_remove(i).body;
                return;
            }
        }
    }
    // Arms yielding the same constant become one wildcard arm.
    let constant = |a: &CoreArm| matches!(a.body.kind.as_ref(), CoreExprKind::ConstBool(_) | CoreExprKind::ConstInt(..));
    let same = |a: &CoreExpr, b: &CoreExpr| a.kind == b.kind && a.repr == b.repr;
    let mut best: Option<(CoreExpr, usize)> = None;
    for a in arms.iter().filter(|a| constant(a)) {
        let n = arms.iter().filter(|o| constant(o) && same(&o.body, &a.body)).count();
        if best.as_ref().is_none_or(|(_, m)| n > *m) { best = Some((a.body.clone(), n)); }
    }
    let Some((value, count)) = best else { return };
    let wildcard_conflicts = arms.iter().any(|a| a.tag == ANY_TAG && !same(&a.body, &value));
    if count < 2 || wildcard_conflicts { return; }
    arms.retain(|a| !(constant(a) && same(&a.body, &value)));
    arms.push(CoreArm { tag: ANY_TAG, binds: Vec::new(), body: value });
}

fn placeholder() -> CoreFn {
    CoreFn { name: String::new(), params: vec![], extern_by_ref: vec![], ret: Repr::Unit, locals: vec![], local_names: vec![],
        body: CoreBlock { stmts: vec![], tail: None }, closure_captures: vec![], is_closure: false, is_extern: true,
        hints: Default::default(), span: NO_SPAN }
}

/// Apply `f` to every expression (parents before children) and `bind` to
/// every `let`-bound local.
fn rename_block(b: &mut CoreBlock, f: &mut impl FnMut(&mut CoreExpr), bind: &mut impl FnMut(&mut LocalId)) {
    for s in &mut b.stmts {
        match s {
            CoreStmt::Let(l, e) => { bind(l); rename_expr(e, f, bind); }
            CoreStmt::Expr(e) => rename_expr(e, f, bind),
        }
    }
    if let Some(t) = &mut b.tail { rename_expr(t, f, bind); }
}

fn rename_expr(e: &mut CoreExpr, f: &mut impl FnMut(&mut CoreExpr), bind: &mut impl FnMut(&mut LocalId)) {
    f(e);
    match e.kind.as_mut() {
        CoreExprKind::If(c, y, n) => { rename_expr(c, f, bind); rename_block(y, f, bind); rename_block(n, f, bind); }
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => rename_block(b, f, bind),
        CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
            rename_expr(scrutinee, f, bind);
            for a in arms { rename_expr(&mut a.body, f, bind); }
        }
        _ => for c in crate::copyprop::children_mut(e) { rename_expr(c, f, bind) },
    }
}

/// Visit every expression in `b`.
pub(crate) fn visit_block(b: &CoreBlock, f: &mut impl FnMut(&CoreExpr)) {
    for s in &b.stmts {
        match s {
            CoreStmt::Let(_, e) | CoreStmt::Expr(e) => visit_expr(e, f),
        }
    }
    if let Some(t) = &b.tail { visit_expr(t, f); }
}

pub(crate) fn visit_expr(e: &CoreExpr, f: &mut impl FnMut(&CoreExpr)) {
    f(e);
    match e.kind.as_ref() {
        CoreExprKind::If(c, y, n) => { visit_expr(c, f); visit_block(y, f); visit_block(n, f); }
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => visit_block(b, f),
        CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
            visit_expr(scrutinee, f);
            for a in arms { visit_expr(&a.body, f); }
        }
        _ => for c in crate::copyprop::children(e) { visit_expr(c, f) },
    }
}

/// Apply `f` to every expression in `e`, parents first.
pub(crate) fn visit_expr_mut(e: &mut CoreExpr, f: &mut impl FnMut(&mut CoreExpr)) {
    rename_expr(e, f, &mut |_| {});
}

fn block_size(b: &CoreBlock) -> usize {
    let mut n = 0;
    visit_block(b, &mut |_| n += 1);
    n
}

fn max_label(b: &CoreBlock) -> u32 {
    let mut m = 0;
    visit_block(b, &mut |e| if let CoreExprKind::Once { label, .. } = e.kind.as_ref() { m = m.max(*label) });
    m
}

/// Functions on a call-graph cycle (including direct self-calls).
fn recursive_functions(calls: &[Vec<FuncId>]) -> Vec<bool> {
    // Tarjan's strongly connected components, iteratively.
    let n = calls.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut result = vec![false; n];
    let mut next = 0;
    for root in 0..n {
        if index[root] != usize::MAX { continue; }
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next; low[root] = next; next += 1;
        stack.push(root); on_stack[root] = true;
        while let Some(&mut (v, ref mut i)) = work.last_mut() {
            if *i < calls[v].len() {
                let w = calls[v][*i] as usize;
                *i += 1;
                if index[w] == usize::MAX {
                    index[w] = next; low[w] = next; next += 1;
                    stack.push(w); on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                work.pop();
                if let Some(&(parent, _)) = work.last() { low[parent] = low[parent].min(low[v]); }
                if low[v] == index[v] {
                    let mut members = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        members.push(w);
                        if w == v { break; }
                    }
                    let cyclic = members.len() > 1 || calls[v].contains(&(v as FuncId));
                    for m in members { result[m] = cyclic; }
                }
            }
        }
    }
    result
}

/// Every function, each after the functions it calls (cycles broken anywhere).
fn postorder(calls: &[Vec<FuncId>]) -> Vec<usize> {
    let n = calls.len();
    let mut seen = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for root in 0..n {
        if seen[root] { continue; }
        seen[root] = true;
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(&mut (v, ref mut i)) = work.last_mut() {
            if *i < calls[v].len() {
                let w = calls[v][*i] as usize;
                *i += 1;
                if !seen[w] { seen[w] = true; work.push((w, 0)); }
            } else {
                order.push(v);
                work.pop();
            }
        }
    }
    order
}
