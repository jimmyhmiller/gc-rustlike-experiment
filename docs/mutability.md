# Mutability

`src/lower.rs` records binding mutability and checks assignment access paths.
Bindings default to immutable; `let mut` permits reassignment. Field/element
writes and mutable receiver calls require a permitted mutable root binding.
Parameters and receivers can declare `mut`.

These checks regulate the path used to mutate. They do not establish exclusive
ownership or object immutability across aliases. Rebinding a parameter changes
the callee's binding; mutating shared heap data can affect the caller.

See `examples/mutability.gcr`, lowering's `check_mutable_root`, and tests in
`src/codegen.rs` for accepted/rejected cases. Value aggregates and closures need
operation-specific checking; do not infer an ownership guarantee from `mut`.
Thread capture checks impose additional restrictions described in threads.md.
