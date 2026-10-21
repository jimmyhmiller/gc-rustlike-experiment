//! Shared mutable captures.
//!
//! A closure captures a local by copying its value into the closure's
//! environment. Rust's `FnMut` closures assign to the captured variable
//! itself (`let mut n = 0; let mut inc = || n += 1;`), so the change is seen
//! outside. To keep that meaning, a `let mut x` that some closure assigns to
//! is stored in a heap cell: the binding holds `MutCell { v: init }` and each
//! use of `x` in its scope reads or writes `x.v`. Closures capture the cell,
//! so every copy refers to the same storage.

use crate::ast::*;
use std::sync::Arc;

/// Rewrite every block in `body`.
pub fn share_mutated_captures(body: &mut Block) {
    block(body);
}

fn block(b: &mut Block) {
    let mut i = 0;
    while i < b.stmts.len() {
        if let Stmt::Let { pattern: Pattern { kind: PatternKind::Binding { is_mut: true, name }, .. }, .. } = &b.stmts[i] {
            let name = name.clone();
            let rest_assigns = b.stmts[i + 1..].iter().any(|s| stmt_closure_assigns(s, &name))
                || b.tail.as_ref().is_some_and(|t| closure_assigns(t, &name, false));
            if rest_assigns {
                if let Stmt::Let { ty, init, span, .. } = &mut b.stmts[i] {
                    let span = *span;
                    if let Some(t) = ty.take() {
                        *ty = Some(Type { kind: TypeKind::Path(Path::single("MutCell".into(), span), vec![t]), span });
                    }
                    if let Some(e) = init.take() {
                        *init = Some(cell(e, span));
                    }
                }
                // Uses in the rest of the block go through the cell, until a
                // later `let` of the same name shadows it.
                let mut j = i + 1;
                while j < b.stmts.len() {
                    let shadows = stmt_rewrite(&mut b.stmts[j], &name);
                    if shadows { break; }
                    j += 1;
                }
                if j == b.stmts.len() {
                    if let Some(t) = &mut b.tail { rewrite(t, &name); }
                }
            }
        }
        // Nested blocks get the same treatment for their own lets.
        match &mut b.stmts[i] {
            Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) => expr_blocks(e),
            _ => {}
        }
        i += 1;
    }
    if let Some(t) = &mut b.tail { expr_blocks(t); }
}

fn cell(init: Expr, span: Span) -> Expr {
    let ex = |kind: ExprKind| Expr { kind: Arc::new(kind), span };
    ex(ExprKind::StructLit {
        path: Path::single("MutCell".into(), span),
        fields: vec![FieldInit { name: "v".into(), value: Some(init), span }],
        span,
    })
}

use crate::lexer::Span;

/// Visit the blocks nested in an expression.
fn expr_blocks(e: &mut Expr) {
    let kind = Arc::make_mut(&mut e.kind);
    match kind {
        ExprKind::Block(b) | ExprKind::Unsafe(b) | ExprKind::Loop { body: b } => block(b),
        ExprKind::If { cond, then_branch, else_branch } => {
            expr_blocks(cond);
            block(then_branch);
            if let Some(e) = else_branch { expr_blocks(e); }
        }
        ExprKind::While { cond, body } => { expr_blocks(cond); block(body); }
        ExprKind::For { iter, body, .. } => { expr_blocks(iter); block(body); }
        ExprKind::Match { scrutinee, arms } => {
            expr_blocks(scrutinee);
            for a in arms {
                if let Some(g) = &mut a.guard { expr_blocks(g); }
                expr_blocks(&mut a.body);
            }
        }
        ExprKind::Closure { body, .. } => expr_blocks(body),
        _ => each_child(kind, &mut expr_blocks),
    }
}

