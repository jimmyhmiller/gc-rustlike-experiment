//! Scalar replacement of objects that never leave their function.
//!
//! A local bound once to a new struct object (`New`) and used only as the
//! object of field reads and writes holds the only reference to that object:
//! no other code can observe it, so its fields can live in locals instead.
//! The allocation disappears, each field read becomes a local read and each
//! field write an assignment. Inlining (`crate::inline`) exposes these: a
//! method call on a local object becomes field accesses on that local.
//! An object may also escape at its last use (stored in another object,
//! passed to a call, returned): there it is allocated from the current field
//! values, so only the paths that let it escape pay for it.

use crate::core::*;
use std::collections::{BTreeSet, HashMap, HashSet};

pub fn scalar_replace(prog: &mut CoreProgram) {
    if !crate::inline::enabled() { return; }
    for f in &mut prog.funcs {
        if f.is_extern { continue; }
        // Replacing an object can expose the objects held in its fields.
        while replace_fn(f, &prog.layouts) {}
        replace_values(f, &prog.values);
    }
}

/// Scalar replacement of value aggregates: a struct or tuple value bound once
/// by `let t = MakeValue {..}` and only projected (`t.i`) is its fields, a
/// local each, and is never built. The match on `(kind, a, b)` tests the
/// three values, not a tuple copied into the frame.
fn replace_values(f: &mut CoreFn, values: &[ValueLayout]) {
    let n = f.locals.len();
    let mut defs = vec![0u32; n];
    let mut built = vec![false; n];
    let mut reads = vec![0u32; n];
    let mut projected = vec![0u32; n];
    let incoming = f.closure_captures.len() + f.params.len();
    for d in defs.iter_mut().take(incoming) { *d += 1; }
    fn count_b(b: &CoreBlock, defs: &mut [u32], built: &mut [bool], reads: &mut [u32], projected: &mut [u32], values: &[ValueLayout]) {
        for s in &b.stmts {
            match s {
                CoreStmt::Let(l, e) => {
                    defs[*l as usize] += 1;
                    if let CoreExprKind::MakeValue { value, .. } = e.kind.as_ref() {
                        if values[*value as usize].variants.is_none() { built[*l as usize] = true; }
                    }
                    count_e(e, defs, built, reads, projected, values);
                }
                CoreStmt::Expr(e) => count_e(e, defs, built, reads, projected, values),
            }
        }
        if let Some(t) = &b.tail { count_e(t, defs, built, reads, projected, values); }
    }
    fn count_e(e: &CoreExpr, defs: &mut [u32], built: &mut [bool], reads: &mut [u32], projected: &mut [u32], values: &[ValueLayout]) {
        use CoreExprKind::*;
        match e.kind.as_ref() {
            Local(l) => reads[*l as usize] += 1,
            Field { base, loc: FieldLoc::ValueField { .. } } if matches!(base.kind.as_ref(), Local(_)) => {
                let Local(l) = base.kind.as_ref() else { unreachable!() };
                reads[*l as usize] += 1;
                projected[*l as usize] += 1;
            }
            Assign { local, value } => { defs[*local as usize] += 1; count_e(value, defs, built, reads, projected, values); }
            If(c, y, z) => { count_e(c, defs, built, reads, projected, values); count_b(y, defs, built, reads, projected, values); count_b(z, defs, built, reads, projected, values); }
            Block(b) | Loop(b) | Once { body: b, .. } => count_b(b, defs, built, reads, projected, values),
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                count_e(scrutinee, defs, built, reads, projected, values);
                for a in arms { for b in &a.binds { defs[*b as usize] += 1; } count_e(&a.body, defs, built, reads, projected, values); }
            }
            _ => for c in crate::copyprop::children(e) { count_e(c, defs, built, reads, projected, values) },
        }
    }
    count_b(&f.body, &mut defs, &mut built, &mut reads, &mut projected, values);
    let replace: Vec<bool> = (0..n).map(|l| built[l] && defs[l] == 1 && reads[l] == projected[l]).collect();
    if !replace.iter().any(|r| *r) { return; }
    // Each replaced aggregate's fields, as new locals.
    let mut parts: HashMap<LocalId, Vec<LocalId>> = HashMap::new();
    fn rewrite_b(b: &mut CoreBlock, replace: &[bool], parts: &mut HashMap<LocalId, Vec<LocalId>>, locals: &mut Vec<Repr>) {
        let mut out = Vec::with_capacity(b.stmts.len());
        for s in std::mem::take(&mut b.stmts) {
            match s {
                CoreStmt::Let(l, mut e) if replace[l as usize] => {
                    rewrite_e(&mut e, replace, parts, locals);
                    let CoreExprKind::MakeValue { fields, .. } = *e.kind else { unreachable!() };
                    let mut ids = Vec::with_capacity(fields.len());
                    for field in fields {
                        let id = locals.len() as LocalId;
                        locals.push(field.repr.clone());
                        ids.push(id);
                        out.push(CoreStmt::Let(id, field));
                    }
                    parts.insert(l, ids);
                }
                CoreStmt::Let(l, mut e) => { rewrite_e(&mut e, replace, parts, locals); out.push(CoreStmt::Let(l, e)); }
                CoreStmt::Expr(mut e) => { rewrite_e(&mut e, replace, parts, locals); out.push(CoreStmt::Expr(e)); }
            }
        }
        b.stmts = out;
        if let Some(t) = &mut b.tail { rewrite_e(t, replace, parts, locals); }
    }
    fn rewrite_e(e: &mut CoreExpr, replace: &[bool], parts: &mut HashMap<LocalId, Vec<LocalId>>, locals: &mut Vec<Repr>) {
        use CoreExprKind::*;
        if let Field { base, loc: FieldLoc::ValueField { index } } = e.kind.as_ref() {
            if let Local(l) = base.kind.as_ref() {
                if let Some(ids) = parts.get(l) {
                    let id = ids[*index as usize];
                    *e = CoreExpr::new(Local(id), locals[id as usize].clone());
                    return;
                }
            }
        }
        match e.kind.as_mut() {
            If(c, y, z) => { rewrite_e(c, replace, parts, locals); rewrite_b(y, replace, parts, locals); rewrite_b(z, replace, parts, locals); }
            Block(b) | Loop(b) | Once { body: b, .. } => rewrite_b(b, replace, parts, locals),
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                rewrite_e(scrutinee, replace, parts, locals);
                for a in arms { rewrite_e(&mut a.body, replace, parts, locals); }
            }
            _ => for c in crate::copyprop::children_mut(e) { rewrite_e(c, replace, parts, locals) },
        }
    }
    let mut body = std::mem::replace(&mut f.body, CoreBlock { stmts: vec![], tail: None });
    rewrite_b(&mut body, &replace, &mut parts, &mut f.locals);
    f.body = body;
    f.local_names.resize(f.locals.len(), None);
    for l in parts.keys() { f.locals[*l as usize] = Repr::Unit; }
}

