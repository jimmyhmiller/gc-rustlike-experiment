# gc-rust

An experimental Rust-like language with a precise moving garbage collector.
The compiler is written in Rust and uses LLVM through an Inkwell fork. The
LLVM-free `gcrust-rt` crate supplies the runtime for native executables.

**Current status:** native execution, collections, threads, C interoperability,
source debugging, and heap inspection exist. Production readiness is unproven.
The workspace tests and full 50-iteration debug/release concurrency gates pass
after fixes to stack allocation, GC rooting, and channel lock coordination.
See [verified status](docs/STATUS.md) for results and limitations.

## Build and run

The workspace uses Rust edition 2024. `Cargo.toml` selects Inkwell's LLVM 21
feature; a compatible LLVM installation and a native linker are build inputs.
This repository does not yet document a tested clean-machine installation.

```sh
cargo gcr-build
./target/debug/gcr check examples/fib.gcr
./target/debug/gcr run examples/fib.gcr
./target/debug/gcr build examples/fib.gcr -o /tmp/gcr-fib
```

`cargo gcr-build` builds the compiler and runtime static library in the same
profile. `cargo gcr-release` does the same in release mode. Native builds link
a profile-matched runtime. In a development checkout, native builds ask Cargo
for a dedicated `target/gcr-aot-runtime` archive and hold its lock through linking;
custom compiler target directories have their own cache. `GCRUST_RUNTIME_LIB`
overrides this with a caller-owned archive for packaging or sanitizer builds.
Runtime build failures stop compilation.

A bare-file `run` without a discovered manifest JIT-executes and prints the
integer result. A project `run` builds a native executable and forwards its exit
status. `run <project> --jit -- <args>` explicitly selects JIT with matching
arguments, stdout, and exit-status behavior. The result of native `main` becomes the process exit status, rather than
the extra result line printed by the JIT driver.

```sh
./target/debug/gcr run examples/project
./target/debug/gcr build examples/project -o /tmp/gcr-calculator
```

## Applications

[gcr-search](apps/gcr-search/README.md) is a gc-rust CLI that saves a text snapshot
of a directory and runs literal queries against it. It exercises checked host
I/O, strings, collections, serialization, native execution, and moving GC.

[gcr-logstats](apps/gcr-logstats/README.md) summarizes real UTF-8 log files with
atomic work claiming and immutable CAS-published summaries. It exercises a fixed
worker set, checked parallel I/O, atomics and moving-GC contention.

[gcr-csvreport](apps/gcr-csvreport/README.md) groups integer CSV data with a fixed
worker set and bounded completion channels. [gcr-buildplan](apps/gcr-buildplan/README.md)
schedules dependency graphs, and [gcr-routes](apps/gcr-routes/README.md) finds cheapest
directed routes. All three validate input and emit JSON to stdout or an atomic
output file; generated reference-model tests cover native/JIT and moving GC.

## Documentation

- [Verified state and open defects](docs/STATUS.md)
- [Production-readiness proposal and acceptance gates](docs/PRODUCTION.md)
- [Implemented language surface](docs/language.md)
- [Compiler IR and pipeline](docs/core-ir.md)
- [GC/runtime](docs/gc.md), [threads](docs/threads.md), [FFI](docs/ffi.md)
- [Checked I/O and application APIs](docs/io.md), [CSV and minimum heaps](docs/csv-heap.md)
- [Modules](docs/modules.md), [mutability](docs/mutability.md), [arithmetic](docs/overflow.md)
- [Debugging, reflection, and heap tools](docs/reflection.md)
- [Runnable examples](docs/tour.md)
- [Benchmark methodology and evidence limits](bench/RESULTS.md)

`cargo test --workspace` passes on the assessed macOS ARM64 host. The docs
distinguish inspected code, observed tests, and proposed work; they make no
blanket performance or memory-safety claim.

The proposed shared-memory, thread, task and async semantics are in
[the concurrency contract](docs/concurrency.md), with an implementation ledger
and required conformance cases.

Remaining work and acceptance criteria are recorded in [the implementation plan](docs/PLAN.md).