/// Apply `f` to the direct child expressions of non-binding expression kinds.
fn each_child(kind: &mut ExprKind, f: &mut dyn FnMut(&mut Expr)) {
    match kind {
        ExprKind::Int(..) | ExprKind::Float(..) | ExprKind::Str(_) | ExprKind::Char(_) | ExprKind::Bool(_)
        | ExprKind::Unit | ExprKind::Path(_) | ExprKind::Continue => {}
        ExprKind::Call(c, args) => { f(c); for a in args { f(a); } }
        ExprKind::MethodCall { recv, args, .. } => { f(recv); for a in args { f(a); } }
        ExprKind::Field { base, .. } => f(base),
        ExprKind::Index { base, index } => { f(base); f(index); }
        ExprKind::Unary(_, e) | ExprKind::Cast(e, _) | ExprKind::Try(e) => f(e),
        ExprKind::Binary(_, a, b) => { f(a); f(b); }
        ExprKind::Assign { target, value, .. } => { f(target); f(value); }
        ExprKind::StructLit { fields, .. } => for fi in fields { if let Some(v) = &mut fi.value { f(v); } },
        ExprKind::Tuple(es) => for e in es { f(e); },
        ExprKind::Array(ArrayLit::Elems(es)) => for e in es { f(e); },
        ExprKind::Array(ArrayLit::Repeat(a, b)) => { f(a); f(b); }
        ExprKind::Return(e) | ExprKind::Break(e) => if let Some(e) = e { f(e); },
        ExprKind::Range { lo, hi, .. } => { if let Some(e) = lo { f(e); } if let Some(e) = hi { f(e); } }
        // Kinds with blocks or bindings are handled by the callers.
        ExprKind::Block(_) | ExprKind::Unsafe(_) | ExprKind::If { .. } | ExprKind::Match { .. }
        | ExprKind::While { .. } | ExprKind::Loop { .. } | ExprKind::For { .. } | ExprKind::Closure { .. } => {}
    }
}

fn stmt_closure_assigns(s: &Stmt, name: &str) -> bool {
    match s {
        Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) => closure_assigns(e, name, false),
        _ => false,
    }
}

/// Does a closure in `e` assign to the outer `name` (`name = ..`, `name += ..`)?
/// `inside` is true within a closure body that still sees the outer `name`.
fn closure_assigns(e: &Expr, name: &str, inside: bool) -> bool {
    match &*e.kind {
        ExprKind::Assign { target, value, .. } => {
            (inside && matches!(&*target.kind, ExprKind::Path(p) if p.is_single() && p.last() == name))
                || closure_assigns(target, name, inside) || closure_assigns(value, name, inside)
        }
        ExprKind::Closure { params, body, .. } => {
            !params.iter().any(|p| p.name == name) && closure_assigns(body, name, true)
        }
        ExprKind::Block(b) | ExprKind::Unsafe(b) | ExprKind::Loop { body: b } => block_assigns(b, name, inside),
        ExprKind::If { cond, then_branch, else_branch } => {
            closure_assigns(cond, name, inside) || block_assigns(then_branch, name, inside)
                || else_branch.as_ref().is_some_and(|e| closure_assigns(e, name, inside))
        }
        ExprKind::While { cond, body } => closure_assigns(cond, name, inside) || block_assigns(body, name, inside),
        ExprKind::For { pat, iter, body } => {
            closure_assigns(iter, name, inside) || (!pattern_binds(pat, name) && block_assigns(body, name, inside))
        }
        ExprKind::Match { scrutinee, arms } => {
            closure_assigns(scrutinee, name, inside) || arms.iter().any(|a| !pattern_binds(&a.pattern, name)
                && (a.guard.as_ref().is_some_and(|g| closure_assigns(g, name, inside)) || closure_assigns(&a.body, name, inside)))
        }
        kind => {
            let mut found = false;
            let mut kind = kind.clone();
            each_child(&mut kind, &mut |c| { found = found || closure_assigns(c, name, inside); });
            found
        }
    }
}

