//! Reuse of field reads.
//!
//! `let t = x.f` reads a field; a later `let u = x.f` may reuse `t` when
//! nothing between them can write that field: no store to the same field of
//! the same layout (any object may be `x`), no reassignment of `x` or `t`,
//! and no call that may write fields or synchronize with another thread.
//! Plain field reads are not synchronizing (Java's model), so a value read
//! once may stand for a racy later read. Inlining leaves many such repeats:
//! a method reads `self.len` each time it is called.
//!
//! A read of an inline aggregate (`let p = x.proto`) copies all of it, where
//! Rust would take a reference. While that read is still available, a
//! projection of the copy (`p.code`) reads through the object instead
//! (`x.proto.code`), so only the projected part is loaded and the copy is
//! often dead.
//!
//! Each loop body starts with the reads available before the loop that its
//! body cannot clobber; reads made inside a loop or one branch do not outlive
//! it. A path that leaves (`return`, `break`, a panic) does not constrain
//! what follows its join, and a `Once` is followed by what is available at
//! every exit from it. A read in a loop of an object the loop does not redefine,
//! of a field the loop cannot write, is made once before the loop instead
//! (reads have no effects, and the object is initialized on entry).

use crate::core::*;
use std::collections::HashMap;

/// A field of a layout: what a store to that field may change.
type Key = (LayoutId, FieldLoc);

/// Something that invalidates available reads.
#[derive(Default, Clone)]
struct Clobbers {
    all: bool,
    fields: Vec<Key>,
    locals: Vec<LocalId>,
}

pub fn reuse_field_reads(prog: &mut CoreProgram) {
    if !crate::inline::enabled() { return; }
    let writers = field_writers(prog);
    let diverging = crate::codegen_effects::diverging_functions(prog);
    for f in &mut prog.funcs {
        if f.is_extern { continue; }
        let mut cx = Reuse { writers: &writers, diverging: &diverging, avail: HashMap::new(), views: HashMap::new(), same: HashMap::new(), live: true, onces: Vec::new(), locals: &mut f.locals };
        let mut body = std::mem::replace(&mut f.body, CoreBlock { stmts: vec![], tail: None });
        cx.block(&mut body);
        f.body = body;
        f.local_names.resize(f.locals.len(), None);
    }
}

/// Functions that may write a field, call unknown code, or synchronize.
fn field_writers(prog: &CoreProgram) -> Vec<bool> {
    let mut writes: Vec<bool> = prog.funcs.iter().map(|f| f.is_extern).collect();
    let mut calls: Vec<Vec<FuncId>> = vec![Vec::new(); prog.funcs.len()];
    for (id, f) in prog.funcs.iter().enumerate() {
        if f.is_extern { continue; }
        crate::inline::visit_block(&f.body, &mut |e| {
            if let CoreExprKind::Call(g, _) = e.kind.as_ref() { calls[id].push(*g); }
            else if clobbers_all(e) || matches!(e.kind.as_ref(), CoreExprKind::SetField { .. }) { writes[id] = true; }
        });
    }
    loop {
        let mut changed = false;
        for id in 0..writes.len() {
            if !writes[id] && calls[id].iter().any(|g| writes[*g as usize]) { writes[id] = true; changed = true; }
        }
        if !changed { return writes; }
    }
}

/// Operations that may write any field or synchronize (calls aside).
fn clobbers_all(e: &CoreExpr) -> bool {
    use CoreExprKind::*;
    match e.kind.as_ref() {
        CallClosure { .. } | ThreadSpawn(_) | ThreadJoin(_) | AtomLoad { .. } | AtomCas { .. }
        | ChanSend { .. } | ChanRecv { .. } | ThreadSleep(_) | ThreadYield | HostCall { .. } => true,
        // Runtime helpers that only read, allocate, or copy array elements.
        RuntimeCall { func, .. } => !matches!(*func, "ai_array_copy" | "ai_str_copy_bytes" | "ai_str_from_bytes"
            | "ai_str_from_utf8_prefix" | "ai_fmt_f64"),
        _ => false,
    }
}

fn key(base: &CoreExpr, loc: &FieldLoc) -> Option<Key> {
    match (&base.repr, loc) {
        (Repr::Ref(layout), FieldLoc::Ptr { .. } | FieldLoc::Raw { .. } | FieldLoc::ValueAt { .. }) => Some((*layout, *loc)),
        _ => None,
    }
}

struct Reuse<'a> {
    writers: &'a [bool],
    diverging: &'a [bool],
    /// (object local, field) → the local holding its last read.
    avail: HashMap<(LocalId, Key), LocalId>,
    /// Aggregate locals equal to a projection of an available read: the
    /// read, the local that made it, and the projection's expression.
    views: HashMap<LocalId, View>,
    /// Locals bound to a reused read, and the local whose value they hold:
    /// reads through either are the same reads.
    same: HashMap<LocalId, LocalId>,
    /// Whether control reaches this point (not after a `return`, ...).
    live: bool,
    /// For each enclosing `Once`, by label: the reads available at every
    /// exit from it so far.
    onces: Vec<(u32, Option<Avail>)>,
    locals: &'a mut Vec<Repr>,
}

