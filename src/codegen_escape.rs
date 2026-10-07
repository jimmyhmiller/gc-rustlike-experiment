//! Whole-program, flow-insensitive escape analysis for scalar arrays.
//!
//! Allocation sites flow through local aliases, direct-call arguments and
//! returns. Heap storage, closures, native/unknown calls and unsupported uses
//! escape their reference operands. An unknown origin prevents specialization.
//! This intentionally does not attempt heap-field or indirect-call analysis.
use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::core::{CoreBlock, CoreExpr, CoreExprKind, CoreProgram, CoreStmt, Repr};

type Node = usize;

#[derive(Default)]
struct Points {
    allocations: BTreeSet<usize>,
    unknown: bool,
    nullable: bool,
    integers: BTreeSet<u64>,
}

#[derive(Default)]
pub(crate) struct PrivateArrays {
    expressions: BTreeSet<usize>,
    lengths: HashMap<usize, u64>,
    integers: HashMap<usize, u64>,
    origins: HashMap<usize, BTreeSet<usize>>,
}

impl PrivateArrays {
    pub(crate) fn analyze(program: &CoreProgram) -> Self {
        Analysis::new(program).finish()
    }

    pub(crate) fn contains(&self, expression: &CoreExpr) -> bool {
        self.expressions
            .contains(&(expression as *const CoreExpr as usize))
    }

    pub(crate) fn length(&self, expression: &CoreExpr) -> Option<u64> {
        self.lengths
            .get(&(expression as *const CoreExpr as usize))
            .copied()
    }

    pub(crate) fn integer(&self, expression: &CoreExpr) -> Option<u64> {
        self.integers
            .get(&(expression as *const CoreExpr as usize))
            .copied()
    }

    pub(crate) fn origins(&self, expression: &CoreExpr) -> Option<&BTreeSet<usize>> {
        self.origins.get(&(expression as *const CoreExpr as usize))
    }

    pub(crate) fn origin_sets(&self) -> BTreeSet<BTreeSet<usize>> {
        self.origins.values().cloned().collect()
    }
}

struct Analysis<'p> {
    program: &'p CoreProgram,
    points: Vec<Points>,
    edges: Vec<Vec<Node>>,
    locals: Vec<Vec<Node>>,
    returns: Vec<Node>,
    expressions: HashMap<usize, Node>,
    escaping: BTreeSet<Node>,
    allocation_lengths: Vec<Option<Node>>,
    breaks: Vec<Option<Node>>,
}

impl<'p> Analysis<'p> {
    fn new(program: &'p CoreProgram) -> Self {
        let mut a = Self {
            program,
            points: Vec::new(),
            edges: Vec::new(),
            locals: Vec::new(),
            returns: Vec::new(),
            expressions: HashMap::new(),
            escaping: BTreeSet::new(),
            allocation_lengths: Vec::new(),
            breaks: Vec::new(),
        };
        for function in &program.funcs {
            let locals = function.locals.iter().map(|_| a.node()).collect();
            a.locals.push(locals);
            let ret = a.node();
            a.returns.push(ret);
        }
        // Execution entry arguments and lifted closure captures have origins
        // outside the direct-call graph. Address-taken parameters are handled
        // when their MakeClosure/CallbackPtr expressions are encountered.
        if let Some(entry) = program.entry {
            a.external_entry(entry as usize);
        }
        for (id, function) in program.funcs.iter().enumerate() {
            for capture in &function.closure_captures {
                a.points[a.locals[id][capture.local as usize]].unknown = true;
            }
        }
        a
    }

    fn node(&mut self) -> Node {
        let id = self.points.len();
        self.points.push(Points::default());
        self.edges.push(Vec::new());
        id
    }

    fn flow(&mut self, from: Option<Node>, to: Option<Node>) {
        if let (Some(from), Some(to)) = (from, to) {
            self.edges[from].push(to);
        }
    }

    fn escape(&mut self, node: Option<Node>) {
        if let Some(node) = node {
            self.escaping.insert(node);
        }
    }

    fn external_entry(&mut self, id: usize) {
        let function = &self.program.funcs[id];
        let start = function.closure_captures.len();
        for (index, repr) in function.params.iter().enumerate() {
            if matches!(repr, Repr::Ref(_) | Repr::Scalar(_)) {
                self.points[self.locals[id][start + index]].unknown = true;
            }
        }
        // An address-taken function's returned reference can leave the known
        // caller graph as well, including functions used as native callbacks.
        self.escaping.insert(self.returns[id]);
    }

