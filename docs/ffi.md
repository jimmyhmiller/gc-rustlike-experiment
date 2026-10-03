# C interoperability

The parser accepts `extern "C"` declarations. Lowering checks their parameter
and return representations; managed reference types do not cross as ordinary
extern arguments. Codegen calls C symbols without the hidden managed Thread
parameter and emits native/managed transition calls.

Inspected/covered features:

- Scalar arguments and returns.
- Blittable value-struct arguments by value; mutable value-struct parameters by
  pointer to stack storage. Aggregate coercion uses AAPCS64 classification.
- Blittable aggregate returns up to 16 bytes; larger returns are rejected.
- `as_c_bytes` copies String/scalar-array contents to stack storage for a direct
  extern argument. Mutable buffer parameters support copy-back.
- Named callback functions use synthesized C-ABI trampolines and runtime reentry.

19 FFI integration tests and four focused buffer/native-transition regressions pass on the assessed macOS ARM64 host. This establishes
coverage for those cases, not portability to another C ABI or all callback uses.
The implementation rejects non-C ABI declarations. Native link configuration
comes from CLI link arguments and/or the manifest's `[link]` section.

Native calls publish roots and mark the mutator blocked. Returning from native
code or entering a managed callback waits for any collection before resuming.
Buffer copy-back reloads the destination from its root after native return.
Dynamic buffers are released after each call and copy-back, including inside
loops; their conversions preserve argument order. Tests cover allocating callbacks,
blocking native calls during another mutator's collections, and repeated buffers.

Review targets include native pointer lifetimes/retention, foreign-thread callbacks,
callback failure/unwinding, stack use for very large copies, and ABI classification on
supported targets. Existing `RawPtr` operations require an explicit safety
contract. See PRODUCTION.md. The raylib example has not been launched as part
of this assessment.
