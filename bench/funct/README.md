# funct interpreter: Rust, gc-rust AOT and gc-rust JIT

Run locally on macOS from the repository root:

```sh
cargo build --workspace --release
python3 bench/funct/run.py
```

Requires Python 3, macOS `/usr/bin/time -l`, a working gc-rust toolchain,
and the original funct repository at the path used by
`ports/funct/reference/Cargo.toml` (`~/Documents/Code/funct` in this checkout).
The harness builds the release Rust reference and AOT port. Each of three
process forks runs all nine workloads with one warmup and five measured
iterations per case. Process order is shuffled using a recorded fixed seed.
`--warmup`, `--samples`, `--forks`, and `--output` override these settings.
`--skip-build` reuses existing binaries when already rebuilt from the intended
sources; binary hashes and this choice are recorded in the results.

The `.ft` compute inputs in `workloads/` are copies of funct's original
`bench/ft` programs: fib(30), 2000×2000 loops, collatz over 1..100000, and
10,000-element list map/filter/fold. Other inputs are the port's demo,
example tests, and chess source, plus a one-line startup script. All three
implementations receive identical files. Every iteration's complete output
must match exactly; failed script evaluation or example tests abort the run.

The driver command `bench WARMUP SAMPLES (MODE PATH)+` is available in both
`ports/funct` and its Rust reference. Supported modes are `run`, `test`, `lex`,
and `parse`. Each `run` interval includes source file reading, fresh VM/prelude
initialization, funct parsing/compilation, script execution, result output,
and cleanup before returning. Rust drops its VM inside the interval; gc-rust
may reclaim dead VMs during later iterations. Lex/parse intervals include file
reading, one pass, count output and cleanup. Tests run the ordinary example
test runner. These differ from the old README's whole-process VM timings and
multi-pass lex/parse timings, so compare the freshly measured implementations
to each other rather than mixing the old and new tables.

**JIT compiles the gc-rust interpreter, not the funct script.** The script runs
on funct's bytecode VM in every implementation. AOT uses LLVM O3 and the reference uses Cargo's release profile (O3).
JIT compiles the complete reachable executable eagerly with LLVM O3 before
entering the program. The JIT command is
`gcr run ports/funct --jit -- bench ...`; AOT executes the built native binary.
Both port modes use a 16 MiB nursery, 256 MiB tenured semispace and one GC worker.
Inherited GCR/GCRUST settings are cleared. No benchmark processes run concurrently.

`jit-results.json` records raw iteration timings and outputs, process commands,
wall time, RSS, GC counters, source and binary hashes, toolchain/revision, and
medians. Warmup runs the same cases before measured iterations. On-demand
compilation on a function's first call is included in whichever interval
reaches it; it is not subtracted from individual samples. Process `wall_s` includes
all warmup/measured work, startup and shutdown, plus compilation for JIT.
`outside_workload_s` subtracts all workload intervals from wall time; it includes
compiler/driver/runtime setup, timing output and shutdown, and is not isolated
compilation time. JIT peak RSS includes the compiler and LLVM. Batch medians do
not measure first-script latency. Full-process GC counters include warmup.

`jit-results.json` is the original eager O3 JIT comparison. The files named
`rejected-*` record a rejected reduced-optimization/on-demand experiment.
Their startup measurements do not satisfy the full compilation target.

Measure complete first-script latency with `python3 bench/funct/startup.py`
after a release compiler build. Each run is a fresh process with no compiler
cache and includes eager compilation before user entry. Wall and child CPU
time are recorded. `--runs` and `--output` control the records. The full optimized
compilation target of under two seconds remains open.