    fn block(&mut self, id: usize, block: &CoreBlock) -> Option<Node> {
        for statement in &block.stmts {
            match statement {
                CoreStmt::Let(local, expression) => {
                    let value = self.expression(id, expression);
                    self.flow(value, Some(self.locals[id][*local as usize]));
                }
                CoreStmt::Expr(expression) => {
                    self.expression(id, expression);
                }
            }
        }
        block
            .tail
            .as_ref()
            .and_then(|expression| self.expression(id, expression))
    }

    fn expression(&mut self, id: usize, e: &CoreExpr) -> Option<Node> {
        use CoreExprKind::*;
        let result =
            if matches!(e.repr, Repr::Ref(_)) || matches!(e.repr, Repr::Scalar(s) if s.is_int()) {
                let node = self.node();
                self.expressions.insert(e as *const CoreExpr as usize, node);
                Some(node)
            } else {
                None
            };
        match e.kind.as_ref() {
            Local(local) => self.flow(Some(self.locals[id][*local as usize]), result),
            ConstZero(repr) => {
                if let Some(result) = result {
                    if matches!(repr, Repr::Ref(_)) {
                        self.points[result].nullable = true;
                    } else {
                        self.points[result].integers.insert(0);
                    }
                }
            }
            ConstInt(value, scalar) => {
                if let Some(result) = result {
                    let bits = scalar.bits();
                    let value = if bits < 64 && scalar.is_signed() {
                        (((*value << (64 - bits)) as i64) >> (64 - bits)) as u64
                    } else if bits < 64 {
                        *value & ((1_u64 << bits) - 1)
                    } else {
                        *value
                    };
                    self.points[result].integers.insert(value);
                }
            }
            ArrayNew { len, elem, .. } => {
                let length = self.expression(id, len);
                if let Some(result) = result {
                    if matches!(elem, Repr::Scalar(_)) {
                        self.points[result]
                            .allocations
                            .insert(self.allocation_lengths.len());
                        self.allocation_lengths.push(length);
                    } else {
                        self.points[result].unknown = true;
                    }
                }
            }
            Assign { local, value } => {
                let value = self.expression(id, value);
                self.flow(value, Some(self.locals[id][*local as usize]));
                self.flow(value, result);
            }
            Call(callee, args) => {
                let callee = *callee as usize;
                let function = &self.program.funcs[callee];
                let external = function.is_extern;
                let start = function.closure_captures.len();
                for (index, arg) in args.iter().enumerate() {
                    let value = self.expression(id, arg);
                    if external {
                        self.escape(value);
                    } else {
                        self.flow(value, Some(self.locals[callee][start + index]));
                    }
                }
                if external {
                    if let Some(result) = result {
                        self.points[result].unknown = true;
                    }
                } else {
                    self.flow(Some(self.returns[callee]), result);
                }
            }
            If(condition, yes, no) => {
                self.expression(id, condition);
                let yes = self.block(id, yes);
                self.flow(yes, result);
                let no = self.block(id, no);
                self.flow(no, result);
            }
            Block(block) => {
                let value = self.block(id, block);
                self.flow(value, result);
            }
            Loop(block) => {
                self.breaks.push(result);
                self.block(id, block);
                self.breaks.pop();
            }
            Break(value) => {
                let value = value.as_ref().and_then(|value| self.expression(id, value));
                self.flow(value, self.breaks.last().copied().flatten());
            }
            Return(value) => {
                let value = value.as_ref().and_then(|value| self.expression(id, value));
                self.flow(value, Some(self.returns[id]));
            }
            ArrayLen(array) => {
                self.expression(id, array);
                self.unknown(result);
            }
            ArrayGet { array, index, .. }
            | ArrayGetUnchecked { array, index, .. }
            | ArrayGetChecked { array, index, .. } => {
                self.expression(id, array);
                self.expression(id, index);
                if let Some(result) = result {
                    self.points[result].unknown = true;
                }
            }
            ArraySet {
                array,
                index,
                value,
                ..
            } => {
                self.expression(id, array);
                self.expression(id, index);
                let value = self.expression(id, value);
                self.escape(value);
                if let Some(result) = result {
                    self.points[result].integers.insert(0);
                }
            }
            Bin(_, a, b) => {
                self.expression(id, a);
                self.expression(id, b);
                self.unknown(result);
            }
            Un(_, value) | FloatIntrinsic(_, value) | FloatBits(value) | Print(value) => {
                let value = self.expression(id, value);
                self.escape(value);
                self.unknown(result);
            }
            Match { scrutinee, arms } | ValueMatch { scrutinee, arms } => {
                let value = self.expression(id, scrutinee);
                self.escape(value);
                for arm in arms {
                    for &local in &arm.binds {
                        self.points[self.locals[id][local as usize]].unknown = true;
                    }
                    let value = self.expression(id, &arm.body);
                    self.flow(value, result);
                }
            }
            MakeClosure { code, captures, .. } => {
                self.external_entry(*code as usize);
                for capture in captures {
                    let value = self.expression(id, capture);
                    self.escape(value);
                }
                if let Some(result) = result {
                    self.points[result].unknown = true;
                }
            }
            CallbackPtr(code) => self.external_entry(*code as usize),
            ConstFloat(..) | ConstBool(..) | ConstChar(..) | Unit | Continue | ThreadYield
            | ThreadCurrentId => self.unknown(result),
            // Everything else is conservative. Keep the match exhaustive so a
            // future IR operation must explicitly state its escape behavior.
            ConstStr(_) => self.unknown(result),
            PrintStr(v) | PrintStrRaw(v) | StrLen(v) | StrToFloat(v) | StrHash(v) | TypeIdOf(v)
            | PtrReadI64(v) | Panic(v) | ThreadSpawn(v) | ThreadJoin(v) | ThreadSleep(v)
            | EnumTag(v) => self.unknown_use(id, result, [v.as_ref()]),
            StrEq(a, b) | StrGet(a, b) => self.unknown_use(id, result, [a.as_ref(), b.as_ref()]),
            StrConcat { a, b, .. } => self.unknown_use(id, result, [a.as_ref(), b.as_ref()]),
            StrSubstring { s, start, end, .. } => {
                self.unknown_use(id, result, [s.as_ref(), start.as_ref(), end.as_ref()])
            }
            StrFromNum { v, .. } => self.unknown_use(id, result, [v.as_ref()]),
            StrFromChar { cp, .. } => self.unknown_use(id, result, [cp.as_ref()]),
            ReadFile { path, .. } => self.unknown_use(id, result, [path.as_ref()]),
            TypeNameOf { obj, .. } => self.unknown_use(id, result, [obj.as_ref()]),
            AsCBytes { src, .. } => self.unknown_use(id, result, [src.as_ref()]),
            Cast { value, .. } => self.unknown_use(id, result, [value.as_ref()]),
            AtomLoad { atom, .. } => self.unknown_use(id, result, [atom.as_ref()]),
            AtomCas { atom, old, new } => {
                self.unknown_use(id, result, [atom.as_ref(), old.as_ref(), new.as_ref()])
            }
            ChanSend { buf, ctrl, value } => {
                self.unknown_use(id, result, [buf.as_ref(), ctrl.as_ref(), value.as_ref()])
            }
            ChanRecv { buf, ctrl, .. } => {
                self.unknown_use(id, result, [buf.as_ref(), ctrl.as_ref()])
            }
            Field { base, .. } => self.unknown_use(id, result, [base.as_ref()]),
            SetField { base, value, .. } => {
                self.unknown_use(id, result, [base.as_ref(), value.as_ref()])
            }
            EnumPayload { scrutinee, .. } => self.unknown_use(id, result, [scrutinee.as_ref()]),
            HostCall { args, .. } | RuntimeCall { args, .. } | StrJoin { args, .. } => {
                self.unknown_use(id, result, args.iter())
            }
            New { fields, .. }
            | MakeValue { fields, .. }
            | MakeVariant { fields, .. }
            | MakeValueVariant { fields, .. } => self.unknown_use(id, result, fields.iter()),
            CallClosure { callee, args } => self.unknown_use(
                id,
                result,
                std::iter::once(callee.as_ref()).chain(args.iter()),
            ),
        }
        result
    }