fn block_assigns(b: &Block, name: &str, inside: bool) -> bool {
    for s in &b.stmts {
        match s {
            Stmt::Let { pattern, init, .. } => {
                if init.as_ref().is_some_and(|e| closure_assigns(e, name, inside)) { return true; }
                if pattern_binds(pattern, name) { return false; }
            }
            Stmt::Expr(e) => if closure_assigns(e, name, inside) { return true; },
            Stmt::Item(_) => {}
        }
    }
    b.tail.as_ref().is_some_and(|t| closure_assigns(t, name, inside))
}

pub(crate) fn pattern_binds(p: &Pattern, name: &str) -> bool {
    match &p.kind {
        PatternKind::Binding { name: n, .. } => n == name,
        PatternKind::Variant { payload, .. } => payload.iter().any(|q| pattern_binds(q, name)),
        PatternKind::Struct { fields, .. } => fields.iter().any(|(_, q)| pattern_binds(q, name)),
        PatternKind::Tuple(ps) => ps.iter().any(|q| pattern_binds(q, name)),
        PatternKind::Wildcard | PatternKind::Literal(_) => false,
    }
}

/// Rewrite uses of `name` in a statement. Returns true when the statement is
/// a `let` that shadows `name` (its initializer is still rewritten).
fn stmt_rewrite(s: &mut Stmt, name: &str) -> bool {
    match s {
        Stmt::Let { pattern, init, .. } => {
            if let Some(e) = init { rewrite(e, name); }
            pattern_binds(pattern, name)
        }
        Stmt::Expr(e) => { rewrite(e, name); false }
        Stmt::Item(_) => false,
    }
}

fn rewrite_block(b: &mut Block, name: &str) {
    for s in &mut b.stmts {
        if stmt_rewrite(s, name) { return; }
    }
    if let Some(t) = &mut b.tail { rewrite(t, name); }
}

/// Replace each use of the outer `name` in `e` with `name.v`.
fn rewrite(e: &mut Expr, name: &str) {
    let span = e.span;
    if let ExprKind::Path(p) = &*e.kind {
        if p.is_single() && p.last() == name {
            let base = e.clone();
            *e = Expr { kind: Arc::new(ExprKind::Field { base, field: FieldAccess::Named("v".into()) }), span };
        }
        return;
    }
    let kind = Arc::make_mut(&mut e.kind);
    match kind {
        ExprKind::Block(b) | ExprKind::Unsafe(b) | ExprKind::Loop { body: b } => rewrite_block(b, name),
        ExprKind::If { cond, then_branch, else_branch } => {
            rewrite(cond, name);
            rewrite_block(then_branch, name);
            if let Some(e) = else_branch { rewrite(e, name); }
        }
        ExprKind::While { cond, body } => { rewrite(cond, name); rewrite_block(body, name); }
        ExprKind::For { pat, iter, body } => {
            rewrite(iter, name);
            if !pattern_binds(pat, name) { rewrite_block(body, name); }
        }
        ExprKind::Match { scrutinee, arms } => {
            rewrite(scrutinee, name);
            for a in arms {
                if pattern_binds(&a.pattern, name) { continue; }
                if let Some(g) = &mut a.guard { rewrite(g, name); }
                rewrite(&mut a.body, name);
            }
        }
        ExprKind::Closure { params, body, .. } => {
            if !params.iter().any(|p| p.name == name) { rewrite(body, name); }
        }
        // `Struct { name }` shorthand reads the variable.
        ExprKind::StructLit { fields, .. } => {
            for fi in fields {
                match &mut fi.value {
                    Some(v) => rewrite(v, name),
                    None if fi.name == name => {
                        let mut v = Expr { kind: Arc::new(ExprKind::Path(Path::single(name.to_string(), fi.span))), span: fi.span };
                        rewrite(&mut v, name);
                        fi.value = Some(v);
                    }
                    None => {}
                }
            }
        }
        _ => each_child(kind, &mut |c| rewrite(c, name)),
    }
}
