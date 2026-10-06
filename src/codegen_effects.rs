//! Conservative call effects for the generated root-cache protocol.
//!
//! Only expressions lowered without collecting or parking runtime calls are
//! admitted. Loops always poll. Unrecognized operations remain effectful, so
//! extending the IR cannot accidentally omit a required relocation check.
use crate::core::{CoreBlock, CoreExpr, CoreExprKind, CoreProgram, CoreStmt, FuncId};

pub(crate) fn nonrelocating_functions(program: &CoreProgram) -> Vec<bool> {
    let mut calls = vec![Vec::new(); program.funcs.len()];
    let mut safe: Vec<bool> = program
        .funcs
        .iter()
        .enumerate()
        .map(|(id, function)| {
            !function.is_extern && block_is_nonrelocating(&function.body, &mut calls[id])
        })
        .collect();
    // Remove every function that reaches an effectful callee. Pure recursive
    // components remain safe: none of their steps can collect or park.
    loop {
        let mut changed = false;
        for id in 0..safe.len() {
            if safe[id]
                && calls[id]
                    .iter()
                    .any(|&callee| !safe.get(callee as usize).copied().unwrap_or(false))
            {
                safe[id] = false;
                changed = true;
            }
        }
        if !changed {
            return safe;
        }
    }
}

fn block_is_nonrelocating(block: &CoreBlock, calls: &mut Vec<FuncId>) -> bool {
    block.stmts.iter().all(|statement| {
        let (CoreStmt::Let(_, expression) | CoreStmt::Expr(expression)) = statement;
        expression_is_nonrelocating(expression, calls)
    }) && block
        .tail
        .as_ref()
        .is_none_or(|tail| expression_is_nonrelocating(tail, calls))
}

