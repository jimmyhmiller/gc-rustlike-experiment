//! Matches on values built by known constructors. Inlining leaves Rust's
//! `Option`/`Result` plumbing (`v.last()?`, `pop().ok_or_else(..)?`,
//! `unwrap()`) as a value built in each branch and matched right after:
//!
//! ```text
//! let r = if c { None } else { Some(x) };
//! let s = match r { Some(v) => A, None => B };
//! ```
//!
//! The match moves into the branches, where the constructor is known, so the
//! value is never built:
//!
//! ```text
//! let s = if c { B } else { let v = x; A };
//! ```
//!
//! This applies when `r` is used only by the match (its scrutinee, and its
//! arms' payload reads), and every way `r`'s value is produced is a
//! constructor of the matched enum or a path that does not produce one (a
//! `return`, a panic). An arm body is placed once per constructor that
//! reaches it; a body reached more than once must be small and bind nothing,
//! so copying it introduces no local twice.
use crate::core::*;

pub fn known_constructors(prog: &mut CoreProgram) {
    if !crate::inline::enabled() { return; }
    let diverging = crate::codegen_effects::diverging_functions(prog);
    for f in &mut prog.funcs {
        if f.is_extern { continue; }
        let mut counts = Counts { uses: vec![0; f.locals.len()], payload_reads: vec![0; f.locals.len()] };
        count_block(&f.body, &mut counts);
        let mut cx = Cx { counts, diverging: &diverging };
        cx.block(&mut f.body);
    }
}

struct Cx<'a> {
    /// Over-approximate: a copied arm adds its uses.
    counts: Counts,
    diverging: &'a [bool],
}

/// For each local: its uses, other than reads of its payload, and those.
struct Counts {
    uses: Vec<u32>,
    payload_reads: Vec<u32>,
}

/// A way the value of an expression is produced.
enum Leaf { Known(u32), Diverges, Unknown }