#[derive(Default, Clone, Copy)]
struct Uses {
    /// Every read of the local.
    reads: u32,
    /// Reads as the object of a field access.
    field_bases: u32,
    /// `let` definitions whose value is a new object.
    new_defs: u32,
    /// Any other definition (another `let`, an assignment, a binding).
    other_defs: u32,
}

fn replace_fn(f: &mut CoreFn, layouts: &[Layout]) -> bool {
    let mut uses = vec![Uses::default(); f.locals.len()];
    let incoming = f.closure_captures.len() + f.params.len();
    for u in uses.iter_mut().take(incoming) { u.other_defs += 1; }
    count_block(&f.body, &mut uses);
    let single = |l: usize| matches!(f.locals[l], Repr::Ref(_)) && uses[l].new_defs == 1 && uses[l].other_defs == 0;
    let mut candidates: Vec<LocalId> = (0..f.locals.len())
        .filter(|&l| single(l) && uses[l].reads == uses[l].field_bases).map(|l| l as LocalId).collect();
    // Objects that escape: each escape must be the object's last use. One
    // without field accesses gains nothing (and one allocated at its escape
    // is such an object).
    let escaping: BTreeSet<LocalId> = (0..f.locals.len())
        .filter(|&l| single(l) && uses[l].field_bases > 0 && uses[l].reads > uses[l].field_bases).map(|l| l as LocalId).collect();
    let mut materialized = HashSet::new();
    if !escaping.is_empty() {
        let dead = crate::codegen_liveness::dead_after(f, &escaping);
        let mut ok: HashMap<LocalId, bool> = escaping.iter().map(|l| (*l, true)).collect();
        check_block(&f.body, &dead, &escaping, &mut ok, false);
        for (l, good) in ok { if good { candidates.push(l); materialized.insert(l); } }
    }
    if candidates.is_empty() { return false; }
    // Each candidate's fields, as new locals in source field order.
    let mut fields: HashMap<LocalId, (LayoutId, Vec<LocalId>)> = HashMap::new();
    let mut shapes: HashMap<LocalId, Vec<Repr>> = HashMap::new();
    find_shapes(&f.body, &candidates, &mut shapes);
    for (&object, reprs) in &shapes {
        let Repr::Ref(layout) = f.locals[object as usize] else { continue };
        let shape = &layouts[layout as usize];
        if shape.field_map.len() != reprs.len() { continue; }
        let base = f.locals.len() as LocalId;
        f.local_names.resize(f.locals.len(), None);
        let name = f.local_names.get(object as usize).cloned().flatten();
        for (i, r) in reprs.iter().enumerate() {
            f.locals.push(r.clone());
            f.local_names.push(name.as_ref().map(|n| format!("{n}.{i}")));
        }
        fields.insert(object, (layout, (0..reprs.len() as LocalId).map(|i| base + i).collect()));
    }
    if fields.is_empty() { return false; }
    materialized.retain(|l| fields.contains_key(l));
    let mut cx = Rewrite { fields: &fields, layouts, materialized: &materialized, locals: &mut f.locals };
    let mut body = std::mem::replace(&mut f.body, CoreBlock { stmts: vec![], tail: None });
    cx.block(&mut body);
    f.body = body;
    f.local_names.resize(f.locals.len(), None);
    for object in fields.keys() { f.locals[*object as usize] = Repr::Unit; }
    true
}

