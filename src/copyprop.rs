//! Copy propagation over Core IR.
//!
//! Lowering often binds a local to a copy of another (`let t = x;` for a
//! match scrutinee, a payload binding, or a call operand). Each copy is a
//! stack slot, and one holding references is also a GC root that is zeroed
//! and registered on entry. A copy whose source cannot change while the copy
//! is in use is replaced by the source.
//!
//! `let n = o` is removed when `n` and `o` are each defined exactly once and
//! `o`'s single definition is a parameter or capture, a match-arm binding,
//! or a `let` outside every loop. Then every use of `n` (all after its
//! `let`) sees the value `o` had at the copy, so `o` can stand in for it.
//!
//! It is also removed when every use of `n` follows its `let` within its
//! block, and `o`'s `let` precedes `n`'s in that block or an enclosing one:
//! `o` is then not redefined while `n` is in use, even inside a loop, since
//! running `o`'s `let` again also runs `n`'s before any use of `n`.

use crate::core::{CoreBlock, CoreExpr, CoreExprKind, CoreFn, CoreProgram, CoreStmt, LocalId, Repr};
use std::collections::HashMap;

pub fn copy_propagate(prog: &mut CoreProgram) {
    for f in &mut prog.funcs {
        propagate_fn(f);
    }
}

#[derive(Default, Clone, Copy)]
struct Defs {
    count: u32,
    /// The single definition is stable for the copy's lifetime.
    stable: bool,
}

fn propagate_fn(f: &mut CoreFn) {
    if f.is_extern { return; }
    let mut defs = vec![Defs::default(); f.locals.len()];
    let incoming = f.closure_captures.len() + f.params.len();
    for d in defs.iter_mut().take(incoming) { *d = Defs { count: 1, stable: true }; }
    count_block(&f.body, 0, &mut defs);
    let mut order = Order { next: 0, lets: HashMap::new(), uses: vec![(u32::MAX, 0); f.locals.len()] };
    order.block(&f.body);
    let mut copies: HashMap<LocalId, LocalId> = HashMap::new();
    find_copies_block(&f.body, &defs, &f.locals, &order, &mut copies);
    if copies.is_empty() { return; }
    // Resolve chains (`a = b; c = a`).
    let resolve = |mut l: LocalId| { let mut n = 0; while let Some(&o) = copies.get(&l) { l = o; n += 1; if n > copies.len() { break; } } l };
    let mapping: HashMap<LocalId, LocalId> = copies.keys().map(|&n| (n, resolve(n))).collect();
    rewrite_block(&mut f.body, &mapping);
    for &n in mapping.keys() { f.locals[n as usize] = Repr::Unit; }
}

fn count_def(defs: &mut [Defs], l: LocalId, stable: bool) {
    let d = &mut defs[l as usize];
    d.count += 1;
    d.stable = d.count == 1 && stable;
}

fn count_block(b: &CoreBlock, loops: u32, defs: &mut [Defs]) {
    for s in &b.stmts {
        match s {
            CoreStmt::Let(l, e) => { count_def(defs, *l, loops == 0); count_expr(e, loops, defs); }
            CoreStmt::Expr(e) => count_expr(e, loops, defs),
        }
    }
    if let Some(t) = &b.tail { count_expr(t, loops, defs); }
}

fn count_expr(e: &CoreExpr, loops: u32, defs: &mut [Defs]) {
    match e.kind.as_ref() {
        CoreExprKind::Assign { local, value } => { count_def(defs, *local, false); count_expr(value, loops, defs); }
        CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
            count_expr(scrutinee, loops, defs);
            for a in arms {
                // A binding is set on entry to its arm and nowhere else.
                for &l in &a.binds { count_def(defs, l, true); }
                count_expr(&a.body, loops, defs);
            }
        }
        CoreExprKind::Loop(b) => count_block(b, loops + 1, defs),
        CoreExprKind::Block(b) | CoreExprKind::Once { body: b, .. } => count_block(b, loops, defs),
        CoreExprKind::If(c, y, n) => { count_expr(c, loops, defs); count_block(y, loops, defs); count_block(n, loops, defs); }
        _ => for c in children(e) { count_expr(c, loops, defs) },
    }
}

/// Statements numbered in pre-order. A `let` is at number `at`, its value's
/// statements end at `end`, and the rest of its block ends at `block_end`.
struct Order {
    next: u32,
    lets: HashMap<LocalId, Let>,
    /// Each local's first and last use, by statement number.
    uses: Vec<(u32, u32)>,
}

#[derive(Clone, Copy)]
struct Let { at: u32, end: u32, block_end: u32 }

impl Order {
    fn block(&mut self, b: &CoreBlock) {
        let mut in_block = Vec::new();
        for s in &b.stmts {
            let at = self.next;
            self.next += 1;
            let (CoreStmt::Let(_, e) | CoreStmt::Expr(e)) = s;
            self.expr(e, at);
            if let CoreStmt::Let(l, _) = s { in_block.push((*l, at, self.next - 1)); }
        }
        if let Some(t) = &b.tail { let at = self.next; self.next += 1; self.expr(t, at); }
        let block_end = self.next - 1;
        for (l, at, end) in in_block { self.lets.insert(l, Let { at, end, block_end }); }
    }

