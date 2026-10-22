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
//! Each loop body starts with the reads available before the loop that its
//! body cannot clobber; reads made inside a loop, a `Once`, or one branch do
//! not outlive it. A read in a loop of an object the loop does not redefine,
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
    for f in &mut prog.funcs {
        if f.is_extern { continue; }
        let mut cx = Reuse { writers: &writers, avail: HashMap::new(), locals: &mut f.locals };
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
    /// (object local, field) → the local holding its last read.
    avail: HashMap<(LocalId, Key), LocalId>,
    locals: &'a mut Vec<Repr>,
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
                            CoreExprKind::Local(x) => key(base, loc).map(|k| (*x, k)),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(read) = read {
                        match self.avail.get(&read) {
                            Some(&u) => *e.kind = CoreExprKind::Local(u),
                            None => { self.avail.insert(read, *t); }
                        }
                    }
                }
                CoreStmt::Expr(e) => self.expr(e),
            }
        }
        if let Some(t) = &mut b.tail { self.expr(t); }
    }

    fn expr(&mut self, e: &mut CoreExpr) {
        use CoreExprKind::*;
        match e.kind.as_mut() {
            If(c, y, n) => {
                self.expr(c);
                let before = self.avail.clone();
                self.block(y);
                let after_yes = std::mem::replace(&mut self.avail, before);
                self.block(n);
                self.avail.retain(|k, v| after_yes.get(k) == Some(v));
            }
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                self.expr(scrutinee);
                let before = self.avail.clone();
                let mut merged: Option<HashMap<(LocalId, Key), LocalId>> = None;
                for a in arms.iter_mut() {
                    self.avail = before.clone();
                    for b in &a.binds { self.forget_local(*b); }
                    self.expr(&mut a.body);
                    let out = std::mem::take(&mut self.avail);
                    merged = Some(match merged {
                        None => out,
                        Some(mut m) => { m.retain(|k, v| out.get(k) == Some(v)); m }
                    });
                }
                self.avail = merged.unwrap_or(before);
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
                self.avail = kept;
                if !hoisted.is_empty() {
                    let repr = e.repr.clone();
                    let span = e.span;
                    let looped = std::mem::replace(e, CoreExpr::new(Unit, Repr::Unit));
                    *e = CoreExpr { kind: Box::new(Block(Box::new(CoreBlock { stmts: hoisted, tail: Some(looped) }))), repr, span };
                    return;
                }
            }
            Once { body: b, .. } => {
                // Early exits: only reads the body cannot clobber stay
                // available, inside and after it.
                let clobbers = block_clobbers(b, self.writers);
                self.apply(&clobbers);
                let kept = self.avail.clone();
                self.block(b);
                self.avail = kept;
            }
            Bin(crate::ast::BinOp::And | crate::ast::BinOp::Or, a, b) => {
                self.expr(a);
                let before = self.avail.clone();
                self.expr(b);
                let after = std::mem::replace(&mut self.avail, before);
                self.avail.retain(|k, v| after.get(k) == Some(v));
            }
            _ => for c in crate::copyprop::children_mut(e) { self.expr(c) },
        }
        // This node's own effect.
        match e.kind.as_ref() {
            SetField { base, loc, .. } => if let Some(k) = key(base, loc) { self.avail.retain(|(_, f), _| *f != k); },
            Assign { local, .. } => self.forget_local(*local),
            Call(g, _) if self.writers[*g as usize] => self.avail.clear(),
            _ if clobbers_all(e) => self.avail.clear(),
            _ => {}
        }
    }

    fn forget_local(&mut self, l: LocalId) {
        self.avail.retain(|(x, _), holder| *x != l && *holder != l);
    }

    fn apply(&mut self, c: &Clobbers) {
        if c.all { self.avail.clear(); return; }
        self.avail.retain(|(x, f), holder| !c.fields.contains(f) && !c.locals.contains(x) && !c.locals.contains(holder));
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