/// Clear `ok` for each escaping object one of whose escapes is not the only
/// use of it in its statement, after which it is dead. An escape in a nested
/// block's tail belongs to the enclosing statement (`covered` for the tails
/// of blocks inside one); one in no statement fails.
fn check_block(b: &CoreBlock, dead: &HashMap<usize, BTreeSet<LocalId>>, escaping: &BTreeSet<LocalId>, ok: &mut HashMap<LocalId, bool>, covered: bool) {
    for s in &b.stmts {
        let (CoreStmt::Let(_, e) | CoreStmt::Expr(e)) = s;
        for x in own_escapes(e, escaping) {
            let last = dead.get(&(s as *const CoreStmt as usize)).is_some_and(|d| d.contains(&x));
            if !last || occurrences(e, x) != 1 { ok.insert(x, false); }
        }
        for nested in nested_blocks(e) { check_block(nested, dead, escaping, ok, true); }
    }
    if let Some(t) = &b.tail {
        if !covered { for x in own_escapes(t, escaping) { ok.insert(x, false); } }
        for nested in nested_blocks(t) { check_block(nested, dead, escaping, ok, covered); }
    }
}

/// The objects read in `e` itself, or in the tails of blocks within it (not
/// in their statements), other than as the object of a field access.
fn own_escapes(e: &CoreExpr, objects: &BTreeSet<LocalId>) -> Vec<LocalId> {
    let mut out = Vec::new();
    fn walk(e: &CoreExpr, objects: &BTreeSet<LocalId>, out: &mut Vec<LocalId>) {
        match e.kind.as_ref() {
            CoreExprKind::Local(l) if objects.contains(l) => out.push(*l),
            CoreExprKind::Field { base, .. } if matches!(base.kind.as_ref(), CoreExprKind::Local(_)) => {}
            CoreExprKind::SetField { base, value, .. } if matches!(base.kind.as_ref(), CoreExprKind::Local(_)) => walk(value, objects, out),
            CoreExprKind::If(c, y, n) => {
                walk(c, objects, out);
                for t in [&y.tail, &n.tail].into_iter().flatten() { walk(t, objects, out); }
            }
            CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => {
                if let Some(t) = &b.tail { walk(t, objects, out); }
            }
            CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
                walk(scrutinee, objects, out);
                for a in arms { walk(&a.body, objects, out); }
            }
            _ => for c in crate::copyprop::children(e) { walk(c, objects, out) },
        }
    }
    walk(e, objects, &mut out);
    out
}

