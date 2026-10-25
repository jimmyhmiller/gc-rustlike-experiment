//! Remove unused local storage while retaining every evaluated expression.
//! Inlining and scalar replacement leave many old local declarations behind;
//! those should not become LLVM allocas or points-to/liveness graph nodes.
use crate::core::*;

pub(crate) fn compact(program: &mut CoreProgram) {
    if !crate::inline::enabled() { return; }
    for function in &mut program.funcs {
        if function.is_extern { continue; }
        let mut read = vec![false; function.locals.len()];
        crate::inline::visit_block(&function.body, &mut |expression| {
            if let CoreExprKind::Local(local) = expression.kind.as_ref() { read[*local as usize] = true; }
        });
        erase_block(&mut function.body, &read);
        let mut used = vec![false; function.locals.len()];
        for live in used.iter_mut().take(function.closure_captures.len() + function.params.len()) { *live = true; }
        for capture in &function.closure_captures { used[capture.local as usize] = true; }
        mark_block(&function.body, &mut used);
        let mut ids = vec![None; used.len()];
        let mut locals = Vec::new();
        let mut names = Vec::new();
        for (old, repr) in function.locals.iter().enumerate() {
            if used[old] {
                ids[old] = Some(locals.len() as LocalId);
                locals.push(repr.clone());
                names.push(function.local_names.get(old).cloned().unwrap_or(None));
            }
        }
        remap_block(&mut function.body, &ids);
        for capture in &mut function.closure_captures { capture.local = ids[capture.local as usize].unwrap(); }
        function.locals = locals;
        function.local_names = names;
    }
}
fn erase_block(block: &mut CoreBlock, read: &[bool]) {
    for statement in &mut block.stmts {
        let (CoreStmt::Let(_, expression) | CoreStmt::Expr(expression)) = statement;
        erase_expr(expression, read);
        // Only discard a store at statement position. Introducing a new Block
        // around a nested assignment would change native-buffer cleanup scope.
        if let CoreStmt::Expr(value) = statement {
            if value.repr == Repr::Unit && matches!(value.kind.as_ref(), CoreExprKind::Assign { local, .. } if !read[*local as usize]) {
                let CoreExprKind::Assign { value: rhs, .. } = std::mem::replace(value.kind.as_mut(), CoreExprKind::Unit) else { unreachable!() };
                *value = *rhs;
            }
        }
        if matches!(statement, CoreStmt::Let(local, _) if !read[*local as usize]) {
            let CoreStmt::Let(_, value) = std::mem::replace(statement, CoreStmt::Expr(CoreExpr::new(CoreExprKind::Unit, Repr::Unit))) else { unreachable!() };
            *statement = CoreStmt::Expr(value);
        }
    }
    if let Some(tail) = &mut block.tail { erase_expr(tail, read); }
}
fn erase_expr(expression: &mut CoreExpr, read: &[bool]) {
    use CoreExprKind::*;
    match expression.kind.as_mut() {
        If(condition, yes, no) => { erase_expr(condition, read); erase_block(yes, read); erase_block(no, read); }
        Block(block) | Loop(block) | Once { body: block, .. } => erase_block(block, read),
        Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
            erase_expr(scrutinee, read);
            for arm in arms { erase_expr(&mut arm.body, read); }
        }
        _ => for child in crate::copyprop::children_mut(expression) { erase_expr(child, read); },
    }

}
fn mark_block(block: &CoreBlock, used: &mut [bool]) {
    for statement in &block.stmts {
        let expression = match statement {
            CoreStmt::Let(local, expression) => { used[*local as usize] = true; expression }
            CoreStmt::Expr(expression) => expression,
        };
        mark_expr(expression, used);
    }
    if let Some(tail) = &block.tail { mark_expr(tail, used); }
}
fn mark_expr(expression: &CoreExpr, used: &mut [bool]) {
    use CoreExprKind::*;
    match expression.kind.as_ref() {
        Local(local) => used[*local as usize] = true,
        Assign { local, value } => { used[*local as usize] = true; mark_expr(value, used); }
        If(condition, yes, no) => { mark_expr(condition, used); mark_block(yes, used); mark_block(no, used); }
        Block(block) | Loop(block) | Once { body: block, .. } => mark_block(block, used),
        Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
            mark_expr(scrutinee, used);
            for arm in arms {
                for local in &arm.binds { used[*local as usize] = true; }
                mark_expr(&arm.body, used);
            }
        }
        _ => for child in crate::copyprop::children(expression) { mark_expr(child, used); },
    }
}
fn remap_block(block: &mut CoreBlock, ids: &[Option<LocalId>]) {
    for statement in &mut block.stmts {
        let expression = match statement {
            CoreStmt::Let(local, expression) => { *local = ids[*local as usize].unwrap(); expression }
            CoreStmt::Expr(expression) => expression,
        };
        remap_expr(expression, ids);
    }
    if let Some(tail) = &mut block.tail { remap_expr(tail, ids); }
}
pub(crate) fn remap_expr(expression: &mut CoreExpr, ids: &[Option<LocalId>]) {
    use CoreExprKind::*;
    match expression.kind.as_mut() {
        Local(local) => *local = ids[*local as usize].unwrap(),
        Assign { local, value } => { *local = ids[*local as usize].unwrap(); remap_expr(value, ids); }
        If(condition, yes, no) => { remap_expr(condition, ids); remap_block(yes, ids); remap_block(no, ids); }
        Block(block) | Loop(block) | Once { body: block, .. } => remap_block(block, ids),
        Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
            remap_expr(scrutinee, ids);
            for arm in arms {
                for local in &mut arm.binds { *local = ids[*local as usize].unwrap(); }
                remap_expr(&mut arm.body, ids);
            }
        }
        _ => for child in crate::copyprop::children_mut(expression) { remap_expr(child, ids); },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn program(source: &str) -> CoreProgram {
        let (module, _) = crate::compile::parse_with_prelude(source).unwrap();
        let globals = crate::resolve::resolve_module(module).unwrap().globals;
        crate::inline::set_enabled(false);
        let program = crate::lower::lower_program(&globals);
        crate::inline::set_enabled(true);
        program.unwrap()
    }
    #[test]
    fn unused_bindings_and_assignments_keep_call_side_effects() {
        let mut p = program(r#"
            fn tick(mut counter: Array<i64>) -> i64 { counter[0] += 1; 99 }
            fn main() -> i64 {
                let counter: Array<i64> = array_new(1);
                let mut unused = tick(counter);
                unused = tick(counter);
                counter[0]
            }
        "#);
        let before: usize = p.funcs.iter().map(|f| f.locals.len()).sum();
        compact(&mut p);
        assert!(p.funcs.iter().map(|f| f.locals.len()).sum::<usize>() < before);
        assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), 2);
    }
    #[test]
    fn closure_captures_parameters_and_payload_bindings_remain_wired() {
        let mut p = program(r#"
            #[value] enum Item { Empty, Number(i64) }
            fn main() -> i64 {
                let unused = 17;
                let offset = 4;
                let f = |item: Item| match item { Item::Empty => offset, Item::Number(n) => n + offset };
                f(Item::Number(7))
            }
        "#);
        compact(&mut p);
        assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&p, true) }.unwrap(), 11);
    }
}
