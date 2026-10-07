//! Separate a cheap recursive base case from its allocating recursive branch.
//! The helper keeps ordinary managed-call and root protocols. Unsupported
//! effects and branches that capture non-parameter locals are left unchanged.
use crate::core::*;
use std::collections::{BTreeSet, HashMap, HashSet};
pub(crate) struct Outlined {
    pub program: CoreProgram,
    pub helpers: HashMap<FuncId, String>,
}
pub(crate) fn outline(program: &CoreProgram) -> Outlined {
    let mut result = Outlined {
        program: program.clone(),
        helpers: HashMap::new(),
    };
    let mut names: HashSet<_> = program.funcs.iter().map(|f| f.name.clone()).collect();
    for (id, function) in program.funcs.iter().enumerate() {
        if function.is_extern
            || !function.closure_captures.is_empty()
            || function.name.starts_with("__closure_")
        {
            continue;
        }
        let Some(tail) = &function.body.tail else {
            continue;
        };
        let CoreExprKind::If(_, yes, no) = tail.kind.as_ref() else {
            continue;
        };
        let mut a = Cost::default();
        let mut b = Cost::default();
        if !inspect_block(yes, id as FuncId, &mut a) || !inspect_block(no, id as FuncId, &mut b) {
            continue;
        }
        let heavy_yes = if a.recursive >= 2 && a.allocations > 0 && b.nodes <= 4 && b.recursive == 0
        {
            true
        } else if b.recursive >= 2 && b.allocations > 0 && a.nodes <= 4 && a.recursive == 0 {
            false
        } else {
            continue;
        };
        let cost = if heavy_yes { &a } else { &b };
        if cost.nodes > 96
            || cost.reads.iter().any(|local| {
                *local as usize >= function.params.len() && !cost.defined.contains(local)
            })
        {
            continue;
        }
        let helper_id = result.program.funcs.len() as FuncId;
        let mut helper = function.clone();
        helper.body = if heavy_yes { *yes.clone() } else { *no.clone() };
        let mut suffix = 0;
        loop {
            helper.name = format!("__gcr_recursive_branch_{id}_{suffix}");
            if names.insert(helper.name.clone()) {
                break;
            }
            suffix += 1;
        }
        let args = function
            .params
            .iter()
            .enumerate()
            .map(|(i, repr)| CoreExpr::new(CoreExprKind::Local(i as LocalId), repr.clone()))
            .collect();
        let call = CoreExpr {
            kind: Box::new(CoreExprKind::Call(helper_id, args)),
            repr: tail.repr.clone(),
            span: tail.span,
        };
        let CoreExprKind::If(_, yes, no) = result.program.funcs[id]
            .body
            .tail
            .as_mut()
            .unwrap()
            .kind
            .as_mut()
        else {
            unreachable!()
        };
        let branch = if heavy_yes { yes } else { no };
        **branch = CoreBlock {
            stmts: Vec::new(),
            tail: Some(call),
        };
        result.program.funcs.push(helper);
        result.helpers.insert(helper_id, function.name.clone());
    }
    result
}
#[derive(Default)]
struct Cost {
    nodes: usize,
    recursive: usize,
    allocations: usize,
    reads: BTreeSet<LocalId>,
    defined: BTreeSet<LocalId>,
}
fn inspect_block(block: &CoreBlock, original: FuncId, cost: &mut Cost) -> bool {
    block.stmts.iter().all(|s| {
        let e = match s {
            CoreStmt::Let(local, e) => {
                cost.defined.insert(*local);
                e
            }
            CoreStmt::Expr(e) => e,
        };
        inspect(e, original, cost)
    }) && block
        .tail
        .as_ref()
        .is_none_or(|e| inspect(e, original, cost))
}
fn inspect(e: &CoreExpr, original: FuncId, cost: &mut Cost) -> bool {
    use CoreExprKind::*;
    cost.nodes += 1;
    match e.kind.as_ref() {
        ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..) | Unit => {
            true
        }
        Block(block) => inspect_block(block, original, cost),
        Local(local) => {
            cost.reads.insert(*local);
            true
        }
        Bin(_, a, b) => inspect(a, original, cost) && inspect(b, original, cost),
        Un(_, e) | Cast { value: e, .. } => inspect(e, original, cost),
        // Assignment/control transfer require capture and successor analysis;
        // retain their original machine function rather than guessing.
        Call(callee, args) => {
            if *callee == original {
                cost.recursive += 1;
            }
            args.iter().all(|e| inspect(e, original, cost))
        }
        New { fields, .. } | MakeVariant { fields, .. } => {
            cost.allocations += 1;
            fields.iter().all(|e| inspect(e, original, cost))
        }
        _ => false,
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
    fn recursive_branch_preserves_results_and_allocation_labels() {
        let program = lower(
            r#"
            enum Tree { Leaf, Node(Tree, Tree) }
            fn make(n: i64) -> Tree {
                if n > 0 { Tree::Node(make(n-1), make(n-1)) } else { Tree::Leaf }
            }
            fn check(t: Tree) -> i64 {
                match t { Tree::Leaf => 1, Tree::Node(a,b) => 1+check(a)+check(b) }
            }
            fn main() -> i64 { check(make(5)) }
        "#,
        );
        let expanded = outline(&program);
        assert_eq!(expanded.helpers.len(), 1);
        assert_eq!(crate::codegen::jit_run_i64_gc(&program, true).unwrap(), 63);
        let context = inkwell::context::Context::create();
        let compiled = crate::codegen::codegen(&context, &program).unwrap();
        assert!(
            compiled
                .alloc_sites
                .iter()
                .all(|site| !site.function.starts_with("__gcr_recursive_branch"))
        );
    }
    #[test]
    fn branch_captures_and_assignments_are_conservatively_retained() {
        let program = lower(
            r#"
            enum Tree { Leaf, Node(Tree, Tree) }
            fn make(n: i64) -> Tree {
                let next = n-1;
                if n > 0 { Tree::Node(make(next), make(next)) } else { Tree::Leaf }
            }
            fn main() -> i64 { let t = make(2); 0 }
        "#,
        );
        assert!(outline(&program).helpers.is_empty());
    }
}