impl Cx<'_> {
    fn block(&mut self, b: &mut CoreBlock) {
        let mut i = 0;
        while i < b.stmts.len() {
            if self.try_fuse(b, i) { continue; }
            match &mut b.stmts[i] { CoreStmt::Let(_, e) | CoreStmt::Expr(e) => self.expr(e) }
            i += 1;
        }
        if let Some(t) = &mut b.tail { self.expr(t); }
    }

    fn expr(&mut self, e: &mut CoreExpr) {
        use CoreExprKind::*;
        match e.kind.as_mut() {
            If(c, y, n) => { self.expr(c); self.block(y); self.block(n); }
            Block(b) | Loop(b) | Once { body: b, .. } => self.block(b),
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms { self.expr(&mut a.body); }
            }
            _ => for c in crate::copyprop::children_mut(e) { self.expr(c) },
        }
    }

    /// Fuse `let r = E` (statement `i`) with a match on `r` right after it.
    fn try_fuse(&mut self, b: &mut CoreBlock, i: usize) -> bool {
        let CoreStmt::Let(r, def) = &b.stmts[i] else { return false };
        let r = *r;
        if self.counts.uses[r as usize] != 1 { return false; }
        // The match: the next statement's value, or the block's tail.
        let next_is_tail = i + 1 == b.stmts.len();
        let m = if next_is_tail {
            match &b.tail { Some(t) => peel(t), None => return false }
        } else {
            match &b.stmts[i + 1] { CoreStmt::Let(_, e) | CoreStmt::Expr(e) => peel(e) }
        };
        let (arms, matched) = match m.kind.as_ref() {
            CoreExprKind::ValueMatch { scrutinee, arms } | CoreExprKind::Match { scrutinee, arms }
                if matches!(scrutinee.kind.as_ref(), CoreExprKind::Local(l) if *l == r) => (arms, scrutinee.repr.clone()),
            _ => return false,
        };
        // Every way `r` gets its value must be a known constructor.
        let mut leaves = Vec::new();
        leaves_of(def, &matched, self.diverging, &mut leaves);
        if leaves.iter().any(|l| matches!(l, Leaf::Unknown)) { return false; }
        // An arm reached by several constructors is copied: only small
        // bodies that bind nothing.
        let mut reached = vec![0usize; arms.len()];
        for leaf in &leaves {
            if let Leaf::Known(tag) = leaf {
                match arm_for(arms, *tag) { Some(k) => reached[k] += 1, None => return false }
            }
        }
        for (k, n) in reached.iter().enumerate() {
            if *n > 1 && !(arms[k].binds.is_empty() && small(&arms[k].body)) { return false; }
        }
        // Every payload read of `r` is in the match's arms.
        let mut inside = Counts { uses: vec![0; self.counts.uses.len()], payload_reads: vec![0; self.counts.uses.len()] };
        for a in arms.iter() { count_expr(&a.body, &mut inside); }
        if inside.payload_reads[r as usize] != self.counts.payload_reads[r as usize] { return false; }
        // A copied arm, and constructor fields standing for payload reads,
        // add uses.
        for (k, n) in reached.iter().enumerate() {
            for _ in 1..*n { count_expr(&arms[k].body, &mut self.counts); }
        }
        for leaf in &leaves {
            if let Leaf::Known(tag) = leaf {
                let k = arm_for(arms, *tag).unwrap();
                let reads = { let mut c = Counts { uses: vec![0; self.counts.uses.len()], payload_reads: vec![0; self.counts.uses.len()] }; count_expr(&arms[k].body, &mut c); c.payload_reads[r as usize] };
                if reads > 0 {
                    // Each field may be read up to `reads` times.
                    let mut fields_uses = Counts { uses: vec![0; self.counts.uses.len()], payload_reads: vec![0; self.counts.uses.len()] };
                    count_constructor_fields(def, *tag, &mut fields_uses);
                    for (l, n) in fields_uses.uses.iter().enumerate() { self.counts.uses[l] += n * reads; }
                }
            }
        }
        let result = m.repr.clone();
        let arms = arms.clone();
        // Rewrite: the match's value is the fused definition's.
        let CoreStmt::Let(_, def) = std::mem::replace(&mut b.stmts[i], CoreStmt::Expr(CoreExpr::new(CoreExprKind::Unit, Repr::Unit))) else { unreachable!() };
        let fused = push_match(def, r, &arms, &result, self.diverging);
        if next_is_tail {
            b.stmts.remove(i);
            b.tail = Some(fused);
        } else {
            let next = b.stmts.remove(i + 1);
            b.stmts[i] = match next {
                CoreStmt::Let(s, _) => CoreStmt::Let(s, fused),
                CoreStmt::Expr(_) => CoreStmt::Expr(fused),
            };
        }
        true
    }
}

/// `e` without blocks that only wrap a tail.
fn peel(e: &CoreExpr) -> &CoreExpr {
    match e.kind.as_ref() {
        CoreExprKind::Block(b) if b.stmts.is_empty() => match &b.tail { Some(t) => peel(t), None => e },
        _ => e,
    }
}

fn arm_for(arms: &[CoreArm], tag: u32) -> Option<usize> {
    arms.iter().position(|a| a.tag == tag).or_else(|| arms.iter().position(|a| a.tag == u32::MAX))
}

fn small(e: &CoreExpr) -> bool {
    let mut n = 0;
    let mut lets = false;
    crate::inline::visit_expr(e, &mut |x| {
        n += 1;
        if let CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } = x.kind.as_ref() {
            if b.stmts.iter().any(|s| matches!(s, CoreStmt::Let(..))) { lets = true; }
        }
        if let CoreExprKind::If(_, y, z) = x.kind.as_ref() {
            if y.stmts.iter().chain(&z.stmts).any(|s| matches!(s, CoreStmt::Let(..))) { lets = true; }
        }
        if matches!(x.kind.as_ref(), CoreExprKind::Match { .. } | CoreExprKind::ValueMatch { .. } | CoreExprKind::MakeClosure { .. }) { lets = true; }
    });
    !lets && n <= 12
}

