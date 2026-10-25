//! Closed-world propagation of immutable enum discriminants and boolean arguments.
//! Facts join all direct callers, assignments and returns. Address-taken entry
//! points and opaque results remain unknown. Removing a proven dead branch may
//! expose another constant argument, so repeat until the Core stops changing.
use std::collections::VecDeque;
use crate::core::*;
use crate::ast::{BinOp, UnOp};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Constant { Tag(u32), Bool(bool) }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Fact { #[default] Bottom, Known(Constant), Unknown }
impl Fact {
    fn join(&mut self, other: Self) -> bool {
        let next = match (*self, other) {
            (Self::Bottom, value) | (value, Self::Bottom) => value,
            (Self::Known(a), Self::Known(b)) if a == b => *self,
            _ => Self::Unknown,
        };
        let changed = next != *self;
        *self = next;
        changed
    }
}
type Node = usize;
struct Analysis<'a> {
    program: &'a CoreProgram,
    reference_enums: Vec<bool>,
    locals: Vec<Vec<Option<Node>>>,
    returns: Vec<Option<Node>>,
    facts: Vec<Fact>,
    edges: Vec<Vec<Node>>,
    function: usize,
    loops: Vec<Option<Node>>,
    onces: Vec<(u32, Option<Node>)>,
}
fn reference_enum_layouts(program: &CoreProgram) -> Vec<bool> {
        // Reference-enum tags are immutable in typed Core, just as value
        // tags are. Keep conservative exclusions for explicitly exposed or
        // mutated layouts, including manually assembled Core programs.
        let mut reference_enums: Vec<_> = program.layouts.iter().map(|layout| matches!(layout.meta.kind, crate::gc::TypeKind::Enum { .. })).collect();
        for function in &program.funcs {
            if function.is_extern {
                for repr in &function.params {
                    if let Repr::Ref(layout) = repr { reference_enums[*layout as usize] = false; }
                }
            }
            crate::inline::visit_block(&function.body, &mut |e| match e.kind.as_ref() {
                CoreExprKind::SetField { base, .. } | CoreExprKind::AsCBytes { src: base, .. } => {
                    if let Repr::Ref(layout) = base.repr { reference_enums[layout as usize] = false; }
                }
                CoreExprKind::Cast { from: Repr::Ref(layout), to: Repr::Scalar(ScalarRepr::Ptr), .. } => reference_enums[*layout as usize] = false,
                _ => {}
            });
        }
    reference_enums
}
impl<'a> Analysis<'a> {
    fn new(program: &'a CoreProgram, reference_enums: Vec<bool>) -> Self {
        let mut this = Self { program, reference_enums, locals: Vec::new(), returns: Vec::new(), facts: Vec::new(), edges: Vec::new(), function: 0, loops: Vec::new(), onces: Vec::new() };
        for function in &program.funcs {
            let locals = function.locals.iter().map(|repr| this.node(repr)).collect();
            this.locals.push(locals);
            let ret = this.node(&function.ret);
            this.returns.push(ret);
        }
        if let Some(entry) = program.entry { this.external_entry(entry as usize); }
        this
    }
    fn node(&mut self, repr: &Repr) -> Option<Node> {
        let tracked = matches!(repr, Repr::Scalar(ScalarRepr::Bool))
            || matches!(repr, Repr::Value(id) if self.program.values[*id as usize].variants.is_some())
            || matches!(repr, Repr::Ref(id) if self.reference_enums[*id as usize]);
        if !tracked { return None; }
        let node = self.facts.len();
        self.facts.push(Fact::Bottom);
        self.edges.push(Vec::new());
        Some(node)
    }
    fn fact(&mut self, node: Option<Node>, fact: Fact) {
        if let Some(node) = node { self.facts[node].join(fact); }
    }
    fn flow(&mut self, from: Option<Node>, to: Option<Node>) {
        // None is Unit or a diverging path, not an unknown produced value.
        // Every tracked expression, including opaque ones, has its own node.
        if let (Some(from), Some(to)) = (from, to) { self.edges[from].push(to); }
    }
    fn external_entry(&mut self, id: usize) {
        let function = &self.program.funcs[id];
        // A foreign function passed straight to C has no Core body or locals.
        if function.is_extern { return; }
        for local in 0..function.closure_captures.len() + function.params.len() {
            self.fact(self.locals[id][local], Fact::Unknown);
        }
    }
    fn block(&mut self, block: &CoreBlock) -> Option<Node> {
        for statement in &block.stmts {
            match statement {
                CoreStmt::Let(local, value) => {
                    let value = self.expression(value);
                    self.flow(value, self.locals[self.function][*local as usize]);
                }
                CoreStmt::Expr(value) => { self.expression(value); }
            }
        }
        block.tail.as_ref().and_then(|tail| self.expression(tail))
    }
    fn expression(&mut self, expression: &CoreExpr) -> Option<Node> {
        use CoreExprKind::*;
        let result = self.node(&expression.repr);
        match expression.kind.as_ref() {
            Local(local) => self.flow(self.locals[self.function][*local as usize], result),
            ConstBool(value) => self.fact(result, Fact::Known(Constant::Bool(*value))),
            MakeValueVariant { tag, fields, .. } | MakeVariant { tag, fields, .. } => {
                for field in fields { self.expression(field); }
                self.fact(result, Fact::Known(Constant::Tag(*tag)));
            }
            Assign { local, value } => {
                let value = self.expression(value);
                self.flow(value, self.locals[self.function][*local as usize]);
                self.flow(value, result);
            }
            Call(id, args) => {
                let id = *id as usize;
                let function = &self.program.funcs[id];
                for (index, arg) in args.iter().enumerate() {
                    let value = self.expression(arg);
                    if function.is_extern {
                        // C copy-out may overwrite a local without a Core Assign.
                        if function.extern_by_ref.get(index).copied().unwrap_or(false) {
                            if let Local(local) = arg.kind.as_ref() {
                                self.fact(self.locals[self.function][*local as usize], Fact::Unknown);
                            }
                        }
                    } else {
                        self.flow(value, self.locals[id][function.closure_captures.len() + index]);
                    }
                }
                if function.is_extern { self.fact(result, Fact::Unknown); }
                else { self.flow(self.returns[id], result); }
            }
            MakeClosure { code, captures, .. } => {
                self.external_entry(*code as usize);
                for capture in captures { self.expression(capture); }
                self.fact(result, Fact::Unknown);
            }
            CallbackPtr(code) => { self.external_entry(*code as usize); }
            If(condition, yes, no) => {
                self.expression(condition);
                let yes = self.block(yes);
                let no = self.block(no);
                self.flow(yes, result); self.flow(no, result);
            }
            Block(block) => { let value = self.block(block); self.flow(value, result); }
            Loop(block) => {
                self.loops.push(result);
                self.block(block);
                self.loops.pop();
            }
            Once { label, body } => {
                self.onces.push((*label, result));
                let value = self.block(body);
                self.flow(value, result);
                self.onces.pop();
            }
            Break(value) => {
                let value = value.as_ref().and_then(|value| self.expression(value));
                self.flow(value, self.loops.last().copied().flatten());
            }
            Return(value) => {
                let value = value.as_ref().and_then(|value| self.expression(value));
                self.flow(value, self.returns[self.function]);
            }
            Leave { label, value } => {
                let value = value.as_ref().and_then(|value| self.expression(value));
                self.flow(value, self.onces.iter().rev().find(|(l, _)| l == label).and_then(|(_, node)| *node));
            }
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                self.expression(scrutinee);
                for arm in arms {
                    for local in &arm.binds { self.fact(self.locals[self.function][*local as usize], Fact::Unknown); }
                    let value = self.expression(&arm.body);
                    self.flow(value, result);
                }
            }
            _ => {
                for child in crate::copyprop::children(expression) { self.expression(child); }
                self.fact(result, Fact::Unknown);
            }
        }
        result
    }
    fn solve(mut self, live: &[bool]) -> Vec<Vec<Fact>> {
        for (id, function) in self.program.funcs.iter().enumerate() {
            if !live[id] || function.is_extern { continue; }
            self.function = id;
            let value = self.block(&function.body);
            self.flow(value, self.returns[id]);
        }
        let mut queue: VecDeque<_> = self.facts.iter().enumerate().filter_map(|(id, fact)| (*fact != Fact::Bottom).then_some(id)).collect();
        let mut queued = vec![false; self.facts.len()];
        for &id in &queue { queued[id] = true; }
        while let Some(source) = queue.pop_front() {
            queued[source] = false;
            let value = self.facts[source];
            for &destination in &self.edges[source] {
                if self.facts[destination].join(value) && !queued[destination] {
                    queued[destination] = true;
                    queue.push_back(destination);
                }
            }
        }
        self.locals.iter().map(|locals| locals.iter().map(|node| node.map_or(Fact::Bottom, |node| self.facts[node])).collect()).collect()
    }
}

