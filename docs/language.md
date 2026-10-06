# Implemented language surface

This is an implementation overview, not a complete grammar or frozen language
specification. `src/lexer.rs`, `src/parser.rs`, and `src/lower.rs` define what the
compiler accepts and implements. Parser acceptance alone does not imply lowering
or execution support.

The checked examples and tests cover explicitly sized signed/unsigned integers,
f32/f64, bool, char, strings, structs, enums, tuples, arrays, inline value types,
generic functions/types, methods, traits, closures, conditionals, loops, match,
and Option/Result with `?`. Generics specialize to concrete representations.
Ordinary structs/enums allocate on the GC heap; value aggregates store inline.

The injected prelude in `src/prelude.gcr` supplies Option/Result, Display/Eq/Ord/
Hash-related APIs, Vec, HashMap/MapStr, string helpers, functional collection
helpers, and concurrency types. Read declarations there for exact signatures:
for example, `vec_get` returns Option<T>, while `vec_get_unchecked` returns T.
The legacy `array_get_unchecked`/`vec_get_unchecked` names now retain a backing
array bounds check and abort with a diagnostic on invalid indices. Array lengths
and allocation-size arithmetic are validated before allocation. This protects
against malformed or racing collection metadata; it does not make a sequence
of collection operations atomic.
Scalar array slots initially contain zero/false. Reference and inline-value
array slots have no generic default value: initialize each slot with `array_set`
before reading it. An in-bounds read of an unwritten slot aborts with an
uninitialized-element diagnostic, including `array_get` and native string join.

## Match limits

Tests cover enum variants, literal/scalar matches, bindings, guards, and
exhaustiveness diagnostics. Lowering explicitly rejects nested patterns, tuple
patterns, and struct patterns in the relevant paths. Do not describe pattern
matching as complete or infer runtime support from the AST's pattern variants.

## Contracts needing work

Read [mutability](mutability.md), [arithmetic](overflow.md), [modules](modules.md),
[threads](threads.md), and [FFI](ffi.md) for inspected behavior. Low-level runtime
root ownership and raw-pointer FFI operations mean a GC alone does not provide a blanket
memory-safety guarantee. Exceptional arithmetic and runtime resource failures
need defined contracts and additional tests.

Use [tour.md](tour.md) for existing examples and [STATUS.md](STATUS.md) for
observed test evidence. Unsupported capabilities should produce diagnostics;
production compiler robustness still needs validation beyond parser fuzzing.
