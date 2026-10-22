//! Definite assignment over typed source-order Core IR, before ANF and backend
//! optimizations. Zeroed GC root storage is not a language-level initialized
//! value. Unreachable paths do not contribute to the state at a merge.
use crate::ast::BinOp;
use crate::core::*;
use std::collections::HashSet;
type State = HashSet<LocalId>;
type Flow = Option<State>;
type Result = std::result::Result<(), (LocalId, SpanId)>;

pub(crate) fn check(body: &CoreBlock, deferred: &[LocalId]) -> Result {
    if deferred.is_empty() {
        return Ok(());
    }
    Checker {
        deferred: deferred.iter().copied().collect(),
        breaks: Vec::new(),
        leaves: Vec::new(),
    }
    .block(body, &mut Some(State::new()))
}
struct Checker {
    deferred: State,
    breaks: Vec<Vec<State>>,
    /// The states reaching each enclosing `Once` exit, by label.
    leaves: Vec<(u32, Vec<State>)>,
}
fn merge(a: Flow, b: Flow) -> Flow {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(mut a), Some(b)) => {
            a.extend(b);
            Some(a)
        }
    }
}
impl Checker {
    fn block(&mut self, block: &CoreBlock, flow: &mut Flow) -> Result {
        for stmt in &block.stmts {
            if flow.is_none() {
                break;
            }
            match stmt {
                CoreStmt::Let(id, value) => {
                    self.expr(value, flow)?;
                    if let Some(state) = flow {
                        if self.deferred.contains(id) {
                            state.insert(*id);
                        } else {
                            state.remove(id);
                        }
                    }
                }
                CoreStmt::Expr(value) => self.expr(value, flow)?,
            }
        }
        if let Some(value) = &block.tail {
            self.expr(value, flow)?;
        }
        Ok(())
    }
    fn values(&mut self, values: &[CoreExpr], flow: &mut Flow) -> Result {
        for value in values {
            self.expr(value, flow)?;
        }
        Ok(())
    }
    fn expr(&mut self, expr: &CoreExpr, flow: &mut Flow) -> Result {
        if flow.is_none() {
            return Ok(());
        }
        use CoreExprKind::*;
        match expr.kind.as_ref() {
            Local(id) => {
                if flow.as_ref().unwrap().contains(id) {
                    return Err((*id, expr.span));
                }
            }
            Assign { local, value } => {
                self.expr(value, flow)?;
                if let Some(state) = flow {
                    state.remove(local);
                }
            }
            If(condition, yes, no) => {
                self.expr(condition, flow)?;
                let mut a = flow.clone();
                let mut b = flow.clone();
                self.block(yes, &mut a)?;
                self.block(no, &mut b)?;
                *flow = merge(a, b);
            }
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                self.expr(scrutinee, flow)?;
                let mut result = None;
                for arm in arms {
                    let mut branch = flow.clone();
                    if let Some(state) = &mut branch {
                        for id in &arm.binds {
                            state.remove(id);
                        }
                    }
                    self.expr(&arm.body, &mut branch)?;
                    result = merge(result, branch);
                }
                *flow = result;
            }
            Block(body) => self.block(body, flow)?,
            Loop(body) => {
                self.breaks.push(Vec::new());
                let mut body_flow = flow.clone();
                self.block(body, &mut body_flow)?;
                let exits = self.breaks.pop().unwrap();
                *flow = exits.into_iter().fold(None, |a, b| merge(a, Some(b)));
            }
            Break(value) => {
                if let Some(value) = value {
                    self.expr(value, flow)?;
                }
                if let Some(state) = flow.take() {
                    self.breaks
                        .last_mut()
                        .expect("lowering validated break scope")
                        .push(state);
                }
            }
            Once { label, body } => {
                self.leaves.push((*label, Vec::new()));
                self.block(body, flow)?;
                let (_, exits) = self.leaves.pop().unwrap();
                *flow = exits.into_iter().fold(flow.take(), |a, b| merge(a, Some(b)));
            }
            Leave { label, value } => {
                if let Some(value) = value {
                    self.expr(value, flow)?;
                }
                if let Some(state) = flow.take() {
                    let exits = self.leaves.iter_mut().rev().find(|(l, _)| l == label)
                        .expect("leave inside its once");
                    exits.1.push(state);
                }
            }
            Continue => *flow = None,
            Return(value) => {
                if let Some(value) = value {
                    self.expr(value, flow)?;
                }
                *flow = None;
            }
            Bin(BinOp::And | BinOp::Or, a, b) => {
                self.expr(a, flow)?;
                let skip = flow.clone();
                self.expr(b, flow)?;
                *flow = merge(skip, flow.take());
            }
            Bin(_, a, b) | StrEq(a, b) | StrGet(a, b) | StrConcat { a, b, .. } => {
                self.expr(a, flow)?;
                self.expr(b, flow)?;
            }
            Un(_, value)
            | FloatIntrinsic(_, value)
            | Print(value)
            | PrintStr(value)
            | PrintStrRaw(value)
            | StrLen(value)
            | StrEqLit(value, _)
            | StrToFloat(value)
            | FloatBits(value)
            | StrHash(value)
            | TypeIdOf(value)
            | ArrayLen(value)
            | ThreadSpawn(value)
            | ThreadJoin(value)
            | ThreadSleep(value)
            | PtrReadI64(value)
            | EnumTag(value)
            | Cast { value, .. }
            | StrFromNum { v: value, .. }
            | StrFromChar { cp: value, .. }
            | ReadFile { path: value, .. }
            | TypeNameOf { obj: value, .. }
            | AsCBytes { src: value, .. }
            | ArrayNew { len: value, .. }
            | AtomLoad { atom: value, .. }
            | Field { base: value, .. }
            | EnumPayload {
                scrutinee: value, ..
            } => self.expr(value, flow)?,
            StrSubstring { s, start, end, .. } => {
                self.expr(s, flow)?;
                self.expr(start, flow)?;
                self.expr(end, flow)?;
            }
            ArrayGet { array, index, .. }
            | ArrayGetUnchecked { array, index, .. }
            | ArrayGetChecked { array, index, .. } => {
                self.expr(array, flow)?;
                self.expr(index, flow)?;
            }
            ArraySet {
                array,
                index,
                value,
                ..
            } => {
                self.expr(array, flow)?;
                self.expr(index, flow)?;
                self.expr(value, flow)?;
            }
            SetField { base, value, .. } => {
                self.expr(base, flow)?;
                self.expr(value, flow)?;
            }
            AtomCas { atom, old, new } => {
                self.expr(atom, flow)?;
                self.expr(old, flow)?;
                self.expr(new, flow)?;
            }
            ChanSend { buf, ctrl, value } => {
                self.expr(buf, flow)?;
                self.expr(ctrl, flow)?;
                self.expr(value, flow)?;
            }
            ChanRecv { buf, ctrl, .. } => {
                self.expr(buf, flow)?;
                self.expr(ctrl, flow)?;
            }
            CallClosure { callee, args } => {
                self.expr(callee, flow)?;
                self.values(args, flow)?;
            }
            Call(_, args)
            | StrJoin { args, .. }
            | HostCall { args, .. }
            | RuntimeCall { args, .. } => self.values(args, flow)?,
            MakeClosure { captures, .. } => self.values(captures, flow)?,
            New { fields, .. }
            | MakeValue { fields, .. }
            | MakeVariant { fields, .. }
            | MakeValueVariant { fields, .. } => self.values(fields, flow)?,
            Panic(value) => {
                self.expr(value, flow)?;
                *flow = None;
            }
            ConstInt(..) | ConstFloat(..) | ConstBool(..) | ConstChar(..) | ConstZero(..)
            | ConstStr(..) | Unit | ThreadYield | ThreadCurrentId | CallbackPtr(..) => {}
        }
        Ok(())
    }
}
