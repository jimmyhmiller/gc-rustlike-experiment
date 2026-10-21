# Repeatable release validation

From the checkout root on the supported macOS ARM64 development host:

```sh
LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21 \
python3 scripts/check_gc_release.py --suite all \
  --output target/gc-validation/my-release-check
```

The output directory must be new. The runner retains a JSON report, individual
command logs, a source archive, file hashes and modes, tool versions, and selected
build environment settings. A failing command stops the suite and retains its
report. Empty test results fail. Each command has a 30-minute timeout by default;
`--timeout SECONDS` changes it. Timeout and interruption terminate the command's
process group, including descendants that ignore SIGTERM.

Remote source verification checks bytes and file modes explicitly, including
when Python runs with `-O` or `-OO`. Mismatched content, modes or missing files
stop validation; optimization cannot remove the guard. Regression tests exercise
the verifier in all three interpreter modes.

Install stable Rust 1.96.0, LLVM 21, Python 3, Docker, and the sanitizer toolchain
before running all suites. The default sanitizer toolchain is
`nightly-2026-03-12`, with the `rust-src` component. An existing equivalent local
alias can be selected with `--tsan-toolchain nightly`. The Linux Miri gate also
requires the pinned Linux nightly's `miri` and `rust-src` components. The sanitizer script
requires macOS ARM64 and the Apple Clang sanitizer runtime; unsupported hosts
fail rather than skipping this gate. Big-endian execution uses the existing
pinned Docker image and s390x QEMU runtime checks.

Suites:

- `core`: Python tooling tests, the complete debug and release workspace tests,
  and the separately armed ignored heap-corruption detector.
- `tsan`: generated native-program and runtime ThreadSanitizer checks.
- `big-endian`: dependency preparation and the selected debug/release s390x
  runtime tests with one and four workers.
- `linux`: full native Linux runtime unit tests in debug/release at worker
  settings one/four, with verification and evacuated-space poisoning, separately
  armed corruption detector, and both doctest profiles. Linux hosts run directly;
  other hosts transfer an isolated source snapshot over SSH to
  `computer.jimmyhmiller.com`. Rust 1.96.0 must already be installed on the Linux
  host. Remote source bytes/modes are checked before and after testing, and the
  staging directory is retained. No existing remote checkout is modified.
- `linux-tsan`: complete runtime unit suite at worker settings one/four on
  native x86-64 Linux, plus explicit detector runs. Rebuilds the standard library
  using installed `nightly-2026-06-24` with `rust-src`, ThreadSanitizer and frame
  pointers. Verification and evacuated-space poisoning are enabled; sanitizer
  reports terminate the run. Uses the same verified SSH snapshot transport to
  `computer.jimmyhmiller.com` from non-Linux hosts. This covers Rust runtime
  execution; generated compiler/JIT/native-code instrumentation remains a separate
  platform requirement.
- `linux-generated-tsan`: build the compiler with LLVM 21 and validate the shared
  50-run generated native corpus on `computer.jimmyhmiller.com` (or directly on
  native x86-64 Linux). Uses `/usr/bin/clang-21` through `CC`, installed LLVM at
  `/usr/lib/llvm-21`, Rust 1.96.0 for the compiler, and `nightly-2026-06-24` with
  `rust-src` for the instrumented runtime/std archive. Generated code and runtime
  link against the same Clang sanitizer. Each fixture/application runs with GC
  stress off/on; program exit codes and application JSON results are checked.
  Linux generated-code CI requires these installed compiler dependencies; the
  hosted workflow currently exercises the shared corpus in its macOS job.
- `linux-miri`: focused runtime interpreter checks on native x86-64 Linux,
  using installed `nightly-2026-06-24` with `rust-src` and `miri`. Runs the
  owner-move/poll and exported allocator-window regressions, moving aggregate/indirect-root regression and
  poisoned-lock restoration at seeds 0, 1 and 17. Every section must execute
  exactly one test; checker flags, twelve seed/case sections and the final summary
  are validated. Default aliasing and leak checks stay enabled. Managed encoded
  pointer paths may emit provenance warnings; this is scoped execution evidence,
  not a whole-collector strict-provenance or soundness claim. Uses the existing
  verified SSH snapshot transport to `computer.jimmyhmiller.com`; CI has an
  independent Linux Miri job. A sysroot setup and each interpretation are bounded.
- `linux-compiler`: complete debug/release workspace tests on native x86-64
  Linux with installed LLVM 21, Clang 21 and Rust 1.96.0. Each profile also runs
  the generated concurrent aggregate JIT regression explicitly and retains its
  eight heap/workload/optimization counters. A separately armed release detector
  must execute once. Missing sections, missing JIT modes, absent scan-time writes
  or shared readers, and missing later generational minors fail validation.
  Uses the same SSH snapshot transport to `computer.jimmyhmiller.com` from macOS.
- `soak`: 72 independent concurrent-allocation ledger runs: three trials of
  three nonzero seeds at one/two/four/eight worker settings in both semispace and
  generational modes. Each run checks 256 concurrent cycles, at least 8192
  allocations, mutation during actual scanning, final graph/live-byte agreement,
  and minor collection in generational mode. The runner validates the reported
  seed/mode and rejects missing or incomplete coverage even if Cargo exits zero.
  Each run has a three-minute maximum timeout (or the lower `--timeout` value).
  Only stable Rust and Python are needed for this runtime-only suite.
- `all`: all of the above, sequentially.

Inherited `GCR_` and `GCRUST_` experiment settings are removed; each gate supplies
its required settings explicitly. Normal Cargo, compiler, and linker environment
settings are retained. Rust result totals count executions; sanitizer and
big-endian filters can overlap, so totals are not unique test counts.

Do not edit build or test inputs while validation runs. The runner checks source
hashes at command boundaries, which cannot detect a transient edit restored
between checks. Its lock excludes other instances of this runner in the same
checkout; unrelated builds and manual edits do not take that lock. Build inputs
under src, crates, tests, scripts, examples, bench, apps, .cargo and .github,
plus root manifests, build script and README are archived. Generated directories
are excluded. Source symlinks are rejected. Historical readiness documents under
docs are outside this source scope and can be updated during validation.

The GitHub Actions workflow uses separate native, macOS sanitizer, big-endian, Linux-runtime, Linux sanitizer, Miri and soak jobs
and retains each job's evidence for 14 days, including failures. Configuring the
workflow and passing local checks does not establish that hosted CI has passed.
A passing suite also does not establish complete production readiness: platform
coverage, longer-running workloads, remaining runtime audits and numerical
performance acceptance budgets remain separate requirements.

Run the bounded randomized matrix independently with:

```sh
python3 scripts/check_gc_release.py --suite soak \
  --output target/gc-validation/my-concurrent-soak
```

Each case's environment, seed, worker setting, mode and checked workload counters
are retained in `report.json`, alongside raw logs and the source snapshot. Worker
settings are upper bounds chosen by the collector policy, not proof that every
small collection uses that many workers. This is a repeated bounded correctness
matrix; it does not establish long-duration leak freedom, RSS, tail latency or
numerical performance acceptance budgets.

AOT linking honors `CC` as one compiler executable path, defaulting to `cc`.
Paths containing spaces are supported; compiler flags belong in explicit link
arguments rather than in the `CC` executable name. Sanitizer validation uses a
specific driver so generated code and the Rust archive share one runtime.
