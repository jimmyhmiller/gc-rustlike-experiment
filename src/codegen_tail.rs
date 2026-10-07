//! Eliminate direct self tail calls before safepoint and root analysis. Letting
//! LLVM introduce an unpolled recursive backedge after those analyses can make
//! an allocation-free mutator invisible to a pending collection.
use crate::core::*;

pub(crate) fn eliminate(program: &mut CoreProgram) {
    for (id, function) in program.funcs.iter_mut().enumerate() {
        if function.is_extern
            || !function.closure_captures.is_empty()
            || function.name.starts_with("__closure_")
        {
            continue;
        }
        let mut body = function.body.clone();
        let mut pass = Tail {
            function: id as FuncId,
            params: &function.params,
            ret: &function.ret,
            locals: function.locals.clone(),
            names: function.local_names.clone(),
            changed: false,
        };
        pass.names.resize(pass.locals.len(), None);
        pass.block(&mut body, true, 0);
        if pass.changed {
            function.locals = pass.locals;
            function.local_names = pass.names;
            function.body = CoreBlock {
                stmts: Vec::new(),
                tail: Some(CoreExpr {
                    kind: Box::new(CoreExprKind::Loop(Box::new(body))),
                    repr: Repr::Unit,
                    span: function.span,
                }),
            };
        }
    }
}
struct Tail<'a> {
    function: FuncId,
    params: &'a [Repr],
    ret: &'a Repr,
    locals: Vec<Repr>,
    names: Vec<Option<String>>,
    changed: bool,
}
impl Tail<'_> {
    fn block(&mut self, block: &mut CoreBlock, tail: bool, depth: usize) {
        // ANF may bind a returned call immediately before its return. Fold
        // that single-use adjacent result back into the tail position.
        let mut index = 1;
        while index < block.stmts.len() {
            let returned = match &block.stmts[index] {
                CoreStmt::Expr(e) => match e.kind.as_ref() {
                    CoreExprKind::Return(Some(value)) => match value.kind.as_ref() {
                        CoreExprKind::Local(local) => Some(*local),
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            };
            let call = match &block.stmts[index - 1] {
                CoreStmt::Let(local, value)
                    if Some(*local) == returned
                        && matches!(value.kind.as_ref(), CoreExprKind::Call(id, _) if *id == self.function) =>
                {
                    Some(value.clone())
                }
                _ => None,
            };
            if let Some(call) = call {
                if depth == 0 {
                    let CoreStmt::Expr(e) = &mut block.stmts[index] else {
                        unreachable!()
                    };
                    e.kind = Box::new(CoreExprKind::Return(Some(Box::new(call))));
                    block.stmts.remove(index - 1);
                    continue;
                }
            }
            index += 1;
        }
        if tail && depth == 0 {
            let returned = block.tail.as_ref().and_then(|e| match e.kind.as_ref() {
                CoreExprKind::Local(local) => Some(*local),
                _ => None,
            });
            let call = match block.stmts.last() {
                Some(CoreStmt::Let(local, value))
                    if Some(*local) == returned
                        && matches!(value.kind.as_ref(), CoreExprKind::Call(id, _) if *id == self.function) =>
                {
                    Some(value.clone())
                }
                _ => None,
            };
            if let Some(call) = call {
                block.stmts.pop();
                block.tail = Some(call);
            }
        }
        for statement in &mut block.stmts {
            let (CoreStmt::Let(_, e) | CoreStmt::Expr(e)) = statement;
            self.expr(e, false, depth);
        }
        if let Some(e) = &mut block.tail {
            self.expr(e, tail, depth);
        } else if tail && matches!(self.ret, Repr::Unit) {
            block.tail = Some(CoreExpr::new(CoreExprKind::Return(None), Repr::Unit));
        }
    }
    fn expr(&mut self, e: &mut CoreExpr, tail: bool, depth: usize) {
        use CoreExprKind::*;
        if depth == 0 {
            if let Return(Some(value)) = e.kind.as_mut() {
                let mut value = value.as_ref().clone();
                self.expr(&mut value, true, depth);
                *e = value;
                return;
            }
            if tail {
                if let Call(function, args) = e.kind.as_ref() {
                    if *function == self.function && args.len() == self.params.len() {
                        // Evaluate all arguments before assigning any parameter:
                        // swaps, aliases, allocations and value aggregates keep
                        // their source evaluation order and old input values.
                        let mut statements = Vec::new();
                        let mut temps = Vec::new();
                        for (argument, repr) in args.iter().zip(self.params) {
                            let local = self.locals.len() as LocalId;
                            self.locals.push(repr.clone());
                            self.names.push(None);
                            statements.push(CoreStmt::Let(local, argument.clone()));
                            temps.push(local);
                        }
                        for (parameter, (local, repr)) in
                            temps.into_iter().zip(self.params).enumerate()
                        {
                            statements.push(CoreStmt::Expr(CoreExpr::new(
                                Assign {
                                    local: parameter as LocalId,
                                    value: Box::new(CoreExpr::new(Local(local), repr.clone())),
                                },
                                Repr::Unit,
                            )));
                        }
                        *e = CoreExpr {
                            kind: Box::new(Block(Box::new(CoreBlock {
                                stmts: statements,
                                tail: Some(CoreExpr::new(Continue, Repr::Unit)),
                            }))),
                            repr: e.repr.clone(),
                            span: e.span,
                        };
                        self.changed = true;
                        return;
                    }
                }
            }
        }
        match e.kind.as_mut() {
            If(condition, yes, no) => {
                self.expr(condition, false, depth);
                self.block(yes, tail, depth);
                self.block(no, tail, depth);
            }
            Block(block) => self.block(block, tail, depth),
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                self.expr(scrutinee, false, depth);
                for arm in arms {
                    self.expr(&mut arm.body, tail, depth);
                }
            }
            Loop(block) => self.block(block, false, depth + 1),
            Return(..) | Continue => {}
            _ if tail => {
                // Divergent unit loops in a non-unit function cannot return.
                if matches!(e.repr, Repr::Unit) && !matches!(self.ret, Repr::Unit) {
                    return;
                }
                let value = e.clone();
                *e = CoreExpr {
                    kind: Box::new(Return(Some(Box::new(value)))),
                    repr: e.repr.clone(),
                    span: e.span,
                };
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lower(source: &str) -> CoreProgram {
        let (module, _) = crate::compile::parse_with_prelude(source).unwrap();
        let module = crate::resolve::resolve_module(module).unwrap();
        crate::lower::lower_program(&module.globals).unwrap()
    }
    #[test]
    fn parallel_arguments_explicit_returns_and_reference_results_survive() {
        let source = r#"
            fn rotate(a: i64, b: i64, n: i64) -> i64 {
                if n == 0 { a*10+b } else { return rotate(b,a,n-1); }
            }
            fn keep(a: Array<i64>, n: i64) -> Array<i64> {
                if n == 0 { a } else { keep(a,n-1) }
            }
            fn main() -> i64 {
                let mut a: Array<i64> = array_new(1);
                array_set(a,0,42);
                rotate(2,3,100001)+keep(a,100001)[0]
            }
        "#;
        let program = lower(source);
        let mut transformed = program.clone();
        eliminate(&mut transformed);
        for name in ["rotate", "keep"] {
            let function = transformed.funcs.iter().find(|f| f.name == name).unwrap();
            assert!(matches!(
                function.body.tail.as_ref().unwrap().kind.as_ref(),
                CoreExprKind::Loop(_)
            ));
        }
        assert_eq!(crate::codegen::jit_run_i64_gc(&program, true).unwrap(), 74);
    }
}