/// The blocks directly inside `e` (not inside each other).
fn nested_blocks(e: &CoreExpr) -> Vec<&CoreBlock> {
    let mut out = Vec::new();
    fn walk<'a>(e: &'a CoreExpr, out: &mut Vec<&'a CoreBlock>) {
        match e.kind.as_ref() {
            CoreExprKind::If(c, y, n) => { walk(c, out); out.push(y); out.push(n); }
            CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => out.push(b),
            CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
                walk(scrutinee, out);
                for a in arms { walk(&a.body, out); }
            }
            _ => for c in crate::copyprop::children(e) { walk(c, out) },
        }
    }
    walk(e, &mut out);
    out
}

fn occurrences(e: &CoreExpr, x: LocalId) -> usize {
    let mut n = 0;
    crate::inline::visit_expr(e, &mut |e| if matches!(e.kind.as_ref(), CoreExprKind::Local(l) if *l == x) { n += 1 });
    n
}

fn count_block(b: &CoreBlock, uses: &mut [Uses]) {
    for s in &b.stmts {
        match s {
            CoreStmt::Let(l, e) => {
                if new_value(e).is_some() { uses[*l as usize].new_defs += 1; } else { uses[*l as usize].other_defs += 1; }
                count_expr(e, uses);
            }
            CoreStmt::Expr(e) => count_expr(e, uses),
        }
    }
    if let Some(t) = &b.tail { count_expr(t, uses); }
}

fn count_expr(e: &CoreExpr, uses: &mut [Uses]) {
    crate::inline::visit_expr(e, &mut |e| match e.kind.as_ref() {
        CoreExprKind::Local(l) => uses[*l as usize].reads += 1,
        CoreExprKind::Field { base, .. } | CoreExprKind::SetField { base, .. } => {
            if let CoreExprKind::Local(l) = base.kind.as_ref() { uses[*l as usize].field_bases += 1; }
        }
        CoreExprKind::Assign { local, .. } => uses[*local as usize].other_defs += 1,
        CoreExprKind::Match { arms, .. } | CoreExprKind::ValueMatch { arms, .. } => {
            for a in arms { for b in &a.binds { uses[*b as usize].other_defs += 1; } }
        }
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => {
            // Nested `let`s are counted here; `visit_expr` sees their values.
            for s in &b.stmts {
                if let CoreStmt::Let(l, v) = s {
                    if new_value(v).is_some() { uses[*l as usize].new_defs += 1; } else { uses[*l as usize].other_defs += 1; }
                }
            }
        }
        CoreExprKind::If(_, y, n) => {
            for b in [y, n] {
                for s in &b.stmts {
                    if let CoreStmt::Let(l, v) = s {
                        if new_value(v).is_some() { uses[*l as usize].new_defs += 1; } else { uses[*l as usize].other_defs += 1; }
                    }
                }
            }
        }
        _ => {}
    });
}

/// The `New` an expression evaluates to: itself, or a block's tail.
fn new_value(e: &CoreExpr) -> Option<&CoreExpr> {
    match e.kind.as_ref() {
        CoreExprKind::New { .. } => Some(e),
        CoreExprKind::Block(b) => b.tail.as_ref().and_then(new_value),
        _ => None,
    }
}

fn find_shapes(b: &CoreBlock, candidates: &[LocalId], out: &mut HashMap<LocalId, Vec<Repr>>) {
    let visit = |l: &LocalId, v: &CoreExpr, out: &mut HashMap<LocalId, Vec<Repr>>| {
        if candidates.contains(l) {
            if let Some(CoreExprKind::New { fields, .. }) = new_value(v).map(|e| e.kind.as_ref()) {
                out.insert(*l, fields.iter().map(|f| f.repr.clone()).collect());
            }
        }
    };
    for s in &b.stmts { if let CoreStmt::Let(l, v) = s { visit(l, v, out); } }
    crate::inline::visit_block(b, &mut |e| {
        let blocks: Vec<&CoreBlock> = match e.kind.as_ref() {
            CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => vec![b],
            CoreExprKind::If(_, y, n) => vec![y, n],
            _ => vec![],
        };
        for b in blocks { for s in &b.stmts { if let CoreStmt::Let(l, v) = s { visit(l, v, out); } } }
    });
}

struct Rewrite<'a> {
    /// Each replaced object's layout and field locals.
    fields: &'a HashMap<LocalId, (LayoutId, Vec<LocalId>)>,
    layouts: &'a [Layout],
    /// The replaced objects allocated where they escape.
    materialized: &'a HashSet<LocalId>,
    locals: &'a mut Vec<Repr>,
}