/// The ways `e` produces its value: constructors of the matched enum (of
/// repr `matched`), paths that leave instead, or anything else.
fn leaves_of(e: &CoreExpr, matched: &Repr, diverging: &[bool], out: &mut Vec<Leaf>) {
    use CoreExprKind::*;
    match e.kind.as_ref() {
        MakeValueVariant { value, tag, .. } if matches!(matched, Repr::Value(v) if v == value) => out.push(Leaf::Known(*tag)),
        MakeVariant { layout, tag, .. } if matches!(matched, Repr::Ref(l) if l == layout) => out.push(Leaf::Known(*tag)),
        Block(b) => block_leaves(b, matched, diverging, out),
        If(_, y, n) => { block_leaves(y, matched, diverging, out); block_leaves(n, matched, diverging, out); }
        Match { arms, .. } | ValueMatch { arms, .. } => for a in arms { leaves_of(&a.body, matched, diverging, out) },
        _ if crate::codegen_effects::expr_diverges(e, diverging) => out.push(Leaf::Diverges),
        Return(_) | Break(_) | Continue | Leave { .. } => out.push(Leaf::Diverges),
        _ => out.push(Leaf::Unknown),
    }
}

fn block_leaves(b: &CoreBlock, matched: &Repr, diverging: &[bool], out: &mut Vec<Leaf>) {
    let stops = b.stmts.iter().any(|s| match s {
        CoreStmt::Let(_, e) | CoreStmt::Expr(e) => crate::codegen_effects::expr_diverges(e, diverging)
            || matches!(e.kind.as_ref(), CoreExprKind::Return(_) | CoreExprKind::Break(_) | CoreExprKind::Continue | CoreExprKind::Leave { .. }),
    });
    match &b.tail {
        _ if stops => out.push(Leaf::Diverges),
        Some(t) => leaves_of(t, matched, diverging, out),
        None => out.push(Leaf::Unknown),
    }
}

/// `e` with each constructor that produces its value replaced by the arm of
/// `arms` it selects; the result has the match's repr.
fn push_match(mut e: CoreExpr, r: LocalId, arms: &[CoreArm], result: &Repr, diverging: &[bool]) -> CoreExpr {
    use CoreExprKind::*;
    let known = match e.kind.as_ref() {
        MakeValueVariant { tag, .. } | MakeVariant { tag, .. } => Some(*tag),
        _ => None,
    };
    if let Some(tag) = known {
        let fields = match *e.kind {
            MakeValueVariant { fields, .. } | MakeVariant { fields, .. } => fields,
            _ => unreachable!(),
        };
        let arm = &arms[arm_for(arms, tag).expect("an arm for every constructor")];
        let mut body = arm.body.clone();
        replace_payload_reads(&mut body, r, &fields);
        let stmts = arm.binds.iter().zip(fields).map(|(b, f)| CoreStmt::Let(*b, f)).collect();
        return CoreExpr::new(Block(Box::new(CoreBlock { stmts, tail: Some(body) })), result.clone());
    }
    let span = e.span;
    let kind = match *e.kind {
        Block(mut b) => { push_block(&mut b, r, arms, result, diverging); Block(b) }
        If(c, mut y, mut n) => {
            push_block(&mut y, r, arms, result, diverging);
            push_block(&mut n, r, arms, result, diverging);
            If(c, y, n)
        }
        Match { scrutinee, arms: inner } => Match { scrutinee, arms: push_arms(inner, r, arms, result, diverging) },
        ValueMatch { scrutinee, arms: inner } => ValueMatch { scrutinee, arms: push_arms(inner, r, arms, result, diverging) },
        // A path that leaves keeps its own repr.
        other => { *e.kind = other; return e; }
    };
    CoreExpr { kind: Box::new(kind), repr: result.clone(), span }
}

fn push_arms(inner: Vec<CoreArm>, r: LocalId, arms: &[CoreArm], result: &Repr, diverging: &[bool]) -> Vec<CoreArm> {
    inner.into_iter().map(|a| CoreArm { tag: a.tag, binds: a.binds, body: push_match(a.body, r, arms, result, diverging) }).collect()
}

