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

The injected prelude in `src/prelude.gcr` supplies Option/Result/Ordering, the
Eq/Ord/Hash/ToString/Clone and operator traits, Vec, HashMap, String methods,
and concurrency types. Its API follows Rust's standard library and is used
through methods: `let mut v: Vec<i64> = Vec::new(); v.push(1); v[0]`,
`s.len()`, `m.insert(k, v)`, `o.unwrap_or(d)`. Read the declarations there for
exact signatures. `mut self` methods mutate the receiver in place, so every
alias of a Vec or HashMap observes a push or insert; `clone` makes an
independent copy. Lookups that may fail return Option rather than a sentinel:
`v.get(i)`, `s.find(needle)`, `v.position(pred)`.

Operators on non-scalar types dispatch to trait methods: `a + b` calls
`Add::add` (likewise Sub/Mul/Div/Rem), `a == b` calls `Eq::eq`, and `a < b`
calls `Ord::cmp`, which returns `Ordering`. `x[i]` on a non-array type calls
`Index::index` and `x[i] = v` calls `IndexSet::index_set`. Each requires an impl
of that trait; an inherent method of the same name does not qualify. Vec
indexing aborts with a diagnostic outside `0..len`.

Array and Vec indexing abort with a diagnostic on invalid indices. Array lengths
and allocation-size arithmetic are validated before allocation. This protects
against malformed or racing collection metadata; it does not make a sequence
of collection operations atomic. Ordinary Vec and HashMap require external
synchronization for shared mutation, including through backing-array aliases.
Safely published collections can be read concurrently while their storage is
unchanged. Concurrent collection types will be separate APIs; they are not yet
implemented. See [concurrency.md](concurrency.md) for the memory-safety contract.
Scalar array slots initially contain zero/false. Reference and inline-value
array slots have no generic default value: initialize each slot with `a[i] = v`
before reading it. An in-bounds read of an unwritten slot aborts with an
uninitialized-element diagnostic, including `Array::get` and native string join.

Two methods with the same name on the same type are an error, as is an
attribute other than `#[value]`. A user item that shadows a prelude type or
trait also drops the prelude's impls for it.

A `let` without a type annotation can take its type from a later use:
`let mut v = Vec::new(); v.push(1);` makes `v` a `Vec<i64>`. The first type
check that relates the binding to a known type decides it (an argument, an
assignment, a struct field, a return). If nothing does, it is an error asking
for an annotation.

`for x in lo..hi` (and `lo..=hi`) evaluates the bounds once and gives each
iteration a fresh `x`; `continue` is safe. `for x in items` visits a `Vec` or
`Array` (any type with `len()` and indexing) in index order. `if let PAT = e`
and `while let PAT = e` are `match` with a wildcard arm.

Built-in macros expand in the parser:

- `format!("...", args)` builds a `String`. `{}` takes the next argument, `{N}`
  argument N, and `{name}` a variable in scope; `{{` and `}}` are literal
  braces. Each value renders with `to_string()`. Arguments are evaluated once,
  in order. Format specs (`{:?}`, `{:>5}`) are not supported and are an error,
  as is an argument the string never uses.
- `println!`, `print!`, `eprintln!`, and `panic!` take the same arguments.
- `vec![a, b, c]`, `vec![]`, and `vec![x; n]`. In `vec![x; n]`, `x` is
  evaluated once and every slot holds that same value (for a struct or a
  collection, the same reference).
- `assert!(cond)`, `assert_eq!(a, b)`, `assert_ne!(a, b)`, each with an
  optional trailing format message.

Known gaps: a method called directly on `None` cannot infer `T` from its
arguments. There is no range indexing, no array literal syntax, and no
user-defined macros.

## Patterns

`match`, `if let`, `while let`, and destructuring `let` accept wildcards,
bindings, literals, enum variants (positional or `Variant { field, .. }`),
tuples, and structs (`Point { x, y }`, `..` for the rest), nested to any
depth. A pattern's sub-patterns are tested left to right, and a payload is read
only after its variant tag has matched. `let` requires an irrefutable pattern.

A `match` must be exhaustive. The check covers nested enum variants, `bool`,
tuples, and structs; integers and strings need a catch-all arm. Guarded arms
do not count toward coverage.

## Contracts needing work

Read [mutability](mutability.md), [arithmetic](overflow.md), [modules](modules.md),
[threads](threads.md), and [FFI](ffi.md) for inspected behavior. Low-level runtime
root ownership and raw-pointer FFI operations mean a GC alone does not provide a blanket
memory-safety guarantee. Exceptional arithmetic and runtime resource failures
need defined contracts and additional tests.

Use [tour.md](tour.md) for existing examples and [STATUS.md](STATUS.md) for
observed test evidence. Unsupported capabilities should produce diagnostics;
production compiler robustness still needs validation beyond parser fuzzing.


The embedded [CSV/heap APIs](csv-heap.md) support application data processing.
Match arms receive contextual return/binding types for generic enum inference,
and `mut` enum payload bindings retain their mutability in switch and guarded
matches. Both inline and reference enums support inline payload extraction;
nested managed references remain traced across moving GC. Inline payloads of
reference enums are currently omitted from runtime field reflection.

## Design decisions

**No references (`&T`, `&mut T`, `*x`).** This is permanent, not a gap. Values
are GC references or inline value types, passed by value; mutability goes
through `mut` bindings and `mut self` receivers. Don't add `&` syntax, and don't
list it as missing.
