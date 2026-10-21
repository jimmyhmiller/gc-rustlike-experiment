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
`Index::index` (`trait Index<Idx> { type Output; .. }`, as Rust's) and
`x[i] = v` calls `IndexSet::index_set`. Each requires an impl
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

Transparent type aliases name an existing type: `type Counts = Array<i64>;`
and `type Rows<T> = Vec<T>;`. They preserve the target's identity, representation,
constructors, methods and pattern forms. Alias definitions resolve names in their
own module; use aliases do not change that scope. Recursive alias cycles and
incorrect generic arity are source errors.

Alias type parameters can have trait bounds in their parameter list. Every
explicit use and inferred construction must satisfy those bounds;
a generic caller must supply sufficient bounds of its own. Trait names resolve
lexically: a nearer type declaration or generic binder of the same name cannot
be skipped to select an outer trait. Qualify the trait path to select it.

Each `impl Trait for T` is checked against the trait: every required method
must be present, no method may be outside the trait, and each method's
receiver (`self` or `mut self`), parameters, and return type must match the
declaration with `Self` and the trait's type parameters substituted. A trait's
default methods apply to every implementing type that does not override them.

Two methods with the same name on the same type are an error unless they
implement different traits; an inherent method takes precedence over a
trait's. An unknown attribute is an error. A user item that shadows a prelude
type or trait also drops the prelude's impls for it.

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
  braces. Rust's format specs follow a `:`: fill and alignment (`{:>8}`,
  `{:*^7}`), `+`, `#`, `0`, width, precision (`{:.2}`), and the types `?`,
  `x`, `X`, `o`, `b`, `e`, `E`; widths and precisions may be `N$`, `name$`,
  or `.*`. `{}` renders with `to_string()` and `{:?}` (`{:#?}` pretty) with
  `Debug`; the output matches Rust's. Arguments are evaluated once, in order.
  An argument the string never uses is an error.
- `println!`, `print!`, `eprintln!`, and `panic!` take the same arguments and
  evaluate to `()`. `write!(f, ...)`/`writeln!` append to a `Formatter` (or
  anything with `write_str`). `todo!`, `unimplemented!`, and `unreachable!`
  panic.
- `vec![a, b, c]`, `vec![]`, and `vec![x; n]`. In `vec![x; n]`, `x` is
  evaluated once and every slot holds that same value (for a struct or a
  collection, the same reference).
- `assert!(cond)`, `assert_eq!(a, b)`, `assert_ne!(a, b)`, each with an
  optional trailing format message.

`x[lo..hi]` on a `String` or `Vec` (also `x[lo..]`, `x[..hi]`, `x[..]`,
`x[lo..=hi]`) returns a copy of that part through the `IndexRange` trait; with
no references there are no borrowed slices. Indices are bytes for a `String`,
and an out-of-range bound aborts.

Array literals `[a, b, c]` and `[x; n]` build a fixed-length `[T; N]`
(`n` must then be an integer literal), or an `Array<T>` where one is expected.
Both share the runtime array object: indexing, `len()`, `get`, and `for` work
on either. In `[x; n]`, every slot holds the same value.

A method called on a value whose type parameters are still open, such as
`None.unwrap_or(7)`, takes them from its arguments.

## Rust surface

These follow Rust; differences come from having no references.

- **Formatting.** `Display` and `Debug` take `fn fmt(self, f: Formatter) ->
  FmtResult` (`fmt::Formatter`/`fmt::Result` also work). A `Formatter` is a GC
  object, so `f` needs no `&mut`. A `Display` type gets `to_string()`, and
  `f.debug_struct(..)`, `debug_tuple`, `debug_list`, `debug_map` build Debug
  output. Debug is implemented for scalars, `String`, `Vec`, `Option`,
  `Result`, `HashMap`, tuples, and arrays.
- **Derives.** `#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord,
  Default, Copy)]` generate impls (`PartialEq`/`Eq` give the `Eq` trait,
  `PartialOrd`/`Ord` the `Ord` trait; `Copy` changes nothing). `#[allow]`,
  `#[inline]`, and similar hints are ignored; `#[test]` and `#[cfg(test)]`
  items are skipped.
