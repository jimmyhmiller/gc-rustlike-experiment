//! Bounded, single-level expansion of small allocating recursive functions.
//! A distinct always-inline wrapper retains calls to the original function;
//! only the original's self calls target the wrapper. LLVM can expose the base
//! case and adjacent allocations without unrolling the recursive call graph
//! indefinitely. Source allocation labels remain attached to the original.
use std::collections::{HashMap, HashSet};
use crate::core::*;

pub(crate) struct Expanded {
    pub program: CoreProgram,
    pub wrappers: HashMap<FuncId, String>,
}

pub(crate) fn expand(program: &CoreProgram) -> Expanded {
    let mut result = Expanded { program: program.clone(), wrappers: HashMap::new() };
    let mut names: HashSet<_> = program.funcs.iter().map(|f| f.name.clone()).collect();
    for (id, function) in program.funcs.iter().enumerate() {
        if function.is_extern || !function.closure_captures.is_empty() { continue; }
        let mut body = function.body.clone();
        let mut cost = Cost::default();
        let wrapper_id = result.program.funcs.len() as FuncId;
        if !visit_block(&mut body, id as FuncId, wrapper_id, &mut cost)
            || cost.calls == 0 || cost.allocations == 0
            || cost.nodes > 96 || cost.nodes * (cost.calls + 1) > 256
        { continue; }
        let mut wrapper = function.clone();
        let mut suffix = 0;
        loop {
            wrapper.name = format!("__gcr_recursive_inline_{id}_{suffix}");
            if names.insert(wrapper.name.clone()) { break; }
            suffix += 1;
        }
        result.program.funcs[id].body = body;
        result.program.funcs.push(wrapper);
        result.wrappers.insert(wrapper_id, function.name.clone());
    }
    result
}

#[derive(Default)]
struct Cost { nodes: usize, calls: usize, allocations: usize }
fn visit_block(block: &mut CoreBlock, original: FuncId, wrapper: FuncId, cost: &mut Cost) -> bool {
    block.stmts.iter_mut().all(|s| {
        let (CoreStmt::Let(_, e) | CoreStmt::Expr(e)) = s;
        visit(e, original, wrapper, cost)
    }) && block.tail.as_mut().is_none_or(|e| visit(e, original, wrapper, cost))
}
fn visit(e: &mut CoreExpr, original: FuncId, wrapper: FuncId, cost: &mut Cost) -> bool {
    use CoreExprKind::*;
    cost.nodes += 1;
    if cost.nodes > 256 { return false; }
    match e.kind.as_mut() {
        ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..) | Local(..) | Unit => true,
        Bin(_, a, b) => visit(a, original, wrapper, cost) && visit(b, original, wrapper, cost),
        Un(_, e) | Cast { value: e, .. } | Assign { value: e, .. } => visit(e, original, wrapper, cost),
        Return(e) => e.as_mut().is_none_or(|e| visit(e, original, wrapper, cost)),
        Block(block) => visit_block(block, original, wrapper, cost),
        If(condition, yes, no) => visit(condition, original, wrapper, cost)
            && visit_block(yes, original, wrapper, cost) && visit_block(no, original, wrapper, cost),
        Call(callee, args) => {
            if *callee == original { *callee = wrapper; cost.calls += 1; }
            args.iter_mut().all(|e| visit(e, original, wrapper, cost))
        }
        New { fields, .. } | MakeVariant { fields, .. } => {
            cost.allocations += 1;
            fields.iter_mut().all(|e| visit(e, original, wrapper, cost))
        }
        // Keep expansion conservative for loops, closures and effects whose
        // code size is not bounded by the simple expression count above.
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
    fn one_level_expansion_preserves_recursive_results_and_labels() {
        let original = lower(r#"
            enum Tree { Leaf, Node(Tree, Tree) }
            fn make(n: i64) -> Tree {
                if n > 0 { Tree::Node(make(n-1), make(n-1)) } else { Tree::Leaf }
            }
            fn check(t: Tree) -> i64 {
                match t { Tree::Leaf => 1, Tree::Node(a,b) => 1+check(a)+check(b) }
            }
            fn main() -> i64 { check(make(5)) }
        "#);
        let expanded = expand(&original);
        assert_eq!(expanded.wrappers.len(), 1);
        assert_eq!(expanded.program.funcs.len(), original.funcs.len()+1);
        assert_eq!(crate::codegen::jit_run_i64_gc(&original, true).unwrap(), 63);
        let (wrapper, name) = expanded.wrappers.iter().next().unwrap();
        assert_eq!(name, "make");
        // The wrapper is a pristine copy, so its recursive calls still target
        // the original and cannot repeatedly expand the wrapper itself.
        let mut wrapper_body = expanded.program.funcs[*wrapper as usize].body.clone();
        let mut cost = Cost::default();
        assert!(visit_block(&mut wrapper_body, *wrapper, *wrapper, &mut cost));
        assert_eq!(cost.calls, 0);
    }
}