pub(crate) fn propagate(program: &mut CoreProgram) {
    if !crate::inline::enabled() || program.entry.is_none() { return; }
    let reference_enums = reference_enum_layouts(program);
    loop {
        let live = crate::reachable::live_functions(program);
        let facts = Analysis::new(program, reference_enums.clone()).solve(&live);
        let mut changed = false;
        for (id, function) in program.funcs.iter_mut().enumerate() {
            if live[id] && !function.is_extern { rewrite_block(&mut function.body, &facts[id], &mut changed); }
        }
        if !changed { break; }
    }
}
fn known(expression: &CoreExpr, facts: &[Fact]) -> Option<Constant> {
    match expression.kind.as_ref() {
        CoreExprKind::Local(local) => match facts[*local as usize] { Fact::Known(value) => Some(value), _ => None },
        CoreExprKind::ConstBool(value) => Some(Constant::Bool(*value)),
        CoreExprKind::MakeValueVariant { tag, .. } | CoreExprKind::MakeVariant { tag, .. } => Some(Constant::Tag(*tag)),
        _ => None,
    }
}
fn rewrite_block(block: &mut CoreBlock, facts: &[Fact], changed: &mut bool) {
    for statement in &mut block.stmts {
        let (CoreStmt::Let(_, value) | CoreStmt::Expr(value)) = statement;
        rewrite(value, facts, changed);
    }
    if let Some(tail) = &mut block.tail { rewrite(tail, facts, changed); }
}
fn rewrite(expression: &mut CoreExpr, facts: &[Fact], changed: &mut bool) {
    use CoreExprKind::*;
    match expression.kind.as_mut() {
        If(condition, yes, no) => { rewrite(condition, facts, changed); rewrite_block(yes, facts, changed); rewrite_block(no, facts, changed); }
        Block(block) | Loop(block) | Once { body: block, .. } => rewrite_block(block, facts, changed),
        Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
            rewrite(scrutinee, facts, changed);
            for arm in arms { rewrite(&mut arm.body, facts, changed); }
        }
        _ => for child in crate::copyprop::children_mut(expression) { rewrite(child, facts, changed); },
    }
    let replacement = match expression.kind.as_mut() {
        Local(_) => match known(expression, facts) { Some(Constant::Bool(value)) => Some(ConstBool(value)), _ => None },
        Un(UnOp::Not, value) => match value.kind.as_ref() { ConstBool(value) => Some(ConstBool(!value)), _ => None },
        Bin(op, left, right) => match (left.kind.as_ref(), right.kind.as_ref()) {
            (ConstBool(a), ConstBool(b)) => match op {
                BinOp::And | BinOp::BitAnd => Some(ConstBool(*a && *b)),
                BinOp::Or | BinOp::BitOr => Some(ConstBool(*a || *b)),
                BinOp::BitXor | BinOp::Ne => Some(ConstBool(*a != *b)),
                BinOp::Eq => Some(ConstBool(*a == *b)),
                _ => None,
            },
            _ => None,
        },
        If(condition, yes, no) => match condition.kind.as_ref() {
            ConstBool(value) => Some(Block(std::mem::replace(if *value { yes } else { no }, Box::new(CoreBlock { stmts: Vec::new(), tail: None })))),
            _ => None,
        },
        ValueMatch { scrutinee, arms } | Match { scrutinee, arms } => {
            if let Some(Constant::Tag(tag)) = known(scrutinee, facts) {
                if let Some(index) = arms.iter().position(|arm| arm.tag == tag).or_else(|| arms.iter().position(|arm| arm.tag == u32::MAX)) {
                    if arms.len() > 1 {
                        let arm = arms.remove(index);
                        arms.clear(); arms.push(arm);
                        *changed = true;
                    }
                }
            }
            None
        }
        EnumTag(value) if matches!(value.kind.as_ref(), Local(_)) => match known(value, facts) {
            Some(Constant::Tag(tag)) => match expression.repr { Repr::Scalar(scalar) => Some(ConstInt(tag as u128, scalar)), _ => None },
            _ => None,
        },
        _ => None,
    };
    if let Some(replacement) = replacement { *expression.kind = replacement; *changed = true; }
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
    fn arms(program: &CoreProgram, function: &str) -> usize {
        let mut count = 0;
        let body = &program.funcs.iter().find(|f| f.name == function).unwrap().body;
        crate::inline::visit_block(body, &mut |e| {
            if let CoreExprKind::ValueMatch { arms, .. } | CoreExprKind::Match { arms, .. } = e.kind.as_ref() { count += arms.len(); }
        });
        count
    }
    const CHOOSE: &str = r#"
        #[value] enum Mode { Fast, Slow(i64) }
        #[inline(never)] fn choose(mode: Mode) -> i64 {
            match mode { Mode::Fast => 7, Mode::Slow(value) => value }
        }
    "#;
    #[test]
    fn constant_enum_arguments_keep_payload_bindings() {
        for (constructor, expected) in [("Mode::Fast", 7), ("Mode::Slow(11)", 11)] {
            let mut p = program(&format!("{CHOOSE} fn main() -> i64 {{ choose({constructor}) }}"));
            assert_eq!(arms(&p, "choose"), 2);
            propagate(&mut p);
            assert_eq!(arms(&p, "choose"), 1);
            assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), expected);
        }
    }
    #[test]
    fn immutable_reference_enum_tags_keep_payloads_across_collection() {
        let mut p = program(r#"
            enum Mode { Fast, Slow(i64), Next(Box<Mode>) }
            #[inline(never)] fn choose(mode: Mode) -> i64 {
                match mode { Mode::Fast => 7, Mode::Slow(value) => value, Mode::Next(_) => 99 }
            }
            fn main() -> i64 {
                let mode = Mode::Slow(11);
                let garbage: Array<i64> = array_new(64);
                choose(mode) + array_len(garbage)
            }
        "#);
        assert!(matches!(p.funcs.iter().find(|f| f.name == "choose").unwrap().params[0], Repr::Ref(_)));
        assert_eq!(arms(&p, "choose"), 3);
        propagate(&mut p);
        assert_eq!(arms(&p, "choose"), 1);
        assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&p, true) }.unwrap(), 75);
    }

    #[test]
    fn differing_callers_and_assignments_do_not_invent_a_constant_tag() {
        for (body, expected) in [
            ("choose(Mode::Fast) + choose(Mode::Slow(11))", 18),
            ("let mut mode = Mode::Fast; mode = Mode::Slow(11); choose(mode)", 11),
        ] {
            let mut p = program(&format!("{CHOOSE} fn main() -> i64 {{ {body} }}"));
            propagate(&mut p);
            assert_eq!(arms(&p, "choose"), 2);
            assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), expected);
        }
    }
    #[test]
    fn constants_flow_through_recursive_calls_and_early_returns() {
        let mut p = program(&format!(r#"{CHOOSE}
            #[inline(never)] fn mode(n: i64) -> Mode {{
                if n == 0 {{ return Mode::Fast; }}
                mode(n - 1)
            }}
            fn main() -> i64 {{ choose(mode(4)) }}
        "#));
        propagate(&mut p);
        assert_eq!(arms(&p, "choose"), 1);
        assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), 7);
    }
    #[test]
    fn closure_entry_arguments_remain_unknown() {
        let mut p = program(&format!(r#"{CHOOSE}
            fn main() -> i64 {{
                let f = |mode: Mode| choose(mode);
                f(Mode::Slow(11)) + choose(Mode::Fast)
            }}
        "#));
        propagate(&mut p);
        assert_eq!(arms(&p, "choose"), 2);
        assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), 18);
    }
    #[test]
    fn folding_a_boolean_result_preserves_the_call_side_effects() {
        let mut p = program(r#"
            #[inline(never)] fn flag(mut counter: Array<i64>) -> bool {
                counter[0] += 1;
                return true;
            }
            #[inline(never)] fn choose(flag: bool) -> i64 { if flag { 7 } else { 99 } }
            fn main() -> i64 {
                let counter: Array<i64> = array_new(1);
                choose(flag(counter)) + counter[0]
            }
        "#);
        propagate(&mut p);
        assert_eq!(unsafe { crate::codegen::jit_run_i64(&p) }.unwrap(), 8);
    }
}