    fn expr(&mut self, e: &CoreExpr, at: u32) {
        match e.kind.as_ref() {
            CoreExprKind::Local(l) => {
                let u = &mut self.uses[*l as usize];
                *u = (u.0.min(at), u.1.max(at));
            }
            CoreExprKind::Loop(b) | CoreExprKind::Block(b) | CoreExprKind::Once { body: b, .. } => self.block(b),
            CoreExprKind::If(c, y, n) => { self.expr(c, at); self.block(y); self.block(n); }
            CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
                self.expr(scrutinee, at);
                for a in arms { self.expr(&a.body, at); }
            }
            _ => for c in children(e) { self.expr(c, at) },
        }
    }

    /// Is every use of `n` after its `let` in its block, and `o`'s `let`
    /// before `n`'s in that block or an enclosing one?
    fn scoped_copy(&self, n: LocalId, o: LocalId) -> bool {
        let (Some(ln), Some(lo)) = (self.lets.get(&n), self.lets.get(&o)) else { return false };
        let (first, last) = self.uses[n as usize];
        let uses_scoped = first == u32::MAX || (first > ln.end && last <= ln.block_end);
        uses_scoped && lo.end < ln.at && ln.at <= lo.block_end
    }
}

fn find_copies_block(b: &CoreBlock, defs: &[Defs], locals: &[Repr], order: &Order, out: &mut HashMap<LocalId, LocalId>) {
    for s in &b.stmts {
        match s {
            CoreStmt::Let(n, e) => {
                if let CoreExprKind::Local(o) = e.kind.as_ref() {
                    let (dn, d_o) = (defs[*n as usize], defs[*o as usize]);
                    if dn.count == 1 && d_o.count == 1 && n != o && locals[*n as usize] == locals[*o as usize]
                        && (d_o.stable || order.scoped_copy(*n, *o)) {
                        out.insert(*n, *o);
                    }
                }
                find_copies_expr(e, defs, locals, order, out);
            }
            CoreStmt::Expr(e) => find_copies_expr(e, defs, locals, order, out),
        }
    }
    if let Some(t) = &b.tail { find_copies_expr(t, defs, locals, order, out); }
}

fn find_copies_expr(e: &CoreExpr, defs: &[Defs], locals: &[Repr], order: &Order, out: &mut HashMap<LocalId, LocalId>) {
    match e.kind.as_ref() {
        CoreExprKind::Loop(b) | CoreExprKind::Block(b) | CoreExprKind::Once { body: b, .. } => find_copies_block(b, defs, locals, order, out),
        CoreExprKind::If(c, y, n) => { find_copies_expr(c, defs, locals, order, out); find_copies_block(y, defs, locals, order, out); find_copies_block(n, defs, locals, order, out); }
        CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
            find_copies_expr(scrutinee, defs, locals, order, out);
            for a in arms { find_copies_expr(&a.body, defs, locals, order, out); }
        }
        _ => for c in children(e) { find_copies_expr(c, defs, locals, order, out) },
    }
}

fn rewrite_block(b: &mut CoreBlock, map: &HashMap<LocalId, LocalId>) {
    b.stmts.retain(|s| !matches!(s, CoreStmt::Let(n, _) if map.contains_key(n)));
    for s in &mut b.stmts {
        match s {
            CoreStmt::Let(_, e) | CoreStmt::Expr(e) => rewrite_expr(e, map),
        }
    }
    if let Some(t) = &mut b.tail { rewrite_expr(t, map); }
}

fn rewrite_expr(e: &mut CoreExpr, map: &HashMap<LocalId, LocalId>) {
    match e.kind.as_mut() {
        CoreExprKind::Local(l) => { if let Some(&o) = map.get(l) { *l = o; } }
        CoreExprKind::Loop(b) | CoreExprKind::Block(b) | CoreExprKind::Once { body: b, .. } => rewrite_block(b, map),
        CoreExprKind::If(c, y, n) => { rewrite_expr(c, map); rewrite_block(y, map); rewrite_block(n, map); }
        CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
            rewrite_expr(scrutinee, map);
            for a in arms { rewrite_expr(&mut a.body, map); }
        }
        _ => for c in children_mut(e) { rewrite_expr(c, map) },
    }
}

