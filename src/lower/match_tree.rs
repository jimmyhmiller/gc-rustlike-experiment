//! Decision trees for `match` (Maranget, "Compiling Pattern Matching to Good
//! Decision Trees"). The arms' patterns form a matrix with one column per
//! value inspected. The first arm's first tested column is tested once: an
//! enum's tag by one switch, literals in order, a bool by an `if`. Each
//! branch continues with the arms that agree with it, and a tuple or struct
//! column becomes its fields' columns. So no value is tested twice on a path,
//! and an arm whose test fails costs nothing after the switch.
//!
//! The arms' bindings and bodies are lowered as for a chain of tests; a leaf
//! of the tree runs one. An arm reached by more than one path (a `_` arm of
//! a tuple match) has one body, entered by `leave` from each of its leaves.
use super::*;

/// An arm's pattern, normalized: what the tree tests.
#[derive(Clone)]
pub(super) enum DPat {
    Wild,
    /// A constructor: an enum variant by tag, `true`/`false` as 1/0, or a
    /// tuple or struct (tag 0), with its fields' patterns.
    Ctor(u32, Vec<DPat>),
    /// A literal or constant, tested by equality; equal keys test alike.
    Lit(String, LitTest),
}

#[derive(Clone)]
pub(super) enum LitTest {
    Lit(LitPattern),
    Const(Path),
}

/// A value the tree tests: an expression reading it, and its type.
#[derive(Clone)]
struct Occ {
    expr: CoreExpr,
    ty: Ty,
}

#[derive(Clone)]
struct Row {
    pats: Vec<DPat>,
    arm: usize,
}

enum Tree {
    /// Arm `arm` matches; if its guard fails, `rest` decides.
    Leaf { arm: usize, rest: Option<Box<Tree>> },
    /// No arm matches. Exhaustiveness leaves this only after guards fail.
    Fail,
    /// The constructor of `occ`, held in `local`, picks a case.
    Switch { occ: Occ, local: LocalId, cases: Vec<(u32, Tree)>, default: Option<Box<Tree>> },
    /// Literal tests in order; `default` when none holds.
    Lits { occ: Occ, local: LocalId, cases: Vec<(LitTest, Tree)>, default: Box<Tree> },
}

/// What a leaf of the tree runs for each arm.
pub(super) struct ArmCode {
    pub binds: Vec<(LocalId, CoreExpr)>,
    pub guard: Option<CoreExpr>,
    pub body: CoreExpr,
}

thread_local! {
    /// `Once` labels for shared arm bodies (unique within any function).
    static LABELS: std::cell::Cell<u32> = const { std::cell::Cell::new(1 << 24) };
}
fn fresh_label() -> u32 {
    LABELS.with(|l| { let v = l.get(); l.set(v + 1); v })
}