type Avail = HashMap<(LocalId, Key), LocalId>;

/// Meets the reads available on one path into a join.
fn meet(joined: &mut Option<Avail>, avail: &Avail) {
    match joined {
        None => *joined = Some(avail.clone()),
        Some(j) => j.retain(|k, v| avail.get(k) == Some(v)),
    }
}

#[derive(Clone)]
struct View {
    read: (LocalId, Key),
    holder: LocalId,
    expr: CoreExpr,
}

impl Reuse<'_> {
    fn block(&mut self, b: &mut CoreBlock) {
        for s in &mut b.stmts {
            match s {
                CoreStmt::Let(t, e) => {
                    self.expr(e);
                    self.forget_local(*t);
                    let read = match e.kind.as_ref() {
                        CoreExprKind::Field { base, loc } => match base.kind.as_ref() {
                            CoreExprKind::Local(x) => key(base, loc).map(|k| (self.same.get(x).copied().unwrap_or(*x), k)),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(read) = read {
                        match self.avail.get(&read) {
                            Some(&u) => {
                                *e.kind = CoreExprKind::Local(u);
                                self.same.insert(*t, self.same.get(&u).copied().unwrap_or(u));
                                if let Some(view) = self.views.get(&u).cloned() { self.views.insert(*t, view); }
                            }
                            None => {
                                self.avail.insert(read, *t);
                                if matches!(e.repr, Repr::Value(_)) {
                                    self.views.insert(*t, View { read, holder: *t, expr: e.clone() });
                                }
                            }
                        }
                    } else if let Some(view) = self.view_of(e) {
                        // A projection forwarded through an object read.
                        if matches!(e.repr, Repr::Value(_)) { self.views.insert(*t, View { expr: e.clone(), ..view }); }
                    }
                }
                CoreStmt::Expr(e) => self.expr(e),
            }
        }
        if let Some(t) = &mut b.tail { self.expr(t); }
    }

    /// The view that projection `e` reads through, if any.
    fn view_of(&self, e: &CoreExpr) -> Option<View> {
        let CoreExprKind::Field { base, loc: FieldLoc::ValueField { .. } } = e.kind.as_ref() else { return None };
        match base.kind.as_ref() {
            CoreExprKind::Field { .. } => self.view_of(base),
            CoreExprKind::Local(x) => self.views.get(x).filter(|v| self.avail.get(&v.read) == Some(&v.holder)).cloned(),
            _ => None,
        }
    }

    fn expr(&mut self, e: &mut CoreExpr) {
        use CoreExprKind::*;
        // `p.f` with `p` a view of a still-available read: read through it.
        if let Field { base, loc: FieldLoc::ValueField { .. } } = e.kind.as_mut() {
            if let Local(p) = base.kind.as_ref() {
                if let Some(view) = self.views.get(p).filter(|v| self.avail.get(&v.read) == Some(&v.holder)) {
                    **base = view.expr.clone();
                }
            }
        }
        match e.kind.as_mut() {
            If(c, y, n) => {
                self.expr(c);
                let (before, live) = (self.avail.clone(), self.live);
                let mut joined = None;
                self.block(y);
                if self.live { meet(&mut joined, &self.avail); }
                (self.avail, self.live) = (before, live);
                self.block(n);
                if self.live { meet(&mut joined, &self.avail); }
                self.join(joined);
            }
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                self.expr(scrutinee);
                let (before, live) = (self.avail.clone(), self.live);
                let mut joined = None;
                for a in arms.iter_mut() {
                    (self.avail, self.live) = (before.clone(), live);
                    for b in &a.binds { self.forget_local(*b); }
                    self.expr(&mut a.body);
                    if self.live { meet(&mut joined, &self.avail); }
                }
                if arms.is_empty() { meet(&mut joined, &before); }
                self.join(joined);
            }
            Block(b) => self.block(b),
            Loop(b) => {
                // Back edges: only reads the body cannot clobber stay
                // available, inside and after it. Invariant reads are made
                // once before the loop.
                let clobbers = block_clobbers(b, self.writers);
                self.apply(&clobbers);
                let mut hoisted = Vec::new();
                if !clobbers.all {
                    for (read, repr) in invariant_reads(b, &clobbers) {
                        if self.avail.contains_key(&read) { continue; }
                        let holder = self.locals.len() as LocalId;
                        self.locals.push(repr.clone());
                        let (object, (layout, loc)) = read;
                        let base = CoreExpr::new(Local(object), Repr::Ref(layout));
                        hoisted.push(CoreStmt::Let(holder, CoreExpr::new(Field { base: Box::new(base), loc }, repr)));
                        self.avail.insert(read, holder);
                    }
                }
                let kept = self.avail.clone();
                self.block(b);
                (self.avail, self.live) = (kept, true);
                if !hoisted.is_empty() {
                    let repr = e.repr.clone();
                    let span = e.span;
                    let looped = std::mem::replace(e, CoreExpr::new(Unit, Repr::Unit));
                    *e = CoreExpr { kind: Box::new(Block(Box::new(CoreBlock { stmts: hoisted, tail: Some(looped) }))), repr, span };
                    return;
                }
            }
            Once { label, body: b } => {
                // What follows is reached from the end of the body and from
                // each `leave` of it.
                self.onces.push((*label, None));
                self.block(b);
                let (_, mut exits) = self.onces.pop().unwrap();
                if self.live { meet(&mut exits, &self.avail); }
                self.join(exits);
            }
            Bin(crate::ast::BinOp::And | crate::ast::BinOp::Or, a, b) => {
                self.expr(a);
                let (before, live) = (self.avail.clone(), self.live);
                let mut joined = None;
                meet(&mut joined, &before);
                self.expr(b);
                if self.live { meet(&mut joined, &self.avail); }
                self.live = live;
                self.join(joined);
            }
            _ => for c in crate::copyprop::children_mut(e) { self.expr(c) },
        }
        // This node's own effect.
        match e.kind.as_ref() {
            SetField { base, loc, .. } => {
                if let Some(k) = key(base, loc) { self.avail.retain(|(_, f), _| *f != k); }
                // An aggregate local updated in place no longer equals its read.
                if let Local(p) = base.kind.as_ref() { self.views.remove(p); }
            }
            Assign { local, .. } => self.forget_local(*local),
            Call(g, _) if self.writers[*g as usize] => self.avail.clear(),
            _ if clobbers_all(e) => self.avail.clear(),
            _ => {}
        }
        // Control does not continue past these.
        match e.kind.as_ref() {
            Leave { label, .. } => {
                if self.live {
                    if let Some((_, exits)) = self.onces.iter_mut().rev().find(|(l, _)| l == label) { meet(exits, &self.avail); }
                }
                self.live = false;
            }
            Return(_) | Break(_) | Continue | Panic(_) => self.live = false,
            Call(g, _) if self.diverging[*g as usize] => self.live = false,
            _ => {}
        }
    }

    /// Continues after a join of the paths met in `joined` (unreachable if
    /// none reaches it).
    fn join(&mut self, joined: Option<Avail>) {
        match joined {
            Some(avail) => (self.avail, self.live) = (avail, true),
            None => self.live = false,
        }
    }

    fn forget_local(&mut self, l: LocalId) {
        self.avail.retain(|(x, _), holder| *x != l && *holder != l);
        self.views.remove(&l);
        self.same.retain(|t, u| *t != l && *u != l);
    }

    fn apply(&mut self, c: &Clobbers) {
        if c.all { self.avail.clear(); return; }
        self.avail.retain(|(x, f), holder| !c.fields.contains(f) && !c.locals.contains(x) && !c.locals.contains(holder));
        self.same.retain(|t, u| !c.locals.contains(t) && !c.locals.contains(u));
    }
}