/// Operand children of the kinds without blocks or arms (those are handled
/// by the callers). Adding an IR operation must state its operands here.
pub(crate) fn children(e: &CoreExpr) -> Vec<&CoreExpr> {
    use CoreExprKind::*;
    match e.kind.as_ref() {
        Bin(_, a, b) | StrEq(a, b) | StrGet(a, b) => vec![a, b],
        Un(_, v) | FloatIntrinsic(_, v) | FloatBits(v) | Print(v) | PrintStr(v) | PrintStrRaw(v)
        | StrLen(v) | StrEqLit(v, _) | StrToFloat(v) | StrHash(v) | TypeIdOf(v) | ThreadSpawn(v)
        | ThreadJoin(v) | ThreadSleep(v) | PtrReadI64(v) | Panic(v) | EnumTag(v) | ArrayLen(v) => vec![v],
        StrConcat { a, b, .. } => vec![a, b],
        StrSubstring { s, start, end, .. } => vec![s, start, end],
        StrFromNum { v, .. } => vec![v],
        StrFromChar { cp, .. } => vec![cp],
        ReadFile { path, .. } => vec![path],
        TypeNameOf { obj, .. } => vec![obj],
        AsCBytes { src, .. } => vec![src],
        ArrayNew { len, .. } => vec![len],
        ArrayGet { array, index, .. } | ArrayGetUnchecked { array, index, .. } | ArrayGetChecked { array, index, .. } => vec![array, index],
        ArraySet { array, index, value, .. } => vec![array, index, value],
        Cast { value, .. } => vec![value],
        Call(_, args) | RuntimeCall { args, .. } | HostCall { args, .. } | StrJoin { args, .. } => args.iter().collect(),
        AtomLoad { atom, .. } => vec![atom],
        AtomCas { atom, old, new } => vec![atom, old, new],
        ChanSend { buf, ctrl, value } => vec![buf, ctrl, value],
        ChanRecv { buf, ctrl, .. } => vec![buf, ctrl],
        CallClosure { callee, args } => std::iter::once(callee.as_ref()).chain(args.iter()).collect(),
        MakeClosure { captures, .. } => captures.iter().collect(),
        New { fields, .. } | MakeValue { fields, .. } | MakeVariant { fields, .. } | MakeValueVariant { fields, .. } => fields.iter().collect(),
        Field { base, .. } => vec![base],
        SetField { base, value, .. } => vec![base, value],
        EnumPayload { scrutinee, .. } => vec![scrutinee],
        Break(value) | Return(value) | Leave { value, .. } => value.iter().map(|e| e.as_ref()).collect(),
        Assign { value, .. } => vec![value],
        Match { .. } | ValueMatch { .. } | If(..) | Block(_) | Loop(_) | Once { .. } => Vec::new(),
        ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..) | ConstStr(..)
        | Unit | Local(..) | Continue | ThreadYield | ThreadCurrentId | CallbackPtr(..) => Vec::new(),
    }
}

pub(crate) fn children_mut(e: &mut CoreExpr) -> Vec<&mut CoreExpr> {
    use CoreExprKind::*;
    match e.kind.as_mut() {
        Bin(_, a, b) | StrEq(a, b) | StrGet(a, b) => vec![a, b],
        Un(_, v) | FloatIntrinsic(_, v) | FloatBits(v) | Print(v) | PrintStr(v) | PrintStrRaw(v)
        | StrLen(v) | StrEqLit(v, _) | StrToFloat(v) | StrHash(v) | TypeIdOf(v) | ThreadSpawn(v)
        | ThreadJoin(v) | ThreadSleep(v) | PtrReadI64(v) | Panic(v) | EnumTag(v) | ArrayLen(v) => vec![v],
        StrConcat { a, b, .. } => vec![a, b],
        StrSubstring { s, start, end, .. } => vec![s, start, end],
        StrFromNum { v, .. } => vec![v],
        StrFromChar { cp, .. } => vec![cp],
        ReadFile { path, .. } => vec![path],
        TypeNameOf { obj, .. } => vec![obj],
        AsCBytes { src, .. } => vec![src],
        ArrayNew { len, .. } => vec![len],
        ArrayGet { array, index, .. } | ArrayGetUnchecked { array, index, .. } | ArrayGetChecked { array, index, .. } => vec![array, index],
        ArraySet { array, index, value, .. } => vec![array, index, value],
        Cast { value, .. } => vec![value],
        Call(_, args) | RuntimeCall { args, .. } | HostCall { args, .. } | StrJoin { args, .. } => args.iter_mut().collect(),
        AtomLoad { atom, .. } => vec![atom],
        AtomCas { atom, old, new } => vec![atom, old, new],
        ChanSend { buf, ctrl, value } => vec![buf, ctrl, value],
        ChanRecv { buf, ctrl, .. } => vec![buf, ctrl],
        CallClosure { callee, args } => std::iter::once(callee.as_mut()).chain(args.iter_mut()).collect(),
        MakeClosure { captures, .. } => captures.iter_mut().collect(),
        New { fields, .. } | MakeValue { fields, .. } | MakeVariant { fields, .. } | MakeValueVariant { fields, .. } => fields.iter_mut().collect(),
        Field { base, .. } => vec![base],
        SetField { base, value, .. } => vec![base, value],
        EnumPayload { scrutinee, .. } => vec![scrutinee],
        Break(value) | Return(value) | Leave { value, .. } => value.iter_mut().map(|e| e.as_mut()).collect(),
        Assign { value, .. } => vec![value],
        Match { .. } | ValueMatch { .. } | If(..) | Block(_) | Loop(_) | Once { .. } => Vec::new(),
        ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..) | ConstStr(..)
        | Unit | Local(..) | Continue | ThreadYield | ThreadCurrentId | CallbackPtr(..) => Vec::new(),
    }
}
