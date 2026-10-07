//! Coalesce safepoints only for small loops with a checked induction proof.
//!
//! A function containing a coalesced loop gets an entry poll, so repeated calls
//! (including recursion) cannot starve a pending collection. Other loops retain
//! their header polls. Unknown calls, allocation and continue paths are rejected.
use crate::ast::{BinOp, UnOp};
use crate::codegen_escape::PrivateHeap;
use crate::core::{CoreBlock, CoreExpr, CoreExprKind, CoreProgram, CoreStmt, Repr};
use std::collections::{BTreeSet, HashMap};

const MAX_ITERATIONS: i128 = 32;
const MAX_WORK: usize = 8192;

#[derive(Clone, Copy)]
struct Range {
    low: i128,
    high: i128,
}
impl Range {
    fn exact(value: i128) -> Self {
        Self {
            low: value,
            high: value,
        }
    }
    fn join(self, other: Self) -> Self {
        Self {
            low: self.low.min(other.low),
            high: self.high.max(other.high),
        }
    }
}

#[derive(Default)]
pub(crate) struct BoundedLoops {
    bodies: BTreeSet<usize>,
    functions: BTreeSet<usize>,
}
impl BoundedLoops {
    pub(crate) fn analyze(program: &CoreProgram, constants: &PrivateHeap) -> Self {
        let mut analysis = Analysis {
            constants,
            result: Self::default(),
            function: 0,
        };
        for (id, function) in program.funcs.iter().enumerate() {
            if function.is_extern {
                continue;
            }
            analysis.function = id;
            let mut locals = vec![None; function.locals.len()];
            analysis.block(&function.body, &mut locals);
        }
        analysis.result
    }
    pub(crate) fn contains(&self, block: &CoreBlock) -> bool {
        self.bodies.contains(&(block as *const CoreBlock as usize))
    }
    pub(crate) fn entry_poll(&self, id: usize) -> bool {
        self.functions.contains(&id)
    }
}

struct Analysis<'a> {
    constants: &'a PrivateHeap,
    result: BoundedLoops,
    function: usize,
}
type Env = Vec<Option<Range>>;
type Evaluation = (Option<Range>, Option<usize>);

fn total(costs: impl IntoIterator<Item = Option<usize>>) -> Option<usize> {
    costs
        .into_iter()
        .try_fold(1usize, |sum, cost| sum.checked_add(cost?))
        .filter(|sum| *sum <= MAX_WORK)
}
fn statement_expr(statement: &CoreStmt) -> &CoreExpr {
    match statement {
        CoreStmt::Let(_, e) | CoreStmt::Expr(e) => e,
    }
}
fn block_children(block: &CoreBlock) -> Vec<&CoreExpr> {
    block
        .stmts
        .iter()
        .map(statement_expr)
        .chain(block.tail.iter())
        .collect()
}

