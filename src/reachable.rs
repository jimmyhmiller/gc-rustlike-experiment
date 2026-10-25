//! Executable reachability after Core transformations have removed call sites.
//! Function addresses in closures and native callbacks are graph edges too.
use crate::core::*;

pub(crate) fn from_entry(program: &CoreProgram) -> CoreProgram {
    let Some(entry) = program.entry else { return program.clone(); };
    let live = live_functions(program);
    let mut ids = vec![None; live.len()];
    let mut funcs = Vec::new();
    for (id, function) in program.funcs.iter().enumerate() {
        if live[id] {
            ids[id] = Some(funcs.len() as FuncId);
            funcs.push(function.clone());
        }
    }
    let mut remap = |e: &mut CoreExpr| match e.kind.as_mut() {
        CoreExprKind::Call(id, _) | CoreExprKind::CallbackPtr(id)
        | CoreExprKind::MakeClosure { code: id, .. } => *id = ids[*id as usize].unwrap(),
        _ => {},
    };
    for function in &mut funcs {
        for statement in &mut function.body.stmts {
            let (CoreStmt::Let(_, e) | CoreStmt::Expr(e)) = statement;
            crate::inline::visit_expr_mut(e, &mut remap);
        }
        if let Some(e) = &mut function.body.tail {
            crate::inline::visit_expr_mut(e, &mut remap);
        }
    }
    CoreProgram {
        funcs, entry: Some(ids[entry as usize].unwrap()),
        layouts: program.layouts.clone(), values: program.values.clone(),
        spans: program.spans.clone(), sources: program.sources.clone(),
    }
}

/// Reachability without cloning or renumbering the program.
pub(crate) fn live_functions(program: &CoreProgram) -> Vec<bool> {
    let Some(entry) = program.entry else { return vec![true; program.funcs.len()]; };
    let mut live = vec![false; program.funcs.len()];
    let mut pending = vec![entry];
    while let Some(id) = pending.pop() {
        if std::mem::replace(&mut live[id as usize], true) { continue; }
        crate::inline::visit_block(&program.funcs[id as usize].body, &mut |e| {
            match e.kind.as_ref() {
                CoreExprKind::Call(id, _) | CoreExprKind::CallbackPtr(id)
                | CoreExprKind::MakeClosure { code: id, .. } => pending.push(*id),
                _ => {},
            }
        });
    }
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_address_taken_targets_and_remaps_recursive_edges() {
        let (module, _) = crate::compile::parse_with_prelude(
            "fn recur(n: i64) -> i64 { if n == 0 { 7 } else { recur(n-1) } } fn main() -> i64 { recur(3) }"
        ).unwrap();
        crate::inline::set_enabled(false);
        let mut original = crate::lower::lower_program(&crate::resolve::resolve_module(module).unwrap().globals).unwrap();
        crate::inline::set_enabled(true);
        let entry = original.entry.unwrap();
        let recur = original.funcs.iter().position(|f| f.name == "recur").unwrap() as FuncId;
        let mut dead = original.funcs[recur as usize].clone();
        dead.name = "dead".into();
        let mut callback = dead.clone();
        callback.name = "callback".into();
        callback.body = CoreBlock { stmts: vec![], tail: Some(CoreExpr::new(CoreExprKind::ConstInt(9, ScalarRepr::I64), Repr::Scalar(ScalarRepr::I64))) };
        original.funcs.push(dead);
        let target = original.funcs.len() as FuncId;
        original.funcs.push(callback);
        original.funcs[entry as usize].body.stmts.push(CoreStmt::Expr(CoreExpr::new(CoreExprKind::CallbackPtr(target), Repr::Scalar(ScalarRepr::Ptr))));
        let result = from_entry(&original);
        assert_eq!(result.funcs.len(), original.funcs.len() - 1);
        assert!(!result.funcs.iter().any(|f| f.name == "dead"));
        let target = result.funcs.iter().position(|f| f.name == "callback").unwrap() as FuncId;
        assert!(result.funcs[result.entry.unwrap() as usize].body.stmts.iter().any(|s| {
            matches!(s, CoreStmt::Expr(e) if matches!(e.kind.as_ref(), CoreExprKind::CallbackPtr(id) if *id == target))
        }));
        assert_eq!(unsafe { crate::codegen::jit_run_i64(&result) }.unwrap(), 7);
    }
}
