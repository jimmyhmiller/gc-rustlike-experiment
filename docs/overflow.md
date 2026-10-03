# Arithmetic behavior

Integer addition, subtraction, and multiplication wrap at the operand width.
The prelude supplies `checked_add_i64` and `checked_mul_i64`, returning Option.
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

Floating operations use LLVM float operations. This assessment does not establish
an exhaustive contract for NaNs, exceptional casts, or platform reproducibility.
Size/offset overflow still requires a broader audit. See PRODUCTION.md.