fn expression_is_nonrelocating(expression: &CoreExpr, calls: &mut Vec<FuncId>) -> bool {
    use CoreExprKind::*;
    match expression.kind.as_ref() {
        ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..) | Unit
        | Local(..) => true,
        Bin(_, a, b) => {
            expression_is_nonrelocating(a, calls) && expression_is_nonrelocating(b, calls)
        }
        Un(_, value)
        | FloatIntrinsic(_, value)
        | FloatBits(value)
        | Cast { value, .. }
        | Assign { value, .. } => expression_is_nonrelocating(value, calls),
        Return(value) => value
            .as_ref()
            .is_none_or(|value| expression_is_nonrelocating(value, calls)),
        Block(block) => block_is_nonrelocating(block, calls),
        If(condition, yes, no) => {
            expression_is_nonrelocating(condition, calls)
                && block_is_nonrelocating(yes, calls)
                && block_is_nonrelocating(no, calls)
        }
        Call(callee, arguments) => {
            calls.push(*callee);
            arguments
                .iter()
                .all(|argument| expression_is_nonrelocating(argument, calls))
        }
        // Reference enums are immutable after construction. Their tag and
        // payload extraction (including nested value payloads) use loads and
        // stack temporaries, with no managed snapshot lock or allocation.
        Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
            expression_is_nonrelocating(scrutinee, calls)
                && arms
                    .iter()
                    .all(|arm| expression_is_nonrelocating(&arm.body, calls))
        }
        EnumTag(scrutinee) | EnumPayload { scrutinee, .. } => {
            expression_is_nonrelocating(scrutinee, calls)
        }
        // In particular, Loop polls even when its arithmetic is pure. Foreign,
        // indirect and runtime calls, allocation, I/O and aggregate heap reads
        // are all conservative. Snapshot locks may park as well as allocate.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_calls_and_pure_recursive_components_are_safe() {
        let source = "
            fn scalar(i: i64, j: i64) -> i64 { (i+j)*(i+j+1)/2+i+1 }
            fn forward(i: i64) -> i64 { scalar(i, 7) }
            fn even(i: i64) -> i64 { if i == 0 { 1 } else { odd(i-1) } }
            fn odd(i: i64) -> i64 { if i == 0 { 0 } else { even(i-1) } }
            fn main() -> i64 { forward(3) + even(4) }
        ";
        let (program, safe) = analyze(source);
        for name in ["scalar", "forward", "even", "odd", "main"] {
            assert!(effect(&program, &safe, name), "{name}");
        }
    }

    #[test]
    fn polling_allocating_foreign_and_transitive_calls_are_effectful() {
        let source = "
            extern \"C\" fn unknown() -> i64;
            fn foreign() -> i64 { unknown() }
            fn allocate() -> i64 { let a: Array<i64> = array_new(1); array_len(a) }
            fn polling() -> i64 { let mut i = 0; while i < 4 { i = i+1; } i }
            fn transit() -> i64 { polling() }
            fn parent() -> i64 { transit() }
            fn main() -> i64 { parent() + allocate() + foreign() }
        ";
        let (program, safe) = analyze(source);
        for name in [
            "unknown", "foreign", "allocate", "polling", "transit", "parent", "main",
        ] {
            assert!(!effect(&program, &safe, name), "{name}");
        }
    }

    #[test]
    fn callers_keep_checks_only_for_potential_relocation() {
        use inkwell::values::AnyValue;
        let source = "
            fn scalar(i: i64) -> i64 { (i+1)*(i+2) }
            fn polling() -> i64 { let mut i = 0; while i < 4 { i = i+1; } i }
            fn safe_user(a: Array<i64>, i: i64) -> i64 { let b = a; scalar(i) }
            fn unsafe_user(a: Array<i64>) -> i64 { let b = a; polling() }
            fn main() -> i64 { let a: Array<i64> = array_new(1); safe_user(a, 3)+unsafe_user(a) }
        ";
        let (program, _) = analyze(source);
        let context = inkwell::context::Context::create();
        let compiled = crate::codegen::codegen(&context, &program).unwrap();
        for (name, must_check) in [("safe_user", false), ("unsafe_user", true)] {
            let ir = compiled
                .module
                .get_function(name)
                .unwrap()
                .print_to_string()
                .to_string();
            assert_eq!(ir.contains("gcframe"), must_check, "{ir}");
            assert_eq!(ir.contains("gc.epoch"), must_check, "{ir}");
            assert_eq!(ir.contains("roots.reload"), must_check, "{ir}");
        }
    }

    #[test]
    fn immutable_enum_reads_are_safe_but_allocating_arms_are_not() {
        let source = "
            enum Tree { Leaf, Node(Tree, Tree) }
            fn check(t: Tree) -> i64 { match t { Tree::Leaf => 1, Tree::Node(l,r) => 1+check(l)+check(r) } }
            fn allocating_arm(t: Tree) -> i64 { match t { Tree::Leaf => { let a: Array<i64> = array_new(1); array_len(a) }, Tree::Node(l,r) => check(l)+check(r) } }
            fn main() -> i64 { allocating_arm(Tree::Leaf) }
        ";
        let (program, safe) = analyze(source);
        assert!(effect(&program, &safe, "check"));
        assert!(!effect(&program, &safe, "allocating_arm"));
    }

    #[test]
    fn deferred_frames_protect_recursive_children_and_retained_caller_roots() {
        use inkwell::values::AnyValue;
        let source = "
            enum Tree { Leaf, Node(Tree, Tree) }
            fn make(n: i64) -> Tree { if n > 0 { Tree::Node(make(n-1),make(n-1)) } else { Tree::Leaf } }
            fn check(t: Tree) -> i64 { match t { Tree::Leaf => 1, Tree::Node(l,r) => 1+check(l)+check(r) } }
            fn with_param(t: Tree) -> i64 { let fresh = make(7); check(t)+check(fresh) }
            fn main() -> i64 { let t = make(8); with_param(t) }
        ";
        let (program, _) = analyze(source);
        let context = inkwell::context::Context::create();
        let compiled = crate::codegen::codegen(&context, &program).unwrap();
        for (name, deferred) in [("make", true), ("with_param", false)] {
            let ir = compiled
                .module
                .get_function(name)
                .unwrap()
                .print_to_string()
                .to_string();
            assert_eq!(ir.contains("roots.active"), deferred, "{ir}");
        }
        assert_eq!(crate::codegen::jit_run_i64_gc(&program, true).unwrap(), 766);
    }

    fn analyze(source: &str) -> (CoreProgram, Vec<bool>) {
        let (module, _) = crate::compile::parse_with_prelude(source).unwrap();
        let resolved = crate::resolve::resolve_module(module).unwrap();
        let program = crate::lower::lower_program(&resolved.globals).unwrap();
        let safe = nonrelocating_functions(&program);
        (program, safe)
    }

    fn effect(program: &CoreProgram, safe: &[bool], name: &str) -> bool {
        let id = program
            .funcs
            .iter()
            .position(|function| function.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "missing {name}: {:?}",
                    program.funcs.iter().map(|f| &f.name).collect::<Vec<_>>()
                )
            });
        safe[id]
    }
}