- **Paths.** `use std::...;` imports nothing (the prelude is always in scope);
  `std::collections::HashMap` and `fmt::Display` name prelude items;
  `use a::{b, c};` groups work. `Box<T>`, `Rc<T>`, and `Arc<T>` are `T`, and
  `Box::new(x)` is `x`.
- **Entry point.** `fn main()` and `fn main() -> Result<(), E>` are accepted
  (an `Err` prints `Error: {e:?}` and exits 1), as is `fn main() -> i64`.
- **Trait-qualified calls.** `Trait::method(x, ..)` and `Type::method(x, ..)`
  call a method with an explicit receiver; two traits may give one type
  methods of the same name. `Trait::f()` without `self` (`Default::default()`)
  takes the impl from the expected type, and `T::f()` calls it on a type
  parameter.
- **Closures.** `F: Fn(A) -> R` (and `FnMut`, `FnOnce`, in `where` clauses
  too, and on struct and impl parameters: `struct Cache<F: Fn(i64) -> i64>`), `impl Fn(A) -> R`, and `dyn Fn(A) -> R` are the closure type
  `fn(A) -> R`. An argument's `impl Trait` is an anonymous type parameter.
  `move` is accepted. A closure that assigns a captured `let mut` variable
  shares it with the enclosing function, so `let mut n = 0; let mut inc = ||
  n += 1;` counts.
- **Trait objects.** `dyn Trait` (usually `Box<dyn Trait>`) holds any value
  implementing `Trait`; a value converts where a `dyn Trait` is expected.
  Methods that take `self` without their own type parameters or `Self` in
  their signature (other than `Self::Item`-style associated types) can be
  called on it, including supertrait methods. Associated types are given as
  bindings: `Box<dyn Iterator<Item = i64>>`. When all of a trait's required
  methods qualify, the object implements the trait, so default methods
  (`map`, `collect`, `to_string`) work on it too.
- **`impl Trait` results.** `fn evens() -> impl Iterator<Item = i64>` returns
  whatever concrete type the body produces; each instantiation is inferred
  from its body, and callers see that type. A function returning
  `impl Trait` cannot call itself.
- **Associated types.** `type Item;` in a trait, `type Item = T;` in an impl
  (an impl may leave it out when its method signatures determine it),
  `Self::Item`, `I::Item`, `<T as Trait>::Item`, and bounds such as
  `I: Iterator<Item = i64>`.
- **Iterators.** `Iterator` is lazy: `map`, `filter`, `filter_map`,
  `enumerate`, `zip`, `chain`, `take`, `skip`, `step_by`, `take_while`,
  `skip_while`, `flat_map`, `peekable`, and `rev` (on ranges and Vec
  iterators) build adapters; `count`, `sum`, `product`, `fold`, `reduce`,
  `any`, `all`, `find`, `position`, `max`, `min`, `max_by_key`, `last`,
  `nth`, `for_each`, and `partition` consume. `collect()` builds a `Vec`,
  `String`, or `HashMap` from the expected type (`collect::<Vec<_>>()` works),
  defaulting to a `Vec`. `for` loops drive any iterator, `lo..hi` is a
  `Range` iterator when used as a value, and `v.iter()` iterates a Vec. Vec's
  own `map`, `filter`, and `fold` remain eager and return Vecs; `chars()`
  returns a `Vec<char>`.
- **Control flow.** `break value` out of a `loop`; labeled loops
  (`'outer: for ..` with `break 'outer [value]` and `continue 'outer`);
  `let PAT = e else { .. };`; `matches!(e, PAT [if guard])`; `let x;`
  assigned later (its type comes from the first assignment, and the binding
  may be reassigned); `for c in 'a'..='z'`; let chains
  (`if let Some(x) = a && x > 0 && let Some(y) = b { .. }`, also in `while`).
- **Items and paths.** `const` and `static` items (a `static` is a constant);
  nested `fn`s and other items inside functions (hoisted beside the enclosing
  item; like Rust's, they capture nothing); `pub(crate)`/`pub(super)` (all
  `pub`); `crate::`, `super::`, and `self::` paths; lifetime parameters,
  `T: 'a`, `Sized`, and `?Sized` are accepted and mean nothing.
