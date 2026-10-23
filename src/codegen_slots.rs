//! Stack slot sharing. Each traced local (a reference with a GC root mirror,
//! or an inline value holding references) costs a slot that is zeroed and
//! registered when the function is entered. Locals whose live ranges do not
//! overlap — the arms of one `match`, say — can share a slot, as a register
//! allocator would share a register.
//!
//! A local's range is the span, in a pre-order numbering of the function, from
//! its first occurrence (definition or use) to its last, widened to the end of
//! every loop it is live through (defined before the loop and used inside it).
//! Code is structured, so two locals live at one point always have
//! overlapping ranges; disjoint branches get disjoint ranges.
use crate::core::*;
use std::collections::HashMap;

/// For each local, the local whose slot it uses (itself when not shared).
/// `kind` gives each shareable local's slot kind; only locals of one kind
/// share. Locals below `incoming` (parameters, captures) never share.
pub(crate) fn share_slots(f: &CoreFn, incoming: usize, kind: impl Fn(LocalId) -> Option<u64>) -> Vec<LocalId> {
    let n = f.locals.len();
    let mut ranges = Ranges { pos: 0, lo: vec![u32::MAX; n], hi: vec![0; n], loops: Vec::new() };
    ranges.block(&f.body);
    // Widen to loop ends: a local defined before a loop and used in it must
    // survive every iteration.
    ranges.loops.sort_by_key(|&(start, end)| (start, std::cmp::Reverse(end)));
    for _ in 0..2 {
        for &(start, end) in ranges.loops.iter().rev() {
            for l in 0..n {
                if ranges.lo[l] < start && ranges.hi[l] >= start && ranges.hi[l] <= end {
                    ranges.hi[l] = end;
                }
            }
        }
    }
    let mut order: Vec<(u32, u32, LocalId, u64)> = (incoming..n)
        .filter(|&l| ranges.lo[l] != u32::MAX)
        .filter_map(|l| kind(l as LocalId).map(|k| (ranges.lo[l], ranges.hi[l], l as LocalId, k)))
        .collect();
    order.sort();
    let mut rep: Vec<LocalId> = (0..n as LocalId).collect();
    // Per kind: the slots in use, as (end of the current occupant, owner).
    let mut slots: HashMap<u64, Vec<(u32, LocalId)>> = HashMap::new();
    for (lo, hi, l, k) in order {
        let free = slots.entry(k).or_default();
        match free.iter_mut().find(|(end, _)| *end < lo) {
            Some(slot) => { rep[l as usize] = slot.1; slot.0 = hi; }
            None => free.push((hi, l)),
        }
    }
    rep
}

struct Ranges {
    pos: u32,
    lo: Vec<u32>,
    hi: Vec<u32>,
    loops: Vec<(u32, u32)>,
}

impl Ranges {
    fn touch(&mut self, l: LocalId) {
        let l = l as usize;
        self.lo[l] = self.lo[l].min(self.pos);
        self.hi[l] = self.hi[l].max(self.pos);
    }

    fn block(&mut self, b: &CoreBlock) {
        for s in &b.stmts {
            self.pos += 1;
            match s {
                CoreStmt::Let(l, e) => { self.touch(*l); self.expr(e); }
                CoreStmt::Expr(e) => self.expr(e),
            }
        }
        if let Some(t) = &b.tail { self.pos += 1; self.expr(t); }
        self.pos += 1;
    }

    fn expr(&mut self, e: &CoreExpr) {
        self.pos += 1;
        match e.kind.as_ref() {
            CoreExprKind::Local(l) => self.touch(*l),
            CoreExprKind::Assign { local, value } => { self.expr(value); self.pos += 1; self.touch(*local); }
            CoreExprKind::If(c, y, n) => { self.expr(c); self.block(y); self.block(n); }
            CoreExprKind::Block(b) | CoreExprKind::Once { body: b, .. } => self.block(b),
            CoreExprKind::Loop(b) => {
                let start = self.pos;
                self.block(b);
                self.loops.push((start, self.pos));
            }
            CoreExprKind::Match { scrutinee, arms } | CoreExprKind::ValueMatch { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms {
                    self.pos += 1;
                    for &b in &a.binds { self.touch(b); }
                    self.expr(&a.body);
                }
            }
            _ => for c in crate::copyprop::children(e) { self.expr(c) },
        }
        self.pos += 1;
    }
}
