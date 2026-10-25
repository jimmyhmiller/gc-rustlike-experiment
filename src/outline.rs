//! Extract large single-exit branch regions into ordinary managed functions.
//! This bounds optimizer CFGs without moving work past program entry. All
//! extracted functions receive the same O3 and native optimization pipeline.
use crate::core::*;
use std::collections::BTreeSet;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Exit { Normal, Return, Leave(u32) }
const MIN_REGION: usize = 16;

pub(crate) fn branches(program: &mut CoreProgram) {
    if !crate::inline::enabled() { return; }
    let base = program.funcs.len();
    let foreign: Vec<_> = program.funcs.iter().map(|f| f.is_extern).collect();
    let mut helpers = Vec::new();
    for function in &mut program.funcs {
        if function.is_extern { continue; }
        let mut size = 0;
        let mut reads = vec![0usize; function.locals.len()];
        crate::inline::visit_block(&function.body, &mut |e| {
            size += 1;
            if let CoreExprKind::Local(local) = e.kind.as_ref() { reads[*local as usize] += 1; }
        });
        if size < 400 { continue; }
        let mut body = std::mem::replace(&mut function.body, CoreBlock { stmts: vec![], tail: None });
        walk_block(&mut body, function, &foreign, &reads, base, &mut helpers);
        function.body = body;
    }
    let mut names: std::collections::HashSet<_> = program.funcs.iter().map(|f| f.name.clone()).collect();
    for helper in &mut helpers {
        let original = helper.name.clone();
        let mut suffix = 0;
        while !names.insert(helper.name.clone()) {
            suffix += 1;
            helper.name = format!("{original}_{suffix}");
        }
    }
    program.funcs.extend(helpers);
}
fn walk_block(block: &mut CoreBlock, function: &CoreFn, foreign: &[bool], total_reads: &[usize], base: usize, helpers: &mut Vec<CoreFn>) {
    for statement in &mut block.stmts {
        let (CoreStmt::Let(_, value) | CoreStmt::Expr(value)) = statement;
        walk(value, function, foreign, total_reads, base, helpers);
    }
    if let Some(tail) = &mut block.tail { walk(tail, function, foreign, total_reads, base, helpers); }
}
fn branch(value: &mut CoreExpr, function: &CoreFn, foreign: &[bool], total_reads: &[usize], base: usize, helpers: &mut Vec<CoreFn>) {
    if !extract(value, function, foreign, total_reads, base, helpers) { walk(value, function, foreign, total_reads, base, helpers); }
}
fn walk(value: &mut CoreExpr, function: &CoreFn, foreign: &[bool], total_reads: &[usize], base: usize, helpers: &mut Vec<CoreFn>) {
    use CoreExprKind::*;
    match value.kind.as_mut() {
        If(condition, yes, no) => {
            walk(condition, function, foreign, total_reads, base, helpers);
            for block in [yes, no] {
                let mut region = CoreExpr::new(Block(block.clone()), value.repr.clone()).at(value.span);
                if extract(&mut region, function, foreign, total_reads, base, helpers) {
                    **block = CoreBlock { stmts: vec![], tail: Some(region) };
                } else { walk_block(block, function, foreign, total_reads, base, helpers); }
            }
        }
        Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
            walk(scrutinee, function, foreign, total_reads, base, helpers);
            for arm in arms { branch(&mut arm.body, function, foreign, total_reads, base, helpers); }
        }
        Block(block) | Loop(block) | Once { body: block, .. } => walk_block(block, function, foreign, total_reads, base, helpers),
        _ => for child in crate::copyprop::children_mut(value) { walk(child, function, foreign, total_reads, base, helpers); },
    }
}
fn terminal(value: &CoreExpr) -> Option<(Exit, Repr)> {
    use CoreExprKind::*;
    match value.kind.as_ref() {
        Return(value) => Some((Exit::Return, value.as_ref().map_or(Repr::Unit, |v| v.repr.clone()))),
        Leave { label, value } => Some((Exit::Leave(*label), value.as_ref().map_or(Repr::Unit, |v| v.repr.clone()))),
        Block(block) => block.tail.as_ref().and_then(terminal).or_else(|| match block.stmts.last()? {
            CoreStmt::Expr(e) => terminal(e), _ => None,
        }),
        If(_, yes, no) => {
            let a = terminal_block(yes)?;
            (terminal_block(no)? == a).then_some(a)
        }
        Match { arms, .. } | ValueMatch { arms, .. } => {
            let a = terminal(&arms.first()?.body)?;
            arms.iter().all(|arm| terminal(&arm.body).as_ref() == Some(&a)).then_some(a)
        }
        _ => None,
    }
}
fn terminal_block(block: &CoreBlock) -> Option<(Exit, Repr)> {
    if let Some(tail) = &block.tail { terminal(tail) }
    else { match block.stmts.last()? { CoreStmt::Expr(e) => terminal(e), _ => None } }
}
fn definitions(value: &CoreExpr, locals: &mut BTreeSet<LocalId>) {
    use CoreExprKind::*;
    fn block(b: &CoreBlock, locals: &mut BTreeSet<LocalId>) {
        for s in &b.stmts {
            let e = match s { CoreStmt::Let(l,e) => { locals.insert(*l); e }, CoreStmt::Expr(e) => e };
            definitions(e, locals);
        }
        if let Some(t) = &b.tail { definitions(t, locals); }
    }
    match value.kind.as_ref() {
        If(c, a, b) => { definitions(c, locals); block(a, locals); block(b, locals); }
        Block(b) | Loop(b) | Once { body: b, .. } => block(b, locals),
        Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
            definitions(scrutinee, locals);
            for a in arms { locals.extend(&a.binds); definitions(&a.body, locals); }
        }
        _ => for c in crate::copyprop::children(value) { definitions(c, locals); },
    }
}
fn extract(region: &mut CoreExpr, function: &CoreFn, foreign: &[bool], total_reads: &[usize], base: usize, helpers: &mut Vec<CoreFn>) -> bool {
    use CoreExprKind::*;
    let Some((exit, ret)) = terminal(region) else { return false };
    let mut size = 0;
    let mut foreign_call = false;
    let mut labels = BTreeSet::new();
    let mut defined = BTreeSet::new();
    let mut reads = BTreeSet::new();
    let mut counts = std::collections::HashMap::new();
    let mut assigned = BTreeSet::new();
    definitions(region, &mut defined);
    crate::inline::visit_expr(region, &mut |e| {
        size += 1;
        match e.kind.as_ref() {
            Call(id, _) if foreign.get(*id as usize).copied().unwrap_or(false) => { foreign_call = true; }
            Once { label, .. } => { labels.insert(*label); }
            Local(l) => { reads.insert(*l); *counts.entry(*l).or_insert(0usize) += 1; }
            Assign { local, .. } => { assigned.insert(*local); }
            _ => {}
        }
    });
    if foreign_call || size < if exit == Exit::Normal { 64 } else { MIN_REGION } || matches!(exit, Exit::Leave(l) if labels.contains(&l))
        || assigned.iter().any(|l| !defined.contains(l))
        // Optimizations can make Core locals non-lexical. A definition used
        // outside the region must remain in the caller's storage.
        || defined.iter().any(|l| total_reads[*l as usize] > counts.get(l).copied().unwrap_or(0)) { return false; }
    // A region may contain its own loops/Once blocks, but cannot transfer to
    // an enclosing loop or to a second continuation. Native buffer lifetimes
    // remain in their original lexical scope by excluding foreign buffers.
    fn valid(e: &CoreExpr, exit: Exit, labels: &BTreeSet<u32>, loops: u32) -> bool {
        use CoreExprKind::*;
        match e.kind.as_ref() {
            Return(_) if exit != Exit::Return => return false,
            Leave { label, .. } if !labels.contains(label) && exit != Exit::Leave(*label) => return false,
            Break(_) | Continue if loops == 0 => return false,
            AsCBytes { .. } => return false,
            Loop(b) => return valid_block(b, exit, labels, loops + 1),
            Block(b) | Once { body: b, .. } => return valid_block(b, exit, labels, loops),
            If(c,a,b) => return valid(c,exit,labels,loops) && valid_block(a,exit,labels,loops) && valid_block(b,exit,labels,loops),
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => return valid(scrutinee,exit,labels,loops) && arms.iter().all(|a|valid(&a.body,exit,labels,loops)),
            _ => {}
        }
        crate::copyprop::children(e).into_iter().all(|c| valid(c,exit,labels,loops))
    }
    fn valid_block(b: &CoreBlock, exit: Exit, labels: &BTreeSet<u32>, loops: u32) -> bool {
        b.stmts.iter().all(|s| valid(match s { CoreStmt::Let(_,e)|CoreStmt::Expr(e)=>e },exit,labels,loops))
            && b.tail.as_ref().is_none_or(|e|valid(e,exit,labels,loops))
    }
    if !valid(region, exit, &labels, 0) { return false; }
    let captures: Vec<_> = reads.difference(&defined).copied().collect();
    let mut ids = vec![None; function.locals.len()];
    let mut locals = Vec::new();
    let mut names = Vec::new();
    for l in captures.iter().chain(defined.iter()) {
        ids[*l as usize] = Some(locals.len() as LocalId);
        locals.push(function.locals[*l as usize].clone());
        names.push(function.local_names.get(*l as usize).cloned().unwrap_or(None));
    }
    let mut body = region.clone();
    crate::inline::visit_expr_mut(&mut body, &mut |e| {
        if matches!(e.kind.as_ref(), Leave { label, .. } if exit == Exit::Leave(*label)) {
            let Leave { value, .. } = std::mem::replace(e.kind.as_mut(), Unit) else { unreachable!() };
            *e.kind = Return(value);
        }
    });
    crate::locals::remap_expr(&mut body, &ids);
    let id = (base + helpers.len()) as FuncId;
    let call = CoreExpr::new(Call(id, captures.iter().map(|&l| CoreExpr::new(Local(l),function.locals[l as usize].clone()).at(region.span)).collect()), ret.clone()).at(region.span);
    let result = if ret == Repr::Unit { CoreBlock { stmts: vec![CoreStmt::Expr(call)], tail: None } }
        else { CoreBlock { stmts: vec![], tail: Some(call) } };
    if exit == Exit::Normal {
        *region = if let Some(value) = result.tail { value } else {
            CoreExpr::new(Block(Box::new(result)), Repr::Unit).at(region.span)
        };
    } else {
        let output = result.tail.map(Box::new);
        let transfer = match exit { Exit::Normal => unreachable!(), Exit::Return => Return(output), Exit::Leave(label) => Leave { label, value: output } };
        let replacement = CoreExpr::new(transfer, region.repr.clone()).at(region.span);
        *region = if result.stmts.is_empty() { replacement }
            else { CoreExpr::new(Block(Box::new(CoreBlock { stmts: result.stmts, tail: Some(replacement) })), region.repr.clone()).at(region.span) };
    }
    helpers.push(CoreFn {
        name: format!("{}.__outlined_{}", function.name, id),
        params: captures.iter().map(|&l|function.locals[l as usize].clone()).collect(), extern_by_ref: vec![], ret,
        locals, local_names: names, body: if exit == Exit::Normal { CoreBlock { stmts: vec![], tail: Some(body) } } else { CoreBlock { stmts: vec![CoreStmt::Expr(body)], tail: None } },
        closure_captures: vec![], is_closure: false, is_extern: false,
        hints: crate::ast::FnHints { inline: crate::ast::InlineHint::Never, ..Default::default() }, span: region.span,
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn program(source: &str) -> CoreProgram {
        let (module, _) = crate::compile::parse_with_prelude(source).unwrap();
        let globals = crate::resolve::resolve_module(module).unwrap().globals;
        crate::inline::set_enabled(false);
        let result = crate::lower::lower_program(&globals);
        crate::inline::set_enabled(true);
        result.unwrap()
    }
    #[test]
    fn outlined_returns_capture_values_and_preserve_internal_loop_exits() {
        let work = "a = a + 1;".repeat(80);
        let source = format!(r#"
            #[inline(never)] fn choose(n: i64) -> i64 {{
                if n > 0 {{ let mut a = n; {work}
                    loop {{ if a >= 100 {{ break; }} a = a + 1; }}
                    return a;
                }} else {{ let mut a = 0 - n; {work} return a; }}
            }}
            fn main() -> i64 {{ choose(1) + choose(-2) }}
        "#);
        let mut p = program(&source);
        assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), 182);
        let count = p.funcs.len();
        branches(&mut p);
        assert!(p.funcs.len() >= count + 2);
        assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), 182);
    }
    #[test]
    fn identical_private_helpers_merge_without_changing_results() {
        let work = "a = a + 1;".repeat(80);
        let mut p = program(&format!(r#"
            fn choose(n: i64) -> i64 {{
                if n > 0 {{ let mut a = n; {work} return a; }}
                else {{ let mut a = n; {work} return a; }}
            }}
            fn main() -> i64 {{ choose(1) + choose(-2) }}
        "#));
        let first = p.funcs.len();
        branches(&mut p);
        crate::locals::compact(&mut p);
        assert_eq!(p.funcs.len(), first + 2);
        merge_helpers(&mut p, first);
        let live = crate::reachable::live_functions(&p);
        assert_eq!(live[first..].iter().filter(|&&live| live).count(), 1);
        assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), 159);
    }

    #[test]
    fn branches_that_mutate_enclosing_locals_stay_in_their_function() {
        let work = "a = a + 1;".repeat(90);
        let mut p = program(&format!(r#"
            fn main() -> i64 {{ let mut a = 0; let n = 1;
                if n > 0 {{ {work} return a; }} else {{ {work} return a + 1; }}
            }}
        "#));
        let count = p.funcs.len();
        branches(&mut p);
        assert_eq!(p.funcs.len(), count);
        assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), 90);
    }
}