- **Types.** `usize` and `isize` are `i64`. Tuple structs (`struct M(f64)`,
  `M(1.0)`, `m.0`, `let M(x) = m`), unit structs (`struct U;`, `U`), struct
  update (`P { x: 1, ..p }`), `Self(..)`/`Self::Variant`/`Self { .. }` in
  impls, and enum discriminants (`enum E { A = 1, B }`, `E::B as i64`).
  Tuples compare (`==`, `<`), sort, hash (as map keys), and clone element by
  element.
- **Operator traits.** Shaped like Rust's: `Add<Rhs = Self>` with
  `type Output = Self` (and `Sub`, `Mul`, `Div`, `Rem`, `BitAnd`, `BitOr`,
  `BitXor`), `Neg`, `Not`, and `AddAssign`-style traits (`a += b` uses
  `add_assign` when implemented, else `a = a + b`). `PartialEq` is `Eq` (an
  empty `impl Eq for T {}` marker is accepted); `PartialOrd` provides
  `partial_cmp`, `Ord` types are `PartialOrd`, and `<` on a type that is
  only `PartialOrd` uses `partial_cmp`. The numeric primitives implement the
  arithmetic traits, so `T: Add<Output = T>` accepts `i64`. `Copy` and `Send`
  bounds always hold.
- **Generic defaults and several impls of one trait.** Trait parameters may
  have defaults (`trait Add<Rhs = Self>`) and associated types defaults
  (`type Output = Self;`). A type may implement one generic trait several
  times (`impl Mul<f64> for V` and `impl Mul<V> for V`, `From<i64>` and
  `From<bool>`); calls choose the impl by argument type.
