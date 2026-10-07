//! Backward local liveness at statement boundaries. Clearing dead traced
//! mirrors avoids retaining garbage through later calls and loop iterations.
//! Loop headers use a monotone fixed point; break/continue and returns have
//! their actual successor sets. Every operand kind is handled explicitly.
use std::collections::{BTreeSet, HashMap};
use crate::core::*;
type Live = BTreeSet<LocalId>;

#[derive(Default)]
pub(crate) struct RootLiveness { dead: HashMap<usize, Live> }
impl RootLiveness {
    pub(crate) fn analyze(program: &CoreProgram) -> Self {
        let mut result = Self::default();
        for function in &program.funcs {
            if function.is_extern { continue; }
            let mut analysis = Analysis {
                dead: &mut result.dead, loops: Vec::new(), definitions: Live::new(),
                all: (0..function.locals.len() as LocalId).collect(),
            };
            analysis.block(&function.body, Live::new());
        }
        result
    }
    pub(crate) fn dead(&self, statement: &CoreStmt) -> Option<&Live> {
        self.dead.get(&(statement as *const CoreStmt as usize))
    }
}
struct Analysis<'a> {
    dead: &'a mut HashMap<usize, Live>,
    definitions: Live,
    loops: Vec<(Live, Live)>, // break and continue successors
    all: Live,
}
impl Analysis<'_> {
    fn block(&mut self, block: &CoreBlock, mut live: Live) -> Live {
        if let Some(tail) = &block.tail { live = self.expr(tail, live); }
        for statement in block.stmts.iter().rev() {
            let after = live.clone();
            let surrounding_defs = std::mem::take(&mut self.definitions);
            let expression = match statement {
                CoreStmt::Let(local, expression) => {
                    live.remove(local); self.definitions.insert(*local); expression
                }
                CoreStmt::Expr(expression) => expression,
            };
            live = self.expr(expression, live);
            let mut dead = live.clone();
            dead.extend(&self.definitions);
            dead.retain(|local| !after.contains(local));
            self.dead.insert(statement as *const CoreStmt as usize, dead);
            self.definitions.extend(surrounding_defs);
        }
        live
    }
    fn operands<'a>(&mut self, expressions: impl DoubleEndedIterator<Item = &'a CoreExpr>, mut live: Live) -> Live {
        for expression in expressions.rev() { live = self.expr(expression, live); }
        live
    }
    fn expr(&mut self, expression: &CoreExpr, mut live: Live) -> Live {
        use CoreExprKind::*;
        match expression.kind.as_ref() {
            Local(local) => { live.insert(*local); live }
            ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..)
            | ConstStr(..) | Unit | CallbackPtr(..) | ThreadYield | ThreadCurrentId => live,
            Assign { local, value } => { self.definitions.insert(*local); live.remove(local); self.expr(value, live) }
            Bin(_, a, b) | StrEq(a, b) | StrGet(a, b) | StrConcat { a, b, .. } => {
                let live = self.expr(b, live); self.expr(a, live)
            }
            Un(_, e) | FloatIntrinsic(_, e) | FloatBits(e) | Print(e) | PrintStr(e)
            | PrintStrRaw(e) | StrLen(e) | StrToFloat(e) | StrHash(e) | TypeIdOf(e)
            | PtrReadI64(e) | Panic(e) | ThreadSpawn(e) | ThreadJoin(e) | ThreadSleep(e)
            | EnumTag(e) | ArrayLen(e) | TypeNameOf { obj: e, .. } | Cast { value: e, .. }
            | StrFromNum { v: e, .. } | StrFromChar { cp: e, .. } | ReadFile { path: e, .. }
            | AsCBytes { src: e, .. } | Field { base: e, .. } | EnumPayload { scrutinee: e, .. }
            | ArrayNew { len: e, .. } | AtomLoad { atom: e, .. } => self.expr(e, live),
            Call(_, args) | HostCall { args, .. } | RuntimeCall { args, .. } | StrJoin { args, .. }
            | New { fields: args, .. } | MakeVariant { fields: args, .. }
            | MakeValue { fields: args, .. } | MakeValueVariant { fields: args, .. }
            | MakeClosure { captures: args, .. } => self.operands(args.iter(), live),
            ArrayGet { array, index, .. } | ArrayGetChecked { array, index, .. }
            | ArrayGetUnchecked { array, index, .. } => {
                let live = self.expr(index, live); self.expr(array, live)
            }
            ArraySet { array, index, value, .. } => {
                let live = self.expr(value, live);
                let live = self.expr(index, live); self.expr(array, live)
            }
            SetField { base, value, .. } => { let live = self.expr(value, live); self.expr(base, live) }
            AtomCas { atom, old, new } => {
                let live = self.expr(new, live); let live = self.expr(old, live); self.expr(atom, live)
            }
            ChanSend { buf, ctrl, value } => {
                let live = self.expr(value, live); let live = self.expr(ctrl, live); self.expr(buf, live)
            }
            ChanRecv { buf, ctrl, .. } => { let live = self.expr(ctrl, live); self.expr(buf, live) }
            StrSubstring { s, start, end, .. } => {
                let live = self.expr(end, live); let live = self.expr(start, live); self.expr(s, live)
            }
            CallClosure { callee, args } => {
                let live = self.operands(args.iter(), live); self.expr(callee, live)
            }
            If(condition, yes, no) => {
                let mut yes_live = self.block(yes, live.clone());
                yes_live.extend(self.block(no, live));
                self.expr(condition, yes_live)
            }
            Block(block) => self.block(block, live),
            Loop(block) => {
                let mut header = Live::new();
                loop {
                    self.loops.push((live.clone(), header.clone()));
                    let before = self.block(block, header.clone());
                    self.loops.pop();
                    let mut next = header.clone(); next.extend(before);
                    if next == header { return header; }
                    header = next;
                }
            }
            Break(value) => {
                let successor = self.loops.last().map(|l| l.0.clone()).unwrap_or_else(|| self.all.clone());
                value.as_ref().map_or(successor.clone(), |value| self.expr(value, successor))
            }
            Continue => self.loops.last().map(|l| l.1.clone()).unwrap_or_else(|| self.all.clone()),
            Return(value) => value.as_ref().map_or_else(Live::new, |value| self.expr(value, Live::new())),
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                let mut before = Live::new();
                for arm in arms {
                    let mut arm_live = self.expr(&arm.body, live.clone());
                    for local in &arm.binds { arm_live.remove(local); }
                    before.extend(arm_live);
                }
                self.expr(scrutinee, before)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn local(id: LocalId) -> CoreExpr { CoreExpr::new(CoreExprKind::Local(id), Repr::Ref(0)) }
    fn statement(expression: CoreExpr) -> CoreStmt { CoreStmt::Expr(expression) }
    fn analyze(block: &CoreBlock, count: u32) -> HashMap<usize, Live> {
        let mut after = HashMap::new();
        Analysis { dead: &mut after, loops: Vec::new(), definitions: Live::new(), all: (0..count).collect() }.block(block, Live::new());
        after
    }
    #[test]
    fn last_reads_and_redefinitions_end_root_lifetimes() {
        let block = CoreBlock { stmts: vec![statement(local(0)),
            CoreStmt::Let(0, CoreExpr::new(CoreExprKind::ConstZero(Repr::Ref(0)), Repr::Ref(0))),
            statement(local(0)), statement(local(1))], tail: None };
        let after = analyze(&block, 2);
        assert_eq!(after[&(&block.stmts[0] as *const CoreStmt as usize)], [0].into());
        assert_eq!(after[&(&block.stmts[1] as *const CoreStmt as usize)], Live::new());
        assert_eq!(after[&(&block.stmts[2] as *const CoreStmt as usize)], [0].into());
    }
    #[test]
    fn loop_carried_reads_survive_and_overwritten_values_die() {
        let body = CoreBlock { stmts: vec![statement(local(0)),
            CoreStmt::Let(1, CoreExpr::new(CoreExprKind::ConstZero(Repr::Ref(0)), Repr::Ref(0))),
            statement(local(1)), statement(CoreExpr::new(CoreExprKind::Continue, Repr::Unit))], tail: None };
        let block = CoreBlock { stmts: vec![statement(CoreExpr::new(CoreExprKind::Loop(Box::new(body)), Repr::Unit))], tail: None };
        let after = analyze(&block, 2);
        let CoreExprKind::Loop(body) = (match &block.stmts[0] { CoreStmt::Expr(e) => e.kind.as_ref(), _ => unreachable!() }) else { unreachable!() };
        assert_eq!(after[&(&body.stmts[2] as *const CoreStmt as usize)], [1].into());
    }
}
