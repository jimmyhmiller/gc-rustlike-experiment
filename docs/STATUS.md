# Verified state

Assessment and fixes: 2026-10-03. Repository HEAD: `64638a6`; fixes are local
working-tree changes. Host: Darwin ARM64; Rust/Cargo 1.96.0. Existing widget,
`.gitignore`, and benchmark-artifact changes were preserved.

The reproduced defects below have fixes and regression coverage. Production
readiness remains unproven; remaining gates are in [PRODUCTION.md](PRODUCTION.md).

## Tests observed after fixes

`cargo test --workspace`: 410 passed, zero failures, five ignored runtime doc
examples. Release verification: 41 tests passed with `GCR_GC_VERIFY=1`.

| Suite | Observed result |
| --- | --- |
| Compiler/frontend unit tests | 163 passed |
| Runtime unit tests | 109 passed; 5 runtime doc examples ignored |
| AOT executables | 4 passed |
| FFI | 19 passed |
| Operand rooting / stack / native transitions | 9 passed |
| Arithmetic boundaries / diagnostics | 5 passed |
| Project CLI / native stress selection | 3 passed |
| LLDB debugger / DWARF | 5 / 3 passed |
| Allocation profiling / benchmark command | 6 / 4 passed |
| Parser fuzz / GC ABI / generational | 5 / 1 / 4 passed |
| Heap command / diff / snapshot | 5 / 2 / 1 passed |
| Modules / reflection / stdlib integration | 7 / 14 / 9 passed |
| Comprehensive concurrency stress | Full default 50 iterations pass in debug and release |
| Shared-memory / threads / randomized thread stress | 15 / 13 / 3 passed |
| Example runner, bundled macOS Bash | 23 expected results pass in normal mode |
| Separate bounded stress example runs | 22 pass; full binary-tree stress run incomplete |

Validation commands:

```sh
cargo test --workspace
GCR_GC_VERIFY=1 cargo test --release --test concurrency_stress --test operand_roots --test arithmetic --test aot --test project_cli --test ffi
/bin/bash scripts/run_examples.sh
```

The release command uses the default 50 concurrency iterations, not a reduced
workload. Arithmetic tests execute optimized JIT output and check native AOT
behavior. Project tests observe zero major collections in normal execution and
at least ten with the stress flag, including a standalone native executable.

`/bin/bash scripts/run_examples.sh --gc-stress` passed atom and channel, then
spent several minutes in the full depth-16, 40-tree benchmark without finishing.
That process was stopped; no pass is claimed for it. All 22 other runner examples
passed separate runs with a 60-second per-process deadline. The unchanged full
binary-tree workload passes normal JIT and AOT execution.

## Defects repaired

### Stack growth in generated loops

LLDB showed an `alloca` subtracting from the stack pointer on every join-loop
iteration. Fixed-size match, variant, and FFI temporaries now allocate once at
function entry. Dynamic foreign buffers use stack save/restore around the call
and copy-back, so repeated calls release their storage.

A regression checks that fixed allocas belong to the entry block and runs
200,000 matches; another runs 200,000 native-buffer calls. Temporarily restoring
expression-site allocation makes the structural regression fail.

### Stale addresses across allocating scalar operands

The old normalization only hoisted GC-valued results. A scalar-returning call
could collect after codegen loaded a sibling object's address, so stores wrote
to stale space. This left join results unset and caused infinite polling.

Eager operands now finish and are rooted/snapshotted before the enclosing
operation loads them. Local snapshots also preserve left-to-right values when
a later operand reassigns the original local. Short-circuit right operands stay
conditional. Temporarily disabling scalar hoisting makes the regression return
10 instead of 52; the fixed code returns 52.

### Channel mutex / collection deadlock

A receiver could wait for the channel mutex while marked RUNNING, while a sender
held that mutex waiting to resume after GC. Collection waited for the receiver.
Mutex acquisition now publishes roots and marks the mutator BLOCKED, including
sender clone/drop paths. The unreduced comprehensive and shared-memory/channel
stress suites pass.

### Native-call state and moved copy-back destinations

Native entry now marks the mutator blocked after publishing roots. Native return
and callback reentry wait for collection before resuming managed execution.
Mutable-buffer copy-back reloads the array from its root after the call instead
of retaining a raw pre-call heap address. Foreign argument normalization keeps
buffer conversion and later argument side effects in source order.

Tests cover callback-triggered relocation, a sleeping C call while another
mutator collects, argument order, and repeated buffer calls.

### Project stress selection and example runner

Manifest-driven `run --gc-stress` propagates stress selection to the native
runtime. Standalone executables honor `GCR_GC_STRESS=1`; bare-file JIT runs honor
that variable too. The Bash runner uses indexed arrays supported by macOS Bash
3.2 and checks compiler/program exit status.

### Manifest validation

Typed TOML deserialization replaces the lenient handwritten parser. Malformed
syntax, wrong types, duplicates, unknown keys, invalid package names, and empty
entries are rejected. Quoted hashes, escapes, and multiline arrays parse
correctly. Discovery reports invalid parent manifests instead of suppressing
errors and running without their link configuration. Relative file arguments also
search ancestors above the working directory.

### Integer arithmetic and heap-setting overflow

Division/remainder by zero abort with a diagnostic. Signed minimum divided by
minus one wraps; its remainder is zero. Shift counts are masked by width minus
one. Checked multiplication rejects minimum times minus one in both orders.
See [overflow.md](overflow.md) for the tested contract.

Heap-setting conversion uses checked MiB-to-byte multiplication. Values that
would overflow fall back to the default instead of wrapping to a small heap.

### AOT test archive replacement race

A final parallel validation run caught an AOT fixture observing a missing runtime
archive immediately after a successful Cargo build. Compiler/dependency builds
could replace the same top-level archive. AOT fixtures now use a dedicated target
directory and share one runtime build per integration-test process. Release
fixtures build and link the release runtime rather than the debug runtime.
The full workspace and release AOT/FFI suites pass with the isolated artifacts.

## Implemented, with limits

The implementation contains monomorphized generics, value aggregates, heap
structs/enums, traits, closures, Option/Result, strings, Vec/HashMap, modules,
Thread<T>, atoms/atomics/channels, generational GC, and source/heap tooling.
Passing component and stress tests support those particular behaviors.

Lowering rejects nested, tuple, and struct match patterns. FFI aggregate handling
uses AAPCS64 classification and rejects aggregate returns larger than 16 bytes.
The manifest handles metadata and native linking, not managed dependency
resolution. These are unsupported capabilities, not closed bug fixes.

Heap sizes are startup-configurable: 16 MiB nursery and 256 MiB tenured setting
by default. Allocation exhaustion aborts. Unchecked array/Vec access remains
ordinary callable functions with undefined behavior outside bounds; the public
safety boundary requires a design decision and audit. Floating casts and general
allocation size/offset arithmetic need further boundary review.

## Evidence still missing

No fresh comparative benchmark, visual widget validation, sanitizer run,
clean-machine install test, cross-platform ABI matrix, or sustained service soak
forms part of this assessment. No `.github` CI directory exists. Concurrent
collector scaffolding exists; these runs establish no supported concurrent-major
collection guarantee. Stress harness deadlines and longer automated soaks still
need a project-wide policy.

For new results, record date, checkout, host, command, environment, completed
coverage, failures, and skipped coverage. A code change alone does not close a
defect. Keep proposed capabilities in PRODUCTION.md until implementation and
verification support them.