impl FnLowerer<'_, '_, '_> {
    /// `pat` normalized for the tree, or `None` if the tree cannot test it.
    pub(super) fn dpat(&mut self, pat: &Pattern, ty: &Ty, span: Span) -> LResult<Option<DPat>> {
        Ok(Some(match &pat.kind {
            PatternKind::Wildcard | PatternKind::Binding { .. } => DPat::Wild,
            PatternKind::At { pat, .. } => return self.dpat(pat, ty, span),
            PatternKind::Literal(LitPattern::Bool(b)) => DPat::Ctor(*b as u32, vec![]),
            PatternKind::Literal(lit) => DPat::Lit(format!("{:?}", lit), LitTest::Lit(lit.clone())),
            // A constant of a type the tree switches on would mix tests.
            PatternKind::Variant { .. } if self.const_pattern(pat, ty) && matches!(ty, Ty::Named { .. } | Ty::Tuple(_) | Ty::Prim(Prim::Bool)) => return Ok(None),
            PatternKind::Variant { path, .. } if self.const_pattern(pat, ty) => {
                DPat::Lit(format!("const {}", path.segments.join("::")), LitTest::Const(path.clone()))
            }
            PatternKind::Variant { path, payload } => {
                let (tag, fields) = self.variant_fields(ty, path, span)?;
                let expanded = expand_rest(payload, fields.len());
                let payload = expanded.as_ref().unwrap_or(payload);
                if payload.len() != fields.len() { return Ok(None); }
                let mut subs = Vec::new();
                for (p, t) in payload.iter().zip(&fields) {
                    let Some(d) = self.dpat(p, t, span)? else { return Ok(None) };
                    subs.push(d);
                }
                DPat::Ctor(tag, subs)
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
                for (n, t) in names.iter().zip(&tys) {
                    subs.push(match fields.iter().find(|(f, _)| f == n) {
                        Some((_, p)) => match self.dpat(p, t, span)? { Some(d) => d, None => return Ok(None) },
                        None => DPat::Wild,
                    });
                }
                DPat::Ctor(tag, subs)
            }
            PatternKind::Tuple(ps) => {
                let Ty::Tuple(elems) = ty else { return Ok(None) };
                let expanded = expand_rest(ps, elems.len());
                let ps = expanded.as_ref().unwrap_or(ps);
                if ps.len() != elems.len() { return Ok(None); }
                let mut subs = Vec::new();
                for (p, t) in ps.iter().zip(elems) {
                    let Some(d) = self.dpat(p, t, span)? else { return Ok(None) };
                    subs.push(d);
                }
                DPat::Ctor(0, subs)
            }
        }))
    }

    /// The match of `scrut` (a local of type `sty`) against `pats`, one per
    /// arm, running `code[i]` for the first arm `i` that matches.
    pub(super) fn match_tree(
        &mut self, scrut: LocalId, sty: &Ty, pats: Vec<DPat>, code: Vec<ArmCode>, repr: &Repr, span: Span,
    ) -> LResult<CoreExpr> {
        let rows: Vec<Row> = pats.into_iter().enumerate().map(|(arm, p)| Row { pats: vec![p], arm }).collect();
        let guarded: Vec<bool> = code.iter().map(|c| c.guard.is_some()).collect();
        let occ = Occ { expr: CoreExpr::new(CoreExprKind::Local(scrut), self.locals[scrut as usize].clone()), ty: sty.clone() };
        let tree = self.tree(rows, vec![occ], &guarded, span)?;
        let mut leaves = vec![0usize; code.len()];
        count_leaves(&tree, &mut leaves);
        let shared: Vec<Option<u32>> = leaves.iter().map(|&n| (n > 1).then(fresh_label)).collect();
        let mut arms: Vec<Option<ArmCode>> = code.into_iter().map(Some).collect();
        if shared.iter().all(Option::is_none) {
            return self.emit(tree, &mut arms, &shared, None, repr, span);
        }
        // Shared bodies follow the tree, each after the `Once` its leaves
        // leave: `once E { once S2 { once S1 { tree } body1 } body2 }`.
        // Every path ends in `leave E`.
        let end = fresh_label();
        let unit = Repr::Unit;
        let tree_code = self.emit(tree, &mut arms, &shared, Some(end), repr, span)?;
        let mut inner = CoreBlock { stmts: vec![], tail: Some(tree_code) };
        for (arm, label) in shared.iter().enumerate() {
            let Some(label) = *label else { continue };
            let code = arms[arm].take().expect("a shared arm's body is placed once");
            let once = CoreExpr::new(CoreExprKind::Once { label, body: Box::new(inner) }, unit.clone());
            // An unguarded arm binds here; a guarded one bound before its guard.
            let mut stmts = vec![CoreStmt::Expr(once)];
            if code.guard.is_none() {
                stmts.extend(code.binds.into_iter().map(|(id, init)| CoreStmt::Let(id, init)));
            }
            stmts.push(CoreStmt::Expr(leave_with(end, code.body, repr)));
            inner = CoreBlock { stmts, tail: None };
        }
        Ok(CoreExpr::new(CoreExprKind::Once { label: end, body: Box::new(inner) }, repr.clone()))
    }

    fn tree(&mut self, rows: Vec<Row>, occs: Vec<Occ>, guarded: &[bool], span: Span) -> LResult<Tree> {
        let Some(first) = rows.first() else { return Ok(Tree::Fail) };
        let Some(c) = first.pats.iter().position(|p| !matches!(p, DPat::Wild)) else {
            let arm = first.arm;
            let rest = if guarded[arm] { Some(Box::new(self.tree(rows[1..].to_vec(), occs, guarded, span)?)) } else { None };
            return Ok(Tree::Leaf { arm, rest });
        };
        let occ = occs[c].clone();
        let without = |occs: &[Occ], subs: Vec<Occ>| -> Vec<Occ> {
            let mut out = occs[..c].to_vec();
            out.extend(subs);
            out.extend_from_slice(&occs[c + 1..]);
            out
        };
        // The rows a wildcard in column `c` keeps, with the column dropped.
        let defaults = |rows: &[Row]| -> Vec<Row> {
            rows.iter().filter(|r| matches!(r.pats[c], DPat::Wild)).map(|r| {
                let mut pats = r.pats.clone();
                pats.remove(c);
                Row { pats, arm: r.arm }
            }).collect()
        };
        if let DPat::Lit(..) = &first.pats[c] {
            let mut heads: Vec<(String, LitTest)> = Vec::new();
            for r in &rows {
                if let DPat::Lit(key, test) = &r.pats[c] {
                    if !heads.iter().any(|(k, _)| k == key) { heads.push((key.clone(), test.clone())); }
                }
            }
            let local = self.fresh_local(occ.expr.repr.clone(), occ.ty.clone());
            let mut cases = Vec::new();
            for (key, test) in heads {
                let kept: Vec<Row> = rows.iter().filter(|r| match &r.pats[c] {
                    DPat::Lit(k, _) => *k == key,
                    DPat::Wild => true,
                    DPat::Ctor(..) => false,
                }).map(|r| {
                    let mut pats = r.pats.clone();
                    pats.remove(c);
                    Row { pats, arm: r.arm }
                }).collect();
                cases.push((test, self.tree(kept, without(&occs, vec![]), guarded, span)?));
            }
            let default = Box::new(self.tree(defaults(&rows), without(&occs, vec![]), guarded, span)?);
            return Ok(Tree::Lits { occ, local, cases, default });
        }
        // Constructors, in the order the arms name them.
        let mut heads: Vec<u32> = Vec::new();
        for r in &rows {
            if let DPat::Ctor(t, _) = &r.pats[c] {
                if !heads.contains(t) { heads.push(*t); }
            }
        }
        let is_enum = matches!(&occ.ty, Ty::Named { name, .. } if self.ctx.enums.contains_key(name));
        let is_bool = matches!(occ.ty, Ty::Prim(Prim::Bool));
        let specialize = |rows: &[Row], tag: u32, arity: usize| -> Vec<Row> {
            rows.iter().filter_map(|r| {
                let subs = match &r.pats[c] {
                    DPat::Ctor(t, subs) if *t == tag => subs.clone(),
                    DPat::Ctor(..) | DPat::Lit(..) => return None,
                    DPat::Wild => vec![DPat::Wild; arity],
                };
                let mut pats = r.pats[..c].to_vec();
                pats.extend(subs);
                pats.extend_from_slice(&r.pats[c + 1..]);
                Some(Row { pats, arm: r.arm })
            }).collect()
        };
        if !is_enum && !is_bool {
            // A tuple or struct: one constructor, so only its fields to test.
            let subs = self.fields_of(&occ, &occ.expr, 0, span)?;
            let arity = subs.len();
            return self.tree(specialize(&rows, 0, arity), without(&occs, subs), guarded, span);
        }
        let local = self.fresh_local(occ.expr.repr.clone(), occ.ty.clone());
        let base = CoreExpr::new(CoreExprKind::Local(local), occ.expr.repr.clone());
        let mut cases = Vec::new();
        for &tag in &heads {
            let subs = self.fields_of(&occ, &base, tag, span)?;
            let arity = subs.len();
            cases.push((tag, self.tree(specialize(&rows, tag, arity), without(&occs, subs), guarded, span)?));
        }
        let all = if is_bool { 2 } else { self.constructors(&occ.ty)?.map_or(usize::MAX, |cs| cs.len()) };
        let default = if heads.len() < all {
            Some(Box::new(self.tree(defaults(&rows), without(&occs, vec![]), guarded, span)?))
        } else {
            None
        };
        Ok(Tree::Switch { occ, local, cases, default })
    }

    /// The fields of constructor `tag` of the value `base` (of `occ`'s type).
    fn fields_of(&mut self, occ: &Occ, base: &CoreExpr, tag: u32, span: Span) -> LResult<Vec<Occ>> {
        match &occ.ty {
            Ty::Tuple(elems) => elems.iter().enumerate().map(|(i, t)| {
                let repr = self.repr_of(t, span)?;
                Ok(Occ { expr: CoreExpr::new(CoreExprKind::Field { base: Box::new(base.clone()), loc: FieldLoc::ValueField { index: i as u32 } }, repr), ty: t.clone() })
            }).collect(),
            Ty::Named { name, .. } if self.ctx.enums.contains_key(name) => {
                let tys = self.ctor_field_tys(&occ.ty, tag)?;
                let payload_reprs: Vec<Repr> = tys.iter().map(|t| self.repr_of(t, span)).collect::<LResult<_>>()?;
                Ok(tys.into_iter().enumerate().map(|(i, t)| Occ {
                    expr: CoreExpr::new(CoreExprKind::EnumPayload {
                        scrutinee: Box::new(base.clone()), field: i as u32,
                        repr: payload_reprs[i].clone(), payload_reprs: payload_reprs.clone(),
                    }, payload_reprs[i].clone()),
                    ty: t,
                }).collect())
            }
            Ty::Named { .. } => {
                let (names, _) = self.struct_field_tys(&occ.ty, span)?;
                names.iter().map(|n| {
                    let (expr, ty) = self.field_read(base.clone(), &occ.ty, n, span)?;
                    Ok(Occ { expr, ty })
                }).collect()
            }
            _ => Ok(vec![]),
        }
    }

    /// The Core for `tree`. Each leaf runs its arm's code, or (for a shared
    /// arm) leaves to it; with an `end` label every path leaves through it.
    fn emit(&mut self, tree: Tree, arms: &mut [Option<ArmCode>], shared: &[Option<u32>], end: Option<u32>, repr: &Repr, span: Span) -> LResult<CoreExpr> {
        let block = |stmts: Vec<CoreStmt>, tail: CoreExpr, repr: &Repr| CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts, tail: Some(tail) })), repr.clone());
        Ok(match tree {
            Tree::Fail => {
                let msg = CoreExpr::new(CoreExprKind::ConstStr("no match arm applies (compiler invariant)".into()), Repr::Ref(self.string_layout_id(span)?));
                CoreExpr::new(CoreExprKind::Panic(Box::new(msg)), Repr::Unit)
            }
            Tree::Leaf { arm, rest } => {
                let fallback = match rest { Some(r) => Some(self.emit(*r, arms, shared, end, repr, span)?), None => None };
                match shared[arm] {
                    Some(label) => {
                        // Bind before the guard only; the body binds otherwise.
                        let go = CoreExpr::new(CoreExprKind::Leave { label, value: None }, Repr::Unit);
                        let code = arms[arm].as_ref().unwrap();
                        match (&code.guard, fallback) {
                            (Some(guard), Some(fallback)) => {
                                let stmts = code.binds.iter().map(|(id, init)| CoreStmt::Let(*id, init.clone())).collect();
                                let test = CoreExpr::new(CoreExprKind::If(Box::new(guard.clone()),
                                    Box::new(CoreBlock { stmts: vec![], tail: Some(go) }),
                                    Box::new(CoreBlock { stmts: vec![], tail: Some(fallback) })), Repr::Unit);
                                block(stmts, test, &Repr::Unit)
                            }
                            _ => go,
                        }
                    }
                    None => {
                        let code = arms[arm].take().expect("an unshared arm has one leaf");
                        let stmts: Vec<CoreStmt> = code.binds.into_iter().map(|(id, init)| CoreStmt::Let(id, init)).collect();
                        let body = match end { Some(end) => leave_with(end, code.body, repr), None => code.body };
                        let out_repr = if end.is_some() { Repr::Unit } else { repr.clone() };
                        let run = match (code.guard, fallback) {
                            (Some(guard), Some(fallback)) => CoreExpr::new(CoreExprKind::If(Box::new(guard),
                                Box::new(CoreBlock { stmts: vec![], tail: Some(body) }),
                                Box::new(CoreBlock { stmts: vec![], tail: Some(fallback) })), out_repr.clone()),
                            _ => body,
                        };
                        block(stmts, run, &out_repr)
                    }
                }
            }
            Tree::Lits { occ, local, cases, default } => {
                let out_repr = if end.is_some() { Repr::Unit } else { repr.clone() };
                let read = CoreExpr::new(CoreExprKind::Local(local), occ.expr.repr.clone());
                let mut chain = self.emit(*default, arms, shared, end, repr, span)?;
                let mut built = Vec::new();
                for (test, sub) in cases { built.push((test, self.emit(sub, arms, shared, end, repr, span)?)); }
                for (test, code) in built.into_iter().rev() {
                    let cond = match &test {
                        LitTest::Lit(lit) => self.literal_eq(&read, lit, &occ.ty, span)?,
                        LitTest::Const(path) => self.const_eq(&read, &occ.ty, path, span)?,
                    };
                    chain = CoreExpr::new(CoreExprKind::If(Box::new(cond),
                        Box::new(CoreBlock { stmts: vec![], tail: Some(code) }),
                        Box::new(CoreBlock { stmts: vec![], tail: Some(chain) })), out_repr.clone());
                }
                block(vec![CoreStmt::Let(local, occ.expr)], chain, &out_repr)
            }
            Tree::Switch { occ, local, cases, default } => {
                let out_repr = if end.is_some() { Repr::Unit } else { repr.clone() };
                let read = CoreExpr::new(CoreExprKind::Local(local), occ.expr.repr.clone());
                let default = match default { Some(d) => Some(self.emit(*d, arms, shared, end, repr, span)?), None => None };
                let mut emitted = Vec::new();
                for (tag, sub) in cases { emitted.push((tag, self.emit(sub, arms, shared, end, repr, span)?)); }
                let switch = if matches!(occ.ty, Ty::Prim(Prim::Bool)) {
                    let branch = |want: u32, emitted: &mut Vec<(u32, CoreExpr)>, default: &Option<CoreExpr>| -> CoreExpr {
                        match emitted.iter().position(|(t, _)| *t == want) {
                            Some(i) => emitted.remove(i).1,
                            None => default.clone().expect("a missing bool case has a default"),
                        }
                    };
                    let yes = branch(1, &mut emitted, &default);
                    let no = branch(0, &mut emitted, &default);
                    CoreExpr::new(CoreExprKind::If(Box::new(read),
                        Box::new(CoreBlock { stmts: vec![], tail: Some(yes) }),
                        Box::new(CoreBlock { stmts: vec![], tail: Some(no) })), out_repr.clone())
                } else {
                    let mut carms: Vec<CoreArm> = emitted.into_iter().map(|(tag, body)| CoreArm { tag, binds: vec![], body }).collect();
                    if let Some(d) = default { carms.push(CoreArm { tag: u32::MAX, binds: vec![], body: d }); }
                    let kind = if matches!(read.repr, Repr::Value(_)) {
                        CoreExprKind::ValueMatch { scrutinee: Box::new(read), arms: carms }
                    } else {
                        CoreExprKind::Match { scrutinee: Box::new(read), arms: carms }
                    };
                    CoreExpr::new(kind, out_repr.clone())
                };
                block(vec![CoreStmt::Let(local, occ.expr)], switch, &out_repr)
            }
        })
    }
}

fn count_leaves(tree: &Tree, out: &mut [usize]) {
    match tree {
        Tree::Fail => {}
        Tree::Leaf { arm, rest } => {
            out[*arm] += 1;
            if let Some(r) = rest { count_leaves(r, out); }
        }
        Tree::Switch { cases, default, .. } => {
            for (_, t) in cases { count_leaves(t, out); }
            if let Some(d) = default { count_leaves(d, out); }
        }
        Tree::Lits { cases, default, .. } => {
            for (_, t) in cases { count_leaves(t, out); }
            count_leaves(default, out);
        }
    }
}

/// `leave label` with `body`'s value (or after running it, for a unit match).
fn leave_with(label: u32, body: CoreExpr, repr: &Repr) -> CoreExpr {
    if matches!(repr, Repr::Unit) {
        let leave = CoreExpr::new(CoreExprKind::Leave { label, value: None }, Repr::Unit);
        CoreExpr::new(CoreExprKind::Block(Box::new(CoreBlock { stmts: vec![CoreStmt::Expr(body)], tail: Some(leave) })), Repr::Unit)
    } else {
        CoreExpr::new(CoreExprKind::Leave { label, value: Some(Box::new(body)) }, Repr::Unit)
    }
}
