# Compiler pipeline and representations

Implementation references: `src/compile.rs`, `src/resolve.rs`, `src/lower.rs`,
`src/core.rs`, `src/layout.rs`, `src/anf.rs`, and `src/codegen.rs`.

The driver loads source files and injects `src/prelude.gcr`, resolves names,
checks/lowers types and generic instantiations, and sends monomorphic Core IR to
LLVM codegen. ANF normalization hoists GC-bearing operand temporaries into locals
so the code generator can root them across safepoints. Type checking and
monomorphization live primarily in `lower.rs`, not in a separate mono module.

`Repr` has four forms: Unit, Scalar, Value(ValueId), and Ref(LayoutId). Scalar
representations include width/signedness, float, bool, char, and a non-GC pointer.
Value layouts describe inline aggregates. Reference layouts describe heap objects:
leading traced pointers, raw bytes, variable-length storage, field locations,
and offsets of references embedded inside value fields.

Core expressions retain source spans; `CoreProgram.sources` maps source IDs to
file text. Codegen uses these for allocation locations and debugger information.
Closure lowering produces code plus an environment; codegen handles indirect
closure calls and traces the environment's reference captures.

For exact enums, fields, and serialized IR, read `src/core.rs` or run:

```sh
./target/debug/gcr emit core examples/types.gcr
./target/debug/gcr emit layout examples/types.gcr
./target/debug/gcr emit mono examples/types.gcr
./target/debug/gcr emit llvm examples/types.gcr --no-opt
```

The first three views emit JSON; `llvm` emits LLVM IR text. These are inspection
interfaces, not a promised stable compiler ABI. See STATUS.md for tested coverage.