impl Rewrite<'_> {
    fn block(&mut self, b: &mut CoreBlock) {
        let stmts = std::mem::take(&mut b.stmts);
        for s in stmts {
            match s {
                CoreStmt::Let(l, mut v) if self.fields.contains_key(&l) => {
                    // `let o = New { f.. }` becomes `let o.i = f.i` for each field.
                    // A replaced object stored in a field escapes here, so it is
                    // allocated first.
                    self.escapes(&mut v, &mut b.stmts);
                    self.expr(&mut v);
                    let (prefix, values) = take_new(&mut v);
                    b.stmts.extend(prefix);
                    for (target, value) in self.fields[&l].1.iter().zip(values) {
                        b.stmts.push(CoreStmt::Let(*target, value));
                    }
                }
                CoreStmt::Let(l, mut v) => { self.escapes(&mut v, &mut b.stmts); self.expr(&mut v); b.stmts.push(CoreStmt::Let(l, v)); }
                CoreStmt::Expr(mut v) => { self.escapes(&mut v, &mut b.stmts); self.expr(&mut v); b.stmts.push(CoreStmt::Expr(v)); }
            }
        }
        if let Some(t) = &mut b.tail { self.expr(t); }
    }

    /// Allocate each object escaping in statement `e` just before it.
    fn escapes(&mut self, e: &mut CoreExpr, out: &mut Vec<CoreStmt>) {
        let objects: BTreeSet<LocalId> = self.materialized.iter().copied().collect();
        for x in own_escapes(e, &objects) {
            let (layout, locals) = &self.fields[&x];
            let fields = locals.iter().map(|l| CoreExpr::new(CoreExprKind::Local(*l), self.locals[*l as usize].clone())).collect();
            let object = self.locals.len() as LocalId;
            self.locals.push(Repr::Ref(*layout));
            out.push(CoreStmt::Let(object, CoreExpr::new(CoreExprKind::New { layout: *layout, fields }, Repr::Ref(*layout))));
            crate::inline::visit_expr_mut(e, &mut |e| if matches!(e.kind.as_ref(), CoreExprKind::Local(l) if *l == x) {
                *e.kind = CoreExprKind::Local(object);
            });
        }
    }

    fn expr(&mut self, e: &mut CoreExpr) {
        match e.kind.as_mut() {
            CoreExprKind::If(c, y, n) => { self.expr(c); self.block(y); self.block(n); }
            CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => self.block(b),
            CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms { self.expr(&mut a.body); }
            }
            _ => for c in crate::copyprop::children_mut(e) { self.expr(c) },
        }
        let (fields, layouts) = (self.fields, self.layouts);
        let field_local = |base: &CoreExpr, loc: &FieldLoc| -> Option<LocalId> {
            let CoreExprKind::Local(object) = base.kind.as_ref() else { return None };
            let (layout, locals) = fields.get(object)?;
            let index = layouts[*layout as usize].field_map.iter().position(|l| l == loc)?;
            Some(locals[index])
        };
        let replacement = match e.kind.as_ref() {
            CoreExprKind::Field { base, loc } => field_local(base, loc).map(CoreExprKind::Local),
            CoreExprKind::SetField { base, loc, .. } => field_local(base, loc).map(|local| {
                let CoreExprKind::SetField { value, .. } = std::mem::replace(e.kind.as_mut(), CoreExprKind::Unit) else { unreachable!() };
                CoreExprKind::Assign { local, value }
            }),
            _ => None,
        };
        if let Some(kind) = replacement { *e.kind = kind; }
    }
}

/// Split `New`-valued `e` into the statements before the `New` and its field
/// values.
fn take_new(e: &mut CoreExpr) -> (Vec<CoreStmt>, Vec<CoreExpr>) {
    match std::mem::replace(e.kind.as_mut(), CoreExprKind::Unit) {
        CoreExprKind::New { fields, .. } => (Vec::new(), fields),
        CoreExprKind::Block(b) => {
            let b = *b;
            let mut tail = b.tail.expect("new-valued block has a tail");
            let (mut inner, values) = take_new(&mut tail);
            let mut stmts = b.stmts;
            stmts.append(&mut inner);
            (stmts, values)
        }
        _ => unreachable!("not new-valued"),
    }
}