/// Only compiler-created direct-call helpers are eligible: merging source
/// functions could change observable callback pointer identity. Allocation-site
/// labels are also retained by excluding bodies with profiled allocations.
pub(crate) fn merge_helpers(program: &mut CoreProgram, first: usize) {
    let mut aliases: Vec<_> = (0..program.funcs.len() as FuncId).collect();
    let mut bodies = std::collections::HashMap::new();
    for id in first..program.funcs.len() {
        let function = &mut program.funcs[id];
        visit_block_mut(&mut function.body, &mut |e| {
            if let CoreExprKind::Call(target, _) = e.kind.as_mut() { *target = aliases[*target as usize]; }
        });
        let mut body = function.body.clone();
        let mut allocated = false;
        let mut floats = Vec::new();
        // Empty block wrappers and local Once label numbers carry no code
        // identity. Buffers were excluded during extraction, so no native
        // cleanup scope is removed by this comparison-only normalization.
        let mut labels = std::collections::HashMap::new();
        visit_block_mut(&mut body, &mut |e| {
            loop {
                let CoreExprKind::Block(block) = e.kind.as_ref() else { break };
                if !block.stmts.is_empty() || block.tail.as_ref().is_none_or(|tail| tail.repr != e.repr) { break; }
                let CoreExprKind::Block(mut block) = std::mem::replace(e.kind.as_mut(), CoreExprKind::Unit) else { unreachable!() };
                *e = block.tail.take().unwrap();
            }
            if let CoreExprKind::Once { label, .. } = e.kind.as_ref() {
                let next = labels.len() as u32;
                labels.insert(*label, next);
            }
        });
        visit_block_mut(&mut body, &mut |e| {
            e.span = NO_SPAN;
            match e.kind.as_mut() {
                CoreExprKind::Once { label, .. } | CoreExprKind::Leave { label, .. } => *label = labels[label],
                _ => {}
            }
            match e.kind.as_ref() {
                CoreExprKind::New { .. } | CoreExprKind::MakeVariant { .. }
                | CoreExprKind::ArrayNew { .. } | CoreExprKind::MakeClosure { .. }
                | CoreExprKind::RuntimeCall { func: "ai_str_from_utf8_prefix", .. } => allocated = true,
                CoreExprKind::ConstFloat(value, _) => floats.push(value.to_bits()),
                _ => {}
            }
        });
        if allocated { continue; }
        let key = serde_json::to_vec(&(&function.params, &function.ret, &function.locals, body, floats)).expect("Core helper serialization");
        let canonical = *bodies.entry(key).or_insert(id as FuncId);
        aliases[id] = canonical;
    }
    for function in &mut program.funcs {
        visit_block_mut(&mut function.body, &mut |e| {
            if let CoreExprKind::Call(target, _) = e.kind.as_mut() { *target = aliases[*target as usize]; }
        });
    }
}

fn visit_block_mut(block: &mut CoreBlock, visitor: &mut impl FnMut(&mut CoreExpr)) {
    for statement in &mut block.stmts {
        let (CoreStmt::Let(_, value) | CoreStmt::Expr(value)) = statement;
        crate::inline::visit_expr_mut(value, visitor);
    }
    if let Some(tail) = &mut block.tail { crate::inline::visit_expr_mut(tail, visitor); }
}