/// Exhaustive traversal: adding an IR operation must state its operands here.
fn children(e: &CoreExpr) -> Vec<&CoreExpr> {
    use CoreExprKind::*;
    match e.kind.as_ref() {
        Bin(_, a, b) | StrEq(a, b) | StrGet(a, b) => vec![a, b],
        Un(_, v)
        | FloatIntrinsic(_, v)
        | FloatBits(v)
        | Print(v)
        | PrintStr(v)
        | PrintStrRaw(v)
        | StrLen(v)
        | StrToFloat(v)
        | StrHash(v)
        | TypeIdOf(v)
        | ThreadSpawn(v)
        | ThreadJoin(v)
        | ThreadSleep(v)
        | PtrReadI64(v)
        | Panic(v)
        | EnumTag(v)
        | ArrayLen(v) => vec![v],
        StrConcat { a, b, .. } => vec![a, b],
        StrSubstring { s, start, end, .. } => vec![s, start, end],
        StrFromNum { v, .. } => vec![v],
        StrFromChar { cp, .. } => vec![cp],
        ReadFile { path, .. } => vec![path],
        TypeNameOf { obj, .. } => vec![obj],
        AsCBytes { src, .. } => vec![src],
        ArrayNew { len, .. } => vec![len],
        ArrayGet { array, index, .. }
        | ArrayGetUnchecked { array, index, .. }
        | ArrayGetChecked { array, index, .. } => vec![array, index],
        ArraySet {
            array,
            index,
            value,
            ..
        } => vec![array, index, value],
        Cast { value, .. } => vec![value],
        Call(_, args) | RuntimeCall { args, .. } | HostCall { args, .. } | StrJoin { args, .. } => {
            args.iter().collect()
        }
        AtomLoad { atom, .. } => vec![atom],
        AtomCas { atom, old, new } => vec![atom, old, new],
        ChanSend { buf, ctrl, value } => vec![buf, ctrl, value],
        ChanRecv { buf, ctrl, .. } => vec![buf, ctrl],
        CallClosure { callee, args } => std::iter::once(callee.as_ref())
            .chain(args.iter())
            .collect(),
        MakeClosure { captures, .. } => captures.iter().collect(),
        New { fields, .. }
        | MakeValue { fields, .. }
        | MakeVariant { fields, .. }
        | MakeValueVariant { fields, .. } => fields.iter().collect(),
        Field { base, .. } => vec![base],
        SetField { base, value, .. } => vec![base, value],
        Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
            std::iter::once(scrutinee.as_ref())
                .chain(arms.iter().map(|a| &a.body))
                .collect()
        }
        EnumPayload { scrutinee, .. } => vec![scrutinee],
        If(condition, yes, no) => std::iter::once(condition.as_ref())
            .chain(block_children(yes))
            .chain(block_children(no))
            .collect(),
        Block(block) | Loop(block) => block_children(block),
        Break(value) | Return(value) => value.iter().map(|e| e.as_ref()).collect(),
        Assign { value, .. } => vec![value],
        ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..)
        | ConstStr(..) | Unit | Local(..) | Continue | ThreadYield | ThreadCurrentId
        | CallbackPtr(..) => Vec::new(),
    }
}
fn assignments(e: &CoreExpr, out: &mut Vec<u32>) {
    if let CoreExprKind::Assign { local, .. } = e.kind.as_ref() {
        out.push(*local);
    }
    for child in children(e) {
        assignments(child, out);
    }
}
fn resolve<'a>(mut e: &'a CoreExpr, definitions: &HashMap<u32, &'a CoreExpr>) -> &'a CoreExpr {
    for _ in 0..definitions.len() {
        if let CoreExprKind::Local(local) = e.kind.as_ref() {
            if let Some(definition) = definitions.get(local) {
                e = definition;
                continue;
            }
        }
        break;
    }
    e
}