    fn unknown(&mut self, result: Option<Node>) {
        if let Some(result) = result {
            self.points[result].unknown = true;
        }
    }

    fn unknown_use<'a>(
        &mut self,
        id: usize,
        result: Option<Node>,
        operands: impl IntoIterator<Item = &'a CoreExpr>,
    ) {
        for operand in operands {
            let value = self.expression(id, operand);
            self.escape(value);
        }
        self.unknown(result);
    }

    fn finish(mut self) -> PrivateArrays {
        for (id, function) in self.program.funcs.iter().enumerate() {
            if !function.is_extern {
                let value = self.block(id, &function.body);
                self.flow(value, Some(self.returns[id]));
            }
        }
        let mut queued = vec![true; self.points.len()];
        let mut queue: VecDeque<_> = (0..self.points.len()).collect();
        while let Some(source) = queue.pop_front() {
            queued[source] = false;
            for &destination in &self.edges[source] {
                let mut changed = false;
                if self.points[source].unknown && !self.points[destination].unknown {
                    self.points[destination].unknown = true;
                    changed = true;
                }
                if self.points[source].nullable && !self.points[destination].nullable {
                    self.points[destination].nullable = true;
                    changed = true;
                }
                let integers: Vec<_> = self.points[source].integers.iter().copied().collect();
                for integer in integers {
                    changed |= self.points[destination].integers.insert(integer);
                }
                let values: Vec<_> = self.points[source].allocations.iter().copied().collect();
                for allocation in values {
                    changed |= self.points[destination].allocations.insert(allocation);
                }
                if changed && !queued[destination] {
                    queued[destination] = true;
                    queue.push_back(destination);
                }
            }
        }
        let mut escaped = BTreeSet::new();
        for &node in &self.escaping {
            escaped.extend(self.points[node].allocations.iter().copied());
        }
        let known_integer = |node: Node| {
            let points = &self.points[node];
            (!points.unknown && points.integers.len() == 1)
                .then(|| *points.integers.first().unwrap())
        };
        let mut lengths = HashMap::new();
        let mut integers = HashMap::new();
        for (&expression, &node) in &self.expressions {
            let points = &self.points[node];
            if let Some(integer) = known_integer(node) {
                integers.insert(expression, integer);
            }
            if points.unknown || points.nullable || points.allocations.is_empty() {
                continue;
            }
            let mut common = None;
            let mut compatible = true;
            for &allocation in &points.allocations {
                let length = self.allocation_lengths[allocation].and_then(known_integer);
                match length {
                    Some(length)
                        if length <= i64::MAX as u64 && common.is_none_or(|old| old == length) =>
                    {
                        common = Some(length)
                    }
                    _ => {
                        compatible = false;
                        break;
                    }
                }
            }
            if compatible {
                if let Some(length) = common {
                    lengths.insert(expression, length);
                }
            }
        }
        let origins = self
            .expressions
            .iter()
            .filter_map(|(&expression, &node)| {
                let points = &self.points[node];
                (!points.unknown
                    && !points.allocations.is_empty()
                    && points.allocations.is_disjoint(&escaped))
                .then(|| (expression, points.allocations.clone()))
            })
            .collect();
        let expressions = self
            .expressions
            .into_iter()
            .filter_map(|(expression, node)| {
                let points = &self.points[node];
                (!points.unknown
                    && !points.allocations.is_empty()
                    && points.allocations.is_disjoint(&escaped))
                .then_some(expression)
            })
            .collect();
        PrivateArrays {
            expressions,
            lengths,
            integers,
            origins,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::codegen::{codegen, codegen_with_debug, DebugLevel};
    use crate::compile::parse_with_prelude;
    use crate::lower::lower_program;
    use crate::resolve::resolve_module;
    use inkwell::values::AnyValue;

    fn program(source: &str) -> crate::core::CoreProgram {
        let (module, _) = parse_with_prelude(source).unwrap();
        lower_program(&resolve_module(module).unwrap().globals).unwrap()
    }

    fn function_ir(source: &str, name: &str) -> String {
        let program = program(source);
        let context = inkwell::context::Context::create();
        codegen(&context, &program)
            .unwrap()
            .module
            .get_function(name)
            .unwrap()
            .print_to_string()
            .to_string()
    }

    #[test]
    fn private_aliases_calls_and_returns_use_plain_accesses_and_constant_lengths() {
        let source = "
            fn identity(a: Array<i64>) -> Array<i64> { a }
            fn use_array(mut a: Array<i64>) -> i64 { let mut b = identity(a); array_set(b, 2, 17); array_len(b)+b[2] }
            fn main() -> i64 { let n = 3; let mut a: Array<i64> = array_new(n); use_array(a) }
        ";
        let ir = function_ir(source, "use_array");
        assert!(!ir.contains("store atomic i64"), "{ir}");
        assert!(!ir.contains("load atomic i64"), "{ir}");
        assert!(!ir.contains("cnt"), "{ir}");
        assert_eq!(
            crate::codegen::jit_run_i64_gc(&program(source), true).unwrap(),
            20
        );
    }

    #[test]
    fn captured_and_heap_stored_arrays_keep_shared_accesses() {
        for source in [
            "fn use_array(mut a: Array<i64>) -> i64 { array_set(a,0,4); a[0] } fn main() -> i64 { let mut a: Array<i64> = array_new(2); let closure = || use_array(a); closure() }",
            "struct Holder { a: Array<i64> } fn use_array(mut a: Array<i64>) -> i64 { array_set(a,0,4); a[0] } fn main() -> i64 { let mut a: Array<i64> = array_new(2); let mut h = Holder { a: a }; use_array(h.a) }",
        ] {
            let ir = function_ir(source, "use_array");
            assert!(ir.contains("store atomic i64"), "{ir}");
            assert!(ir.contains("load atomic i64"), "{ir}");
            assert_eq!(crate::codegen::jit_run_i64_gc(&program(source), true).unwrap(), 4);
        }
    }

    #[test]
    fn mixed_call_origins_and_nullable_aliases_do_not_invent_lengths() {
        let source = "
            fn use_array(mut a: Array<i64>) -> i64 { array_set(a,0,4); a[0]+array_len(a) }
            fn main() -> i64 { let mut a: Array<i64> = array_new(2); let mut b: Array<i64> = array_new(3); use_array(a)+use_array(b) }
        ";
        let ir = function_ir(source, "use_array");
        assert!(ir.contains("cnt"), "{ir}");
        assert_eq!(
            crate::codegen::jit_run_i64_gc(&program(source), true).unwrap(),
            13
        );
        let source = "fn use_array(mut a: Array<i64>) -> i64 { array_len(a) } fn main() -> i64 { let mut a: Array<i64>; a = array_new(2); use_array(a) }";
        let ir = function_ir(source, "use_array");
        assert!(ir.contains("cnt"), "{ir}");
    }

    #[test]
    fn assignments_from_unknown_integer_results_do_not_reuse_old_constants() {
        let source = "
            fn count(n: i64) -> i64 { let mut a: Array<i64> = array_new(n); let mut x = 7; x = array_len(a); x }
            fn main() -> i64 { count(3)+count(4) }
        ";
        assert_eq!(
            crate::codegen::jit_run_i64_gc(&program(source), true).unwrap(),
            7
        );
    }

    #[test]
    fn full_debug_preserves_editable_shared_reference_behavior() {
        let source =
            "fn main() -> i64 { let mut a: Array<i64> = array_new(3); array_set(a,1,4); a[1] }";
        let program = program(source);
        let context = inkwell::context::Context::create();
        let compiled = codegen_with_debug(&context, &program, DebugLevel::Full).unwrap();
        let ir = compiled.module.print_to_string().to_string();
        assert!(
            ir.contains("load atomic i64") && ir.contains("store atomic i64"),
            "{ir}"
        );
    }
}

#[cfg(test)]
mod alias_tests {
    #[test]
    fn overlapping_parameter_origins_never_gain_disjoint_alias_claims() {
        let source="
            fn write(mut a: Array<i64>, mut b: Array<i64>) -> i64 { array_set(a,0,5); array_set(b,0,7); a[0]+b[0] }
            fn main() -> i64 { let mut a: Array<i64> = array_new(1); let mut b: Array<i64> = array_new(1); write(a,a)+write(a,b) }
        ";
        let (module, _) = crate::compile::parse_with_prelude(source).unwrap();
        let program =
            crate::lower::lower_program(&crate::resolve::resolve_module(module).unwrap().globals)
                .unwrap();
        assert_eq!(crate::codegen::jit_run_i64_gc(&program, true).unwrap(), 26);
        let ir = crate::codegen::emit_llvm_ir(&program, true).unwrap();
        assert!(!ir.contains("!invariant.load"));
    }
}
