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
    let mut copies: HashMap<LocalId, LocalId> = HashMap::new();
    find_copies_block(&f.body, &defs, &f.locals, &mut copies);
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
        CoreExprKind::Block(b) => count_block(b, loops, defs),
        CoreExprKind::If(c, y, n) => { count_expr(c, loops, defs); count_block(y, loops, defs); count_block(n, loops, defs); }
        _ => for c in children(e) { count_expr(c, loops, defs) },
    }
}

fn find_copies_block(b: &CoreBlock, defs: &[Defs], locals: &[Repr], out: &mut HashMap<LocalId, LocalId>) {
    for s in &b.stmts {
        match s {
            CoreStmt::Let(n, e) => {
                if let CoreExprKind::Local(o) = e.kind.as_ref() {
                    let (dn, d_o) = (defs[*n as usize], defs[*o as usize]);
                    if dn.count == 1 && d_o.count == 1 && d_o.stable && n != o && locals[*n as usize] == locals[*o as usize] {
                        out.insert(*n, *o);
                    }
                }
                find_copies_expr(e, defs, locals, out);
            }
            CoreStmt::Expr(e) => find_copies_expr(e, defs, locals, out),
        }
    }
    if let Some(t) = &b.tail { find_copies_expr(t, defs, locals, out); }
}

fn find_copies_expr(e: &CoreExpr, defs: &[Defs], locals: &[Repr], out: &mut HashMap<LocalId, LocalId>) {
    match e.kind.as_ref() {
        CoreExprKind::Loop(b) | CoreExprKind::Block(b) => find_copies_block(b, defs, locals, out),
        CoreExprKind::If(c, y, n) => { find_copies_expr(c, defs, locals, out); find_copies_block(y, defs, locals, out); find_copies_block(n, defs, locals, out); }
        CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
            find_copies_expr(scrutinee, defs, locals, out);
            for a in arms { find_copies_expr(&a.body, defs, locals, out); }
        }
        _ => for c in children(e) { find_copies_expr(c, defs, locals, out) },
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
        CoreExprKind::Loop(b) | CoreExprKind::Block(b) => rewrite_block(b, map),
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
fn children(e: &CoreExpr) -> Vec<&CoreExpr> {
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
        Break(value) | Return(value) => value.iter().map(|e| e.as_ref()).collect(),
        Assign { value, .. } => vec![value],
        Match { .. } | ValueMatch { .. } | If(..) | Block(_) | Loop(_) => Vec::new(),
        ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..) | ConstStr(..)
        | Unit | Local(..) | Continue | ThreadYield | ThreadCurrentId | CallbackPtr(..) => Vec::new(),
    }
}

fn children_mut(e: &mut CoreExpr) -> Vec<&mut CoreExpr> {
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
        Break(value) | Return(value) => value.iter_mut().map(|e| e.as_mut()).collect(),
        Assign { value, .. } => vec![value],
        Match { .. } | ValueMatch { .. } | If(..) | Block(_) | Loop(_) => Vec::new(),
        ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..) | ConstStr(..)
        | Unit | Local(..) | Continue | ThreadYield | ThreadCurrentId | CallbackPtr(..) => Vec::new(),
    }
}