impl Analysis<'_> {
    fn block(&mut self, block: &CoreBlock, env: &mut Env) -> Evaluation {
        let mut costs = Vec::new();
        for statement in &block.stmts {
            let (range, cost) = self.expression(statement_expr(statement), env);
            if let CoreStmt::Let(local, _) = statement {
                env[*local as usize] = range;
            }
            costs.push(cost);
        }
        let (range, cost) = block
            .tail
            .as_ref()
            .map_or((None, Some(0)), |e| self.expression(e, env));
        costs.push(cost);
        (range, total(costs))
    }

    fn expression(&mut self, e: &CoreExpr, env: &mut Env) -> Evaluation {
        use CoreExprKind::*;
        match e.kind.as_ref() {
            Local(local) => {
                let constant = self.constants.integer(e).map(|value| {
                    let value = if matches!(e.repr,Repr::Scalar(s) if s.is_signed()) {
                        value as i64 as i128
                    } else {
                        value as i128
                    };
                    Range::exact(value)
                });
                (env[*local as usize].or(constant), Some(1))
            }
            ConstInt(value, scalar) => {
                let bits = scalar.bits();
                let value = if scalar.is_signed() {
                    (((*value << (64 - bits)) as i64) >> (64 - bits)) as i128
                } else if bits < 64 {
                    (*value & ((1_u64 << bits) - 1)) as i128
                } else {
                    *value as i128
                };
                (Some(Range::exact(value)), Some(1))
            }
            Assign { local, value } => {
                let (range, cost) = self.expression(value, env);
                env[*local as usize] = range;
                (None, total([cost]))
            }
            Bin(op, a, b) => {
                let (a, ca) = self.expression(a, env);
                let (b, cb) = self.expression(b, env);
                let range = a
                    .zip(b)
                    .and_then(|(a, b)| match op {
                        BinOp::Add => Some(Range {
                            low: a.low.checked_add(b.low)?,
                            high: a.high.checked_add(b.high)?,
                        }),
                        BinOp::Sub => Some(Range {
                            low: a.low.checked_sub(b.high)?,
                            high: a.high.checked_sub(b.low)?,
                        }),
                        _ => None,
                    })
                    .filter(|range| {
                        if let Repr::Scalar(s) = e.repr {
                            let (low, high) = if s.is_signed() {
                                (-(1_i128 << (s.bits() - 1)), (1_i128 << (s.bits() - 1)) - 1)
                            } else {
                                (0, (1_i128 << s.bits()) - 1)
                            };
                            range.low >= low && range.high <= high
                        } else {
                            false
                        }
                    });
                (range, total([ca, cb]))
            }
            If(condition, yes, no) => {
                let (_, condition) = self.expression(condition, env);
                let mut other = env.clone();
                let (a, ca) = self.block(yes, env);
                let (b, cb) = self.block(no, &mut other);
                for (a, b) in env.iter_mut().zip(other) {
                    *a = a.zip(b).map(|(a, b)| a.join(b));
                }
                (
                    a.zip(b).map(|(a, b)| a.join(b)),
                    total([condition, ca.zip(cb).map(|(a, b)| a.max(b))]),
                )
            }
            Block(block) => self.block(block, env),
            Loop(block) => self.loop_expression(block, env),
            ArrayLen(array) => {
                let (_, cost) = self.expression(array, env);
                (
                    self.constants
                        .length(array)
                        .map(|n| Range::exact(n as i128)),
                    total([cost]),
                )
            }
            // These nodes lower to fixed instruction sequences with only
            // noncollecting failure paths. Value-array writes box and collect.
            ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..) | Unit
            | CallbackPtr(..) => (None, Some(1)),
            Un(..)
            | FloatIntrinsic(..)
            | FloatBits(..)
            | Cast { .. }
            | ArrayGet { .. }
            | ArrayGetUnchecked { .. }
            | ArrayGetChecked { .. }
            | Break(..)
            | Return(..) => {
                let costs = children(e)
                    .into_iter()
                    .map(|e| self.expression(e, env).1)
                    .collect::<Vec<_>>();
                (None, total(costs))
            }
            Field { loc, .. } | SetField { loc, .. }
                if matches!(loc, crate::core::FieldLoc::Ptr { .. } | crate::core::FieldLoc::Raw { .. }) => {
                let costs = children(e).into_iter().map(|e| self.expression(e, env).1).collect::<Vec<_>>();
                (None, total(costs))
            }
            ArraySet { elem, .. } if !matches!(elem, Repr::Value(_)) => {
                let costs = children(e)
                    .into_iter()
                    .map(|e| self.expression(e, env).1)
                    .collect::<Vec<_>>();
                (None, total(costs))
            }
            _ => {
                // Still visit operands to discover nested small loops. Unknown
                // effects prevent coalescing the surrounding loop.
                for child in children(e) {
                    self.expression(child, env);
                }
                (None, None)
            }
        }
    }

    fn proof(&self, body: &CoreBlock, env: &Env) -> Option<(u32, Range, i128)> {
        use CoreExprKind::*;
        let definitions: HashMap<_, _> = body
            .stmts
            .iter()
            .filter_map(|s| {
                if let CoreStmt::Let(l, e) = s {
                    Some((*l, e))
                } else {
                    None
                }
            })
            .collect();
        let first = body.stmts.iter().find(|s| matches!(s, CoreStmt::Expr(_)))?;
        let If(condition, yes, no) = statement_expr(first).kind.as_ref() else {
            return None;
        };
        if !yes.stmts.is_empty()
            || !matches!(
                yes.tail.as_ref().map(|e| e.kind.as_ref()),
                Some(Break(None))
            )
            || !no.stmts.is_empty()
            || no.tail.is_some()
        {
            return None;
        }
        let Un(UnOp::Not, condition) = resolve(condition, &definitions).kind.as_ref() else {
            return None;
        };
        let Bin(BinOp::Lt, left, right) = resolve(condition, &definitions).kind.as_ref() else {
            return None;
        };
        let Local(induction) = resolve(left, &definitions).kind.as_ref() else {
            return None;
        };
        let right = resolve(right, &definitions);
        let bound = match right.kind.as_ref() {
            ConstInt(n, _) => Some(*n),
            _ => self.constants.integer(right),
        }?;
        if bound > MAX_ITERATIONS as u64 {
            return None;
        }
        let initial = env[*induction as usize]?;
        if initial.low < 0 || initial.high > MAX_ITERATIONS {
            return None;
        }
        let Assign { local, value } = statement_expr(body.stmts.last()?).kind.as_ref() else {
            return None;
        };
        if local != induction {
            return None;
        }
        let Bin(BinOp::Add, a, b) = resolve(value, &definitions).kind.as_ref() else {
            return None;
        };
        if !matches!(resolve(a,&definitions).kind.as_ref(),Local(i) if i==induction)
            || !matches!(resolve(b, &definitions).kind.as_ref(), ConstInt(1, _))
        {
            return None;
        }
        let mut writes = Vec::new();
        for e in block_children(body) {
            assignments(e, &mut writes);
        }
        if writes.iter().filter(|&&local| local == *induction).count() != 1 {
            return None;
        }
        // The proof relies on a no-overflow increment in an i64 counter.
        if !matches!(left.repr, Repr::Scalar(crate::core::ScalarRepr::I64)) {
            return None;
        }
        Some((*induction, initial, bound as i128))
    }

    fn loop_expression(&mut self, body: &CoreBlock, env: &mut Env) -> Evaluation {
        let proof = self.proof(body, env);
        let mut writes = Vec::new();
        for e in block_children(body) {
            assignments(e, &mut writes);
        }
        let mut iteration = env.clone();
        for &local in &writes {
            iteration[local as usize] = None;
        }
        if let Some((induction, initial, bound)) = proof {
            iteration[induction as usize] = Some(Range {
                low: initial.low.min(bound),
                high: (bound - 1).max(initial.low.min(bound)),
            });
        }
        let (_, cost) = self.block(body, &mut iteration);
        let cost = proof.and_then(|(_, initial, bound)| {
            let trips = (bound - initial.low).max(0) as usize;
            cost?
                .checked_mul(trips + 1)
                .filter(|work| *work <= MAX_WORK)
        });
        if cost.is_some() {
            self.result.bodies.insert(body as *const CoreBlock as usize);
            self.result.functions.insert(self.function);
        }
        for local in writes {
            env[local as usize] = None;
        }
        (None, cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inkwell::values::AnyValue;

    fn analyze(source: &str) -> (CoreProgram, BoundedLoops) {
        let (module, _) = crate::compile::parse_with_prelude(source).unwrap();
        let program =
            crate::lower::lower_program(&crate::resolve::resolve_module(module).unwrap().globals)
                .unwrap();
        let constants = PrivateHeap::analyze(&program);
        let loops = BoundedLoops::analyze(&program, &constants);
        (program, loops)
    }

    fn loop_counts(e: &CoreExpr, proof: &BoundedLoops) -> (usize, usize) {
        let mut counts = (0, 0);
        if let CoreExprKind::Loop(body) = e.kind.as_ref() {
            counts.0 += 1;
            counts.1 += usize::from(proof.contains(body));
        }
        for child in children(e) {
            let next = loop_counts(child, proof);
            counts.0 += next.0;
            counts.1 += next.1;
        }
        counts
    }

    #[test]
    fn nested_small_inductions_coalesce_and_entry_poll_keeps_incoming_roots() {
        let source="
            fn bounded(mut a: Array<i64>, n: i64) -> i64 {
                let mut i=0; while i<n { let mut j=i+1; while j<n { array_set(a,j,7); j=j+1; } i=i+1; } a[2]
            }
            fn main() -> i64 { let mut a: Array<i64> = array_new(3); bounded(a,3) }
        ";
        let (program, proof) = analyze(source);
        let id = program
            .funcs
            .iter()
            .position(|f| f.name == "bounded")
            .unwrap();
        let counts = block_children(&program.funcs[id].body)
            .into_iter()
            .map(|e| loop_counts(e, &proof))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        assert_eq!(counts, (2, 2));
        assert!(proof.entry_poll(id));
        let context = inkwell::context::Context::create();
        let compiled = crate::codegen::codegen(&context, &program).unwrap();
        let ir = compiled
            .module
            .get_function("bounded")
            .unwrap()
            .print_to_string()
            .to_string();
        assert_eq!(
            ir.matches("call void @ai_gc_pollcheck_slow").count(),
            1,
            "{ir}"
        );
        assert!(
            ir.contains("entry.gc.slow") && ir.contains("entry.root"),
            "{ir}"
        );
        assert_eq!(crate::codegen::jit_run_i64_gc(&program, true).unwrap(), 7);
    }

    #[test]
    fn uncertain_progress_side_effects_large_bounds_and_resets_keep_header_polls() {
        for body in [
            "let mut i=0; while i<33 { i=i+1; } i",
            "let mut i=0; while i<5 { if i==2 { continue; } i=i+1; } i",
            "let mut i=0; while i<5 { i=0; i=i+1; } i",
            "let mut i=0; let mut n=5; while i<n { n=n+1; i=i+1; } i",
            "let mut i=0; while i<5 { print_int(i); i=i+1; } i",
            "let mut i=0; while i<5 { let a: Array<i64> = array_new(2); i=i+1; } i",
        ] {
            let source = format!("fn main() -> i64 {{ {body} }}");
            let (program, proof) = analyze(&source);
            assert!(proof.bodies.is_empty(), "incorrect proof for {body}");
            assert!(!proof.entry_poll(program.entry.unwrap() as usize));
        }
    }

    #[test]
    fn unknown_outer_loop_retains_poll_while_inner_bounded_work_can_coalesce() {
        let source="fn main() -> i64 { let mut s=0; while s<100 { let mut j=0; while j<3 { j=j+1; } s=s+1; } s }";
        let (program, proof) = analyze(source);
        assert_eq!(proof.bodies.len(), 1);
        assert_eq!(crate::codegen::jit_run_i64_gc(&program, true).unwrap(), 100);
    }
}