/// The field reads in loop body `b` of objects it does not redefine, of
/// fields it cannot write, with the read's repr.
fn invariant_reads(b: &CoreBlock, clobbers: &Clobbers) -> Vec<((LocalId, Key), Repr)> {
    let mut out: Vec<((LocalId, Key), Repr)> = Vec::new();
    crate::inline::visit_block(b, &mut |e| {
        let CoreExprKind::Field { base, loc } = e.kind.as_ref() else { return };
        let CoreExprKind::Local(x) = base.kind.as_ref() else { return };
        let Some(k) = key(base, loc) else { return };
        if clobbers.locals.contains(x) || clobbers.fields.contains(&k) { return; }
        if !out.iter().any(|(r, _)| *r == (*x, k)) { out.push(((*x, k), e.repr.clone())); }
    });
    out
}

/// What running `b` (any number of times) may clobber.
fn block_clobbers(b: &CoreBlock, writers: &[bool]) -> Clobbers {
    let mut c = Clobbers::default();
    let lets = |b: &CoreBlock, c: &mut Clobbers| for s in &b.stmts { if let CoreStmt::Let(l, _) = s { c.locals.push(*l); } };
    lets(b, &mut c);
    crate::inline::visit_block(b, &mut |e| match e.kind.as_ref() {
        CoreExprKind::SetField { base, loc, .. } => match key(base, loc) { Some(k) => c.fields.push(k), None => c.all = true },
        CoreExprKind::Assign { local, .. } => c.locals.push(*local),
        CoreExprKind::Call(g, _) if writers[*g as usize] => c.all = true,
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => lets(b, &mut c),
        CoreExprKind::If(_, y, n) => { lets(y, &mut c); lets(n, &mut c); }
        CoreExprKind::Match { arms, .. } | CoreExprKind::ValueMatch { arms, .. } => for a in arms { c.locals.extend(a.binds.iter().copied()); },
        _ if clobbers_all(e) => c.all = true,
        _ => {}
    });
    c
}
