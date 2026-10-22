# Arithmetic behavior

Integer addition, subtraction, and multiplication wrap at the operand width
(8 through 128 bits).
The prelude supplies `i64::checked_add`, `checked_sub`, and `checked_mul`,
returning Option.
Checked multiplication rejects signed minimum times minus one in either order.

Integer division and remainder by zero print a diagnostic and abort, consistent
with the runtime's bounds-failure policy. Signed minimum divided by minus one
wraps to the minimum; its remainder is zero. Codegen substitutes a safe divisor
for this overflow case before emitting LLVM division/remainder instructions.

Shift counts are masked by operand width minus one. A shift by the width acts
like a shift by zero; negative counts use their low bits. Signed right shift
extends the sign; unsigned right shift fills with zero.

Regression tests exercise signed division overflow at 8/16/32/64 bits, signed and
unsigned shift counts at all four widths, zero-divisor diagnostics, checked
multiplication, and native AOT execution. JIT tests run optimized LLVM output.

Float-to-integer casts truncate toward zero and saturate to the destination range.
NaN converts to zero; infinities saturate; negative-to-unsigned converts to zero.
Both float widths and all eight integer types are covered by optimized JIT/native
tests and LLVM IR checks against poison-producing conversions.

Integer-to-char casts validate before narrowing: values outside 0..0x10FFFF and
surrogates 0xD800..0xDFFF abort with a diagnostic. Numeric-to-bool, float-to-char,
and casts involving RawPtr or aggregates are rejected during lowering.
See [SAFETY.md](SAFETY.md) for the cast matrix and remaining audit.

Floating operations use LLVM float operations. This assessment does not establish
an exhaustive contract for NaN payloads, rounding, or platform reproducibility.
NaN compares unequal (including to itself), ordered comparisons with NaN are
false, and signed zeros compare equal. Arithmetic NaN payloads are unspecified.
Integer literals must fit their selected width, including the signed minimum in
negated literals; explicit narrowing casts still wrap. Floating literals reject
overflow at their declared width. Bitwise/shift operators require integer types;
unary and compound operators cannot construct invalid char values.
Size/offset overflow still requires a broader audit. See PRODUCTION.md.