- **Blanket impls and conversions.** `impl<T: Display> Shout for T` applies
  to every type meeting the bounds (a type's own impls take precedence).
  `From<T>` converts (`T::from(x)`, `From::from(x)`, and every type converts
  into itself); `x.into()` converts into the expected type, and `T: Into<U>`
  holds when `U: From<T>`. `?` converts an error to the function's error type
  through `From`.
- **Impl receivers and trait objects.** Traits may be implemented for
  tuples (`impl Area for (i64, i64)`), arrays (`impl<const N: usize> T for
  [i64; N]`), and closure types (`impl<F: Fn(i64) -> i64> Op for F`).
  `Eq<Rhs = Self>` and `PartialOrd<Rhs = Self>` take a right-hand type
  (`impl PartialEq<i64> for M`). `<T as Trait>::f(..)` and `<T>::f(..)` call
  items of a type. `Box<dyn Derived>` converts to `Box<dyn Base>` for a
  supertrait `Base`. `IntoIterator` (for Vecs, HashMaps, and iterators) bounds
  and `for` loops accept any iterable.
- **Projections.** `T::Output` resolves through the bound on `T` that
  declares `Output` (`T: Mul<f64>`), and `<T as Mul<V>>::Output` names the
  trait instance; a type's own impls take precedence over blanket impls.
- **Associated constants.** `const MAX: i64 = 10;` in impls and traits (with
  or without a default), used as `Type::MAX`, `Self::MAX`, or `T::MAX`.
- **Const generics.** `fn f<const N: usize>(a: [i64; N])`,
  `struct Matrix<const R: usize, const C: usize>`, `Matrix<2, 3>`,
  `first_n::<2>(..)`; `N` is usable as a value and in `[x; N]`. Array
  lengths may also name an integer `const`. Arrays compare with `==`.
- **Turbofish.** `f::<i64>()`, `Vec::<i64>::new()`, and `it.sum::<i64>()`
  give type arguments explicitly (`_` leaves one inferred).
- **Primitive constants and char methods.** `i64::MAX`, `u8::MIN`,
  `i32::BITS`, `f64::INFINITY`, `f64::NAN`, `f64::EPSILON`,
  `f64::consts::PI`, and so on; `char` has `is_ascii_digit`, `is_alphabetic`,
  `is_whitespace`, `to_digit`, `to_ascii_uppercase`, and the other ASCII
  classifiers (non-ASCII classification is approximate).
- **Macros.** `macro_rules!` with `expr`, `ident`, `ty`, `tt`, `literal`,
  `pat`, `path`, `block`, `stmt`, and `item` fragments and `$(..),*`, `+`, `?`
  repetitions, used as expressions, statements, or items. A macro must be
  defined before its use. Expansion is not hygienic.

## Callable expressions and argument inference

Any expression with a function type can be called. This includes immediate
closures, returned closures, parenthesized function-valued fields, array elements,
and the result of a block or conditional:

```rust
fn make(offset: i64) -> fn(i64) -> i64 { |n: i64| offset + n }
fn main() -> i64 {
    let incremented = (|n| n + 1)(39);
    make(2)(incremented)
}
```

Immediate closure parameters can take their types from the call's arguments;
annotations remain available when an argument is itself unconstrained. Calls
check arity, argument types and return types. A local binding shadows a global
function or variant name; calling a non-function local is a source error.

The callee is evaluated once, then arguments once each from left to right.
The callee's captured references stay rooted if evaluating an argument collects.

A later concrete argument can constrain an earlier empty array, generic
constructor or record literal. Ordinary successful lowering runs first;
backward argument discovery retries only after discarding a failed attempt.
Generic bounds and nominal identity still apply. Speculative discovery does not
execute source effects or retain discarded compiler IR.

## Patterns

`match`, `if let`, `while let`, and destructuring `let` accept wildcards,
bindings, literals, enum variants (positional or `Variant { field, .. }`),
tuples, and structs (`Point { x, y }`, `..` for the rest), nested to any
depth. A pattern's sub-patterns are tested left to right, and a payload is read
only after its variant tag has matched. `let` requires an irrefutable pattern.

Arms may also use or-patterns (`1 | 2 => ..`, nested as in `(0 | 1, Some(_))`),
range patterns over integers and chars (`1..=9`, `'a'..='z'`, `10..100`,
`i64::MIN..=-1`), negative literals, and `name @ 1..=9`; these also work in
`if let` and `while let`, not in `let`. An or-pattern repeats its arm for each
alternative, and a range is a guard, so a range does not count toward
exhaustiveness.

Slice patterns match a `Vec` or array: `[]`, `[x]`, `[first, .., last]`,
`[x, rest @ ..]` (`rest` is a copy of the remaining elements), with names,
`_`, literals, ranges, and destructuring patterns as elements. A slice
pattern is a length guard, so a match over slices needs a `_` arm unless its
unguarded arms cover every length (`[] / [x] / [x, rest @ ..]`).
`v.as_slice()` is `v`.

A `match` must be exhaustive. The check covers nested enum variants, `bool`,
tuples, and structs; integers and strings need a catch-all arm. Guarded arms
do not count toward coverage.

## Contracts needing work

Foreign calls, raw native primitives and RawPtr record representation operations
require lexical `unsafe { ... }` blocks. Typed library APIs contain their own
unsafe implementations. Deferred bindings require assignment on every reachable
path before use; their GC storage's zero fill is not an initialized value.
String ranges require UTF-8 character boundaries. See [SAFETY.md](SAFETY.md).

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

Program execution starts at a root-level, non-generic `fn main() -> i64` with no
parameters. The runner supplies process arguments through `process_args()`.
Named record initializers evaluate in their written order, including shorthand
fields, and construction preserves those values across later allocation and
mutation. The compiler rejects duplicate, unknown and missing fields. Field
storage follows the declaration independently of initializer evaluation order.

Inline value types require finite storage. Direct/indirect inline cycles and
expanding inline generic cycles are diagnosed. Nested wrappers such as
`Wrap<Wrap<i64>>` remain valid. Recursion through heap references or arrays is
supported, including inline array elements containing array references.
Unqualified type names require a unique declaration or an explicit import;
qualified names must refer to the written module/declaration path.

Unit fields and parameters have no payload storage. Their expressions still
evaluate for side effects. Unit arrays retain their logical length and perform
all normal bounds checks; an in-range optional read yields Some(()). Reflection
omits unit fields, reads scalar/reference fields with SC ordering and rejects
inline aggregates passed to field_i64 (field_kind 5).

All assignment expressions yield unit, including local/field/index assignments,
compound assignments and the array_set primitive. They do not return a dummy
integer. An assignment can therefore be the tail of a unit-returning function.
Compound field and indexed assignments evaluate their receiver and index once,
then read the old value before evaluating the right-hand side. The final store
uses the captured location even if the right-hand side changes the original
access path. Custom indexers call Index::index before the right-hand side and
IndexSet::index_set afterward.