fn push_block(b: &mut CoreBlock, r: LocalId, arms: &[CoreArm], result: &Repr, diverging: &[bool]) {
    let stops = b.stmts.iter().any(|s| match s {
        CoreStmt::Let(_, e) | CoreStmt::Expr(e) => crate::codegen_effects::expr_diverges(e, diverging)
            || matches!(e.kind.as_ref(), CoreExprKind::Return(_) | CoreExprKind::Break(_) | CoreExprKind::Continue | CoreExprKind::Leave { .. }),
    });
    if stops { return; }
    if let Some(t) = b.tail.take() { b.tail = Some(push_match(t, r, arms, result, diverging)); }
}

/// In an arm's body, a read of payload field `i` of `r` is the constructor's
/// field `i`.
fn replace_payload_reads(e: &mut CoreExpr, r: LocalId, fields: &[CoreExpr]) {
    if let CoreExprKind::EnumPayload { scrutinee, field, .. } = e.kind.as_ref() {
        if matches!(scrutinee.kind.as_ref(), CoreExprKind::Local(l) if *l == r) {
            *e = fields[*field as usize].clone();
            return;
        }
    }
    match e.kind.as_mut() {
        CoreExprKind::If(c, y, n) => { replace_payload_reads(c, r, fields); replace_block(y, r, fields); replace_block(n, r, fields); }
        CoreExprKind::Block(b) | CoreExprKind::Loop(b) | CoreExprKind::Once { body: b, .. } => replace_block(b, r, fields),
        CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
            replace_payload_reads(scrutinee, r, fields);
            for a in arms { replace_payload_reads(&mut a.body, r, fields); }
        }
        _ => for c in crate::copyprop::children_mut(e) { replace_payload_reads(c, r, fields) },
    }
}

fn replace_block(b: &mut CoreBlock, r: LocalId, fields: &[CoreExpr]) {
    for s in &mut b.stmts { match s { CoreStmt::Let(_, e) | CoreStmt::Expr(e) => replace_payload_reads(e, r, fields) } }
    if let Some(t) = &mut b.tail { replace_payload_reads(t, r, fields); }
}

fn count_block(b: &CoreBlock, counts: &mut Counts) {
    for s in &b.stmts {
        match s {
            CoreStmt::Let(_, e) | CoreStmt::Expr(e) => count_expr(e, counts),
        }
    }
    if let Some(t) = &b.tail { count_expr(t, counts); }
}

fn count_expr(e: &CoreExpr, counts: &mut Counts) {
    use CoreExprKind::*;
    match e.kind.as_ref() {
        Local(l) => counts.uses[*l as usize] += 1,
        EnumPayload { scrutinee, .. } if matches!(scrutinee.kind.as_ref(), Local(_)) => {
            let Local(l) = scrutinee.kind.as_ref() else { unreachable!() };
            counts.payload_reads[*l as usize] += 1;
        }
        // An assigned local is not fused.
        Assign { local, value } => { counts.uses[*local as usize] += 2; count_expr(value, counts); }
        If(c, y, n) => { count_expr(c, counts); count_block(y, counts); count_block(n, counts); }
        Block(b) | Loop(b) | Once { body: b, .. } => count_block(b, counts),
        Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
            count_expr(scrutinee, counts);
            for a in arms { count_expr(&a.body, counts); }
        }
        _ => for c in crate::copyprop::children(e) { count_expr(c, counts) },
    }
}

/// The uses in the fields of the constructors of `tag` that produce `e`.
fn count_constructor_fields(e: &CoreExpr, tag: u32, counts: &mut Counts) {
    use CoreExprKind::*;
    match e.kind.as_ref() {
        MakeValueVariant { tag: t, fields, .. } | MakeVariant { tag: t, fields, .. } if *t == tag => {
            for f in fields { count_expr(f, counts); }
        }
        Block(b) => if let Some(t) = &b.tail { count_constructor_fields(t, tag, counts) },
        If(_, y, n) => {
            if let Some(t) = &y.tail { count_constructor_fields(t, tag, counts); }
            if let Some(t) = &n.tail { count_constructor_fields(t, tag, counts); }
        }
        Match { arms, .. } | ValueMatch { arms, .. } => for a in arms { count_constructor_fields(&a.body, tag, counts) },
        _ => {}
    }
}
