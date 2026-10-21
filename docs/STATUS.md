# Verified state

## Generated-code Linux sanitizer integration — 2026-10-18

AOT linking now honors `CC` as a single compiler executable path, defaults to
`cc`, and names the selected executable in errors. Missing compiler paths fail
as requested; paths containing spaces link and run. The regression passes in
macOS debug/release and Linux debug. This enables explicitly selecting installed
Clang 21 for generated-code sanitizer validation on `computer.jimmyhmiller.com`.
The previous hard-coded GCC 13 driver linked a sanitizer that failed startup with
an unexpected-memory-mapping diagnostic; this external toolchain observation is
retained in the project pad. No host configuration changed.

The new `linux-generated-tsan` release suite builds the compiler with LLVM 21,
rebuilds Rust runtime/std with instrumentation and an external Clang sanitizer,
and validates a shared corpus: 22 fixtures and three model-checked applications,
each with GC stress off/on. **All 50 Linux runs passed, with zero reports**.
The same shared harness passed all **50 macOS generated runs**. Its fixture list
and application expectations match the preceding archived harness exactly.
Missing/repeated program/stress combinations fail report validation. Linux uses
an isolated SSH snapshot; source bytes/modes match before and after execution.
The hosted macOS job uses the shared harness; hosted Linux generated-code CI
still needs the compiler dependency setup and is not claimed here.

Full local validation passed **1045 debug and 1044 release workspace tests**,
21 tooling tests, one armed detector, and 413 runtime sanitizer executions plus
the 50 generated macOS runs; zero failures. Filter totals overlap; ten/eleven
workspace ignores remain. All 472 source/archive entries and modes match in the
local and remote evidence. Runtime code is unchanged; diff check passes.
Evidence: `target/gc-validation/2026-10-18-linux-generated-tsan/verification.json`,
`remote/report.json`, `local-gates/report.json`, `linux-linker-test.log`.

Remaining operation/embedding/native-accounting and concurrent-publication audits,
long-duration resource/latency budgets, broader platform compiler validation and
hosted CI evidence remain open. Overall production readiness is unfinished.

## Native Linux ThreadSanitizer gate — 2026-10-18

`--suite linux-tsan` now instruments the complete Rust runtime and rebuilt
standard library on native x86-64 Linux using pinned `nightly-2026-06-24`.
From macOS it uses the verified SSH snapshot transport to Jimmy's existing
`computer.jimmyhmiller.com`; Linux CI runs directly. The release runner's `all`
suite and an independent CI job include it. Verification, evacuated-space poison,
frame pointers and `TSAN_OPTIONS=halt_on_error=1` are enabled. No suppressions or
host configuration changes were made.

Both worker settings (1/4) passed **527 runtime tests each**, with six intentional
ignores per run. Both separately selected corruption detectors passed exactly
one test: **1056 successful instrumented executions, zero failures and no
sanitizer reports**. The report requires all four matrix sections and rejects
missing/empty sections and unarmed detectors. All 20 tooling tests, actionlint and
shell syntax checks passed. All 470 source/archive entries and modes were verified
locally and remotely before/after execution. Only gate/CI tooling changed from the
native Linux checkpoint; compiler, runtime and their tests are unchanged.
Evidence: `target/gc-validation/2026-10-18-linux-tsan/verification.json`,
`remote/report.json`, `tooling/report.json`.

This covers Rust runtime execution and its instrumented standard library.
Generated compiler/JIT/native-code sanitizer integration remains unverified on
Linux and separately covered on macOS. Hosted CI, remaining operation/embedding
and native-accounting audits, concurrent publication, long-duration resource and
numeric latency/performance budgets remain open. Overall production readiness
is unfinished.

## Native Linux runtime gate — 2026-10-18

`--suite linux` now runs the full runtime on native Linux. From this macOS host it
uses Jimmy's existing `computer.jimmyhmiller.com` via SSH, transfers an isolated
source archive and retains its staging directory. Linux CI hosts run the same
native script directly. The unvalidated Docker Linux draft was replaced; the
existing s390x dependency preparer remains unchanged. Rust is pinned to installed
1.96.0; no existing remote checkout or host configuration is modified.

On x86-64 Linux, both worker settings (1/4) passed **528 debug and 527 release
runtime tests per setting**, with verification and evacuated-space poisoning.
All four explicit corruption-detector runs passed exactly one test, and each
profile passed nine doctests: **2132 successful executions, zero failures**.
Totals overlap; runtime ignores are five/debug and six/release, plus five ignored
documentation examples per profile. The report validates all ten matrix sections
and rejects missing/empty sections and unarmed detectors. All 19 tooling tests,
actionlint and shell syntax checks passed. All 469 source/archive entries and
modes were verified locally and on the remote before/after execution.

Poisoning exposed an invalid test assumption: the actual-collection barrier
regression read old aliases after their arena was reclaimed. It now reloads the
registered root across three collections, checking current-space membership,
both barriers and payload. Pre-reclamation acquire/publication coverage remains
separate. Production runtime code is unchanged; source comparison verifies this.
The first failed run and the intermediate gate's empty debug-only detector
selection are retained. The final gate uses `--include-ignored --exact` and checks
one detector execution per configuration.

Evidence: `target/gc-validation/2026-10-18-native-linux/verification.json`,
`remote-verified/report.json`, `tooling/report.json`. This is runtime-only Linux
evidence; compiler/JIT, Linux sanitizer, hosted CI, remaining operation/embedding
and native-accounting audits, concurrent publication and numeric resource/latency
budgets remain open. Overall production readiness is unfinished.

## Repeatable concurrent mutation soak — 2026-10-18

The release runner now provides `--suite soak`; `all` includes it. An independent
CI job retains the same evidence. The matrix has three trials of three nonzero
seeds, four worker settings (1/2/4/8) and both heap modes: 72 isolated runs of the
existing independent mutation ledger. Each run requires one exact passing test,
matching seed/mode, 256 concurrent cycles, at least 8192 allocations, mutation
during actual scanning, final graph/live-byte validation and generational minors.
Missing/duplicate summaries and incomplete coverage fail even after exit zero.
Each process group has at most 180 seconds, or a lower user-selected timeout.
Worker settings are policy limits, not proof of parallel participation per cycle.

Local execution passed **72 runs, 18,432 concurrent cycles, 816,749 allocations
and 12,459 minor collections**. All runs overlapped mutation with scanning
(minimum 32 recorded active iterations). All 18 tooling tests and actionlint
passed. Both source archives and current bytes/modes for 467 inputs were verified.
Only release-runner/tests/CI inputs changed from the reusable-SATB checkpoint;
runtime/compiler/test code is unchanged. The initial parser rejection of Rust's
same-line test-name prefix is retained; the corrected parser accepts the exact
known prefix and is regression-tested. Evidence:
`target/gc-validation/2026-10-18-repeatable-soak/verification.json`,
`soak-final/report.json`, `tooling/report.json`.

This establishes repeatable bounded randomized correctness coverage. Hosted CI
has not run; long-duration memory/RSS/latency budgets, wider platforms,
operation/embedding audits, remaining native accounting and concurrent publication
coverage remain open. Overall production readiness is unfinished.

## Reusable mutator SATB publication — 2026-10-18

Active-cycle SATB flushes now synchronously publish borrowed buffer contents,
then clear the local vector while retaining its backing allocation. Previously
every full flush allocated a replacement before the deduplication queue could
discard repeated values. Unique values still enter bounded queue chunks, decoded
in fixed stack batches outside the queue mutex; no local buffer reference escapes.
Owned public chunks and standalone publication still transfer their allocations.
Both pre-write logging and safepoint flushes use the reusable path.

The runtime-path regression warms and drains one unique publication, then checks
64 full duplicate-only flushes for zero native allocation/deallocation traffic,
empty buffers and one unique value. Restoring replacement-buffer publication
fails that check. A second regression checks 513 distinct tagged references,
null/out-of-source rejection, repeated cycle reset, stable local storage and
standalone allocation transfer. All 13 focused optimized SATB tests pass after
source restoration. The negative control records 64 allocations/deallocations
and 131,072 allocated bytes across 64 duplicate-only flushes, versus zero traffic
on the reusable path. No application throughput/latency speedup is claimed.

Full local validation passed: **1044 debug and 1043 release workspace passes**,
15 tooling tests, one armed detector, 50 generated native plus 413 runtime
ThreadSanitizer executions, and 728 big-endian executions; zero failures. Totals
overlap; ten/eleven workspace ignores remain. Both new tests ran in debug/release,
TSAN and all four big-endian profile/worker configurations. All 467 source/archive
entries and modes and the restored fault-control source hash were verified;
diff check passed. See `verification.json` and `gates/report.json` in the evidence
directory below.

Operation/embedding audits, remaining native accounting, concurrent publication
coverage, hosted CI/platform/soak evidence and numeric budgets remain open.
Overall production readiness is unfinished.

Evidence: `target/gc-validation/2026-10-18-satb-buffer-reuse/`.

## Published canonical access — 2026-10-18

Published old aliases now use an acquire forwarding load and lock only the
canonical destination stripe. Previously they acquired the source stripe and
then reacquired the ordered source/destination pair. A publication racing the
initial check also needs only the destination after releasing the source.
Forwarding is immutable until a stopped-world swap, which cannot overlap the
caller's non-safepointing access. The copier retains its ordered pair through
publication, so destination locking still excludes copy completion and guarded
payload access. Marker-only claims retain the existing retry path.

A bounded Compact/Full/offset-header regression holds a distinct source stripe,
resolves and writes canonical storage, verifies the destination lock is held,
checks old payload remains unchanged and detects native allocation traffic.
Restoring source locking times out; removing the destination guard fails the
protection assertion. Both controls fail as intended, source restoration and all
six focused reference tests pass. Full local gates passed: **1042 debug and
1041 release workspace passes**, 15 tooling tests, one armed detector, 50 generated
native plus 411 runtime ThreadSanitizer executions, and 720 big-endian executions;
zero failures. Totals overlap; ten/eleven workspace ignores remain. The new test
ran in debug/release, TSAN and all four big-endian profile/worker configurations.
All 467 source/archive entries and modes and the restored fault-control source
hash were verified; diff check passed. This reduces lock acquisitions; no
throughput or latency improvement is claimed without a matched measurement.

Operation/embedding audits, remaining native accounting, concurrent publication
coverage, hosted CI/platform/soak evidence and numeric budgets remain open.
Overall production readiness is unfinished.

Evidence: `target/gc-validation/2026-10-18-canonical-access/`.

## High-tag graph oracle and big-endian matrix — 2026-10-18

Four new graph-oracle cases use upper-byte `0xa5` tagged references, including
bit 63, across Compact/Full headers and major/promotion paths. Every case runs
16 seeds and six cycles with raw payload, exact live bytes, all edges, nulls,
immediates, cycles and alias identity checked independently of collector traversal.
Major paths alternate STW/concurrent collection; promotion paths verify minor
survivors before major reclamation. These are quiescent graph tests, not concurrent
publication evidence.

All **12 graph cases passed in debug, release and rebuilt ThreadSanitizer**.
The full sanitizer gate before the final emulation-only harness adjustment passed
50 generated native and 410 runtime executions. The expanded big-endian gate
passed **716 executions**, including all 12 graph cases in each of four
profile/worker combinations. Source/archive bytes and modes for 467 inputs and
per-test coverage were verified; diff check passed. Production runtime behavior
is unchanged from the preceding fully validated forwarding checkpoint.

The initial emulated run exposed a harness re-execution failure (child exit 127,
no graph execution). Emulated invocations now explicitly use in-process graph
execution under the gate's 180-second timeout. Native/sanitizer defaults retain
per-test 30-second child deadlines. Failed evidence remains retained.
Evidence: `target/gc-validation/2026-10-18-high-tag-graph/verification.json`,
`focused-final.json`, `tsan/report.json`, `big-endian-fixed/report.json`.

Concurrent publication, complete operation/embedding audits, remaining native
accounting, hosted/platform/soak evidence and numeric performance budgets remain
open. Overall production readiness is unfinished.

## Published forwarding fast path — 2026-10-18

An acquire load now returns an already-published forwarding destination without
acquiring the source stripe. Publication remains a release store after destination
initialization. Marker-only claims still enter the locked retry path; forwarding
remains immutable until the stopped-world swap.

New bounded regressions cover source-lock bypass for Compact, Full and offset
headers, allocation-free destination reuse, and a paused publication race. Removing
the fast path causes the held-stripe test to time out. Treating a marker-only claim
as a published destination fails the publication test. Both controls failed as
expected; source was restored and all five focused reference tests passed.

Eight alternating process pairs per workload compare saved release binaries with
identical benchmark harnesses. Across 64 processes, every graph validates payloads,
aliases, destination membership and live bytes. Median paired candidate/baseline
cycle ratios are **0.377/0.373 for 65,536/262,144 shared references**, approximately
62–63% lower total cycle time on top of the batched scanner. Distinct-child ratios
are **0.999/1.000**; RSS ratios range from 1.0004 to 1.0023. These are warmed
collector-only cycles without competing mutators. Application throughput, tail
latency, startup, pause budgets and general performance acceptance remain open.

Full local validation passed: **1037 debug and 1036 release workspace passes**,
15 tooling tests, one armed detector, 50 generated native plus 406 runtime
ThreadSanitizer executions, and 668 big-endian executions; zero failures.
Suite/filter totals overlap; ten/eleven workspace ignores remain. Both new tests
ran in debug/release, ThreadSanitizer and all four big-endian profile/worker
combinations. All 467 source/archive entries and modes, benchmark binaries,
harness/script and raw timing logs were verified; diff check passes.
Evidence: `target/gc-validation/2026-10-18-forwarded-fast-path/`
(`verification.json`, `benchmark.json`, `fault-controls.json`, `gates/report.json`).

Operation/embedding audits, remaining native accounting, forced transition
coverage, hosted CI/platform/soak evidence and numeric performance budgets remain
open. Overall production readiness is unfinished.

## Bounded concurrent reference snapshots — 2026-10-18

Concurrent discovery now loads up to 32 reference slots under one canonical
owner-lock acquisition, releases the guard, then decodes/copies children. The
bounded snapshots cover fixed fields, interior references and variable tails.
Object geometry remains immutable, parent rewrites remain deferred to final STW,
and SATB/final rescanning preserve edges changed between batches. Scratch is
bounded stack storage; no guard crosses a child copy or pointer-policy callback.

New regressions cover batch boundaries across Compact/Full headers and
identity/tagged policies, raw-neighbor preservation, nulls and shared children.
A direct pre-final-scan test forces owner/child stripe collision, checks zero
native scratch allocations, and gives the final partial batch its only child.
Omitting that tail fails; retaining the owner guard across copying deadlocks and
fails the bounded test. Source restoration and both regressions pass.

Eight alternating process pairs per workload compare matched-harness saved
baseline/candidate release binaries. Every collection validates all payloads and
live-byte counts; two warmups precede nine timed cycles. All 64 runs pass (576
measured plus 128 warmup cycles). Median paired candidate/baseline cycle ratios:
**0.639/0.631 for 65,536/262,144 shared references**, and **0.924/0.918 for distinct
children**. RSS ratios are 1.000/1.000 and 0.999/0.999. The observed reductions are
approximately 36–37% and 8%. These are warmed collector-only total-cycle timings
without competing mutators; no application throughput, mutator/tail latency,
startup, pause reduction or numerical-budget claim follows. Host observations and
raw samples are retained. A post-measurement cfg(test)-only tail-oracle change is
identified separately; measured production source/binaries are unchanged.

Full local validation passed: **1035 debug and 1034 release workspace passes**,
one separately armed detector, 15 tooling tests, 50 generated native plus 404
runtime sanitizer executions, and 660 big-endian executions; zero failures.
Filter/suite totals overlap; ten/eleven workspace ignores remain. Both new tests
appear in debug/release, ThreadSanitizer, and all four big-endian profile/worker
combinations. All 467 current source/archive entries and modes, both benchmark
archives/binaries, harness/script and raw timing logs were verified. Diff check
passes. Evidence: `target/gc-validation/2026-10-18-concurrent-scan/` (`verification.json`,
`benchmark.json`, `benchmark-scope.json`, `fault-controls.json`, `gates/report.json`).
`baseline-matched/` is the comparison baseline; `baseline/` is initial discovery
material and is not used in the paired result.

Remaining operation/embedding audits, total native accounting, forced transition
coverage, hosted CI/platform/soak evidence and numerical performance budgets are
still open. Overall production readiness remains unfinished.


## Generated nursery/concurrent-major integration — 2026-10-18

The generated-access regression now runs both semispace and generational heaps,
with a 1 MiB nursery and 32 MiB tenured space in generational modes. Both single
and shared workloads run optimized and unoptimized. Generational coverage requires
a successful nonparking aggregate replacement during concurrent copying on an
old owner with nursery references, followed by a strictly later minor collection.
The observer reads reference offsets/values without retaining managed pointers or
adding roots, delegates to production try-write, and records the minor count
before any safepoint. Existing exact snapshot/scalar/live-graph checks and shared
participant coverage remain. Semispace single-mutator published-alias coverage
remains mandatory; no generational published-alias claim is added.

An initial generational draft waited indefinitely for an alias condition that
this source did not reliably produce, despite many successful old-to-young writes
and later minors. Those failed runs and a native sample are preserved. The
coordinator now expresses the distinct generational requirements and stops
issuing majors once major-copy coverage is satisfied while allocation pressure
triggers the required later minor. The 60-second whole-workload bound remains.
This is integration coverage, not an isolated proof of a particular card/SATB
barrier: final major rescanning can also preserve edges; deterministic runtime
barrier tests remain separate evidence.

Fresh validation: **241 focused compiler/concurrency/root/thread-completion
passes in debug and 241 in release**, zero failures or ignores. Ten release
repetitions cover eighty code/heap modes. All forty generational modes completed
active old-to-young writes and 1–4 subsequent minors; their observed major-cycle
counts were 8–206 and young-edge writes 1–94,751. These are coverage observations,
not throughput, fairness, pause or RSS budgets. All 466 source/archive entries
and modes match; diff check passes. Source comparison confirms only the existing
regression body changed. Production compiler/runtime inputs are unchanged;
sanitizer/big-endian gates were not repeated and do not instrument this JIT test.

Final evidence: `target/gc-validation/2026-10-18-generated-nursery-major/`
(`report.json`, `scope.json`, `verification.json`, repetition/profile logs).
Development diagnostics: `target/gc-validation/2026-10-18-generational-development/`.

The aggregate nursery/major integration gap is now exercised. Other operations,
forced claim/publication transitions, embedding, total native accounting,
platform/soak coverage and numerical performance budgets remain open. Overall
production readiness is unfinished.


## Multiple generated mutators during concurrent copying — 2026-10-18

The bounded generated-code regression now includes two writers and a dedicated
reader sharing reference-bearing aggregate snapshots and a scalar slot. A live
120,000-reference graph is published read-only, checked during reading and fully
validated after all joins. Test-only ABI observers delegate to production access
operations, distinguish participant Thread addresses without dereferencing foreign
Thread state, and require successful active-copy writes from both writers plus
active-copy snapshots from the dedicated reader. The single-mutator forwarded-
alias case remains and now uses the same graph size. Runtime child completion is
drained before retiring the JIT engine.

The test coordinator waits for initialized managed work and all shared
participants, runs at least eight cycles until required coverage is observed,
and permits managed-operation progress between cycles. Completion is polled
between 1024-operation chunks because the foreign polling ABI parks. Data checks
remain per iteration, and the whole child process retains its 60-second bound.
Earlier small/fixed-cycle drafts missed participant scheduling. Full-release
runs also exposed missing single-mutator alias coverage and a timeout with the
mutator repeatedly waiting in ai_ffi_leave while collections kept running. The
failed reports and a native stack sample remain archived; final coverage-driven
coordination and chunked completion polling pass.

Fresh validation: **241 compiler/concurrency/root/thread-completion passes in each
of debug and release**, zero failures or ignores, plus ten release repetitions
covering forty generated modes. All twenty shared modes proved both writers and
the reader active during copying; they needed 8–37 cycles. Work varies with
scheduling: shared modes completed 2–41,463 active writes and 1–39,900 dedicated
reader snapshots. These are coverage counts, not throughput or fairness budgets.
All 466 source/archive entries and modes match; diff check passes. Comparison
with the prior archive confirms only this test body and its fixture changed;
production compiler/runtime inputs are unchanged. Sanitizer and big-endian gates
were not repeated and do not instrument this new JIT workload.

Final evidence: `target/gc-validation/2026-10-18-generated-shared-chunks/`
(`report.json`, `scope.json`, `verification.json`, repetition/profile logs).
Earlier failed runs remain under `generated-shared`, `generated-shared-final`,
and `generated-shared-verified`; the latter includes `stalled-child.sample`.

This is semispace concurrent integration with explicit test Sync assertions.
It does not close nursery/major interaction, every runtime operation, forced
claim/publication transitions, embedding, total native accounting, platform/soak
coverage or numerical performance budgets. Production readiness remains unfinished.


## Generated access through published old aliases — 2026-10-18

The active-copy compiler fixture now reloads its holder through a heap edge.
Concurrent discovery defers healing that edge until final STW, so generated
operations receive old bases whose forwarding destinations are already published.
The test-only observer requires successful production try-writes through such
aliases in both optimized and unoptimized code. Exact snapshot/scalar checks and
the live 12,000-reference array remain. Ten release repetitions (twenty modes)
completed eight cycles and 1,200–1,568 old-alias writes per mode.

Production fault controls independently store through the incoming old base or
read the snapshot from it, bypassing canonical storage. Both now fail with model
result 81 instead of 42. Each source mutation was restored; repeated release and
focused debug/release checks pass. This strengthens the previous fixture, whose
stale-alias fault was not detected. It proves published-alias use and correct
canonical reads/writes, not a forced generated-code rendezvous during the
claim-to-publication transition or complete multi-mutator coverage.

Fresh validation: **213 compiler/managed-memory/operand-root passes in each of
debug and release**, zero failures or ignores. All 465 source/archive entries and
modes were verified. Comparison with the preceding full-validation source archive
confirms the only source change is inside the existing regression body; production
compiler and runtime code are unchanged. The preceding full suite remains the
baseline evidence, not a newly repeated sanitizer/big-endian result. Diff check
passes. Evidence: `target/gc-validation/2026-10-18-generated-old-alias/`, including
`report.json`, `scope.json`, and `verification.json`; fault/repetition logs are in
`target/gc-validation/2026-10-18-generated-old-alias-fault/`.

No performance budget is established. Forced transition rendezvous, multi-mutator
active-copy integration, remaining embedding/native accounting, platform/soak
coverage and numerical budgets remain open. Production readiness is unfinished.


## Generated aggregate operations during concurrent copying — 2026-10-18

A bounded compiler regression now executes generated scalar and reference-bearing
aggregate accesses alongside a separate concurrent collector. A live array of
12,000 references checks preservation across relocation; exact snapshot values
and scalar writes are checked against the iteration model. Both optimized and
unoptimized code must span eight concurrent cycles. A test-only ABI observer
calls the production nonparking try-write and requires successful replacements
while copying is active. Ten repeated release runs (twenty generated-code modes)
observed 21,542–26,275 active-copy writes per mode. Production/test JIT contexts
now share the runtime-symbol mapping helper.

A fixed-workload version passed debug but finished after seven cycles in the
first complete release run. An explicit native completion handshake now keeps
work/checks running until all eight cycles finish, instead of assuming scheduling
from a loop count. An initial sum-only oracle was strengthened to exact values.
A fault falsely acknowledging active-copy stores without writing fails the final
regression; restored source passes. An earlier stale-alias control was **not**
detected and remains archived: deterministic old/new alias transitions are not
proved by this fixture. It covers one generated mutator and a concurrent collector,
not the complete multi-mutator or embedding audit. The new JIT regression itself
is not run under the separate ThreadSanitizer harness.

Final local validation: **1033 debug and 1032 release workspace passes**, one
armed detector, 15 tooling tests, 50 generated native and 402 runtime sanitizer
executions, and 652 big-endian executions; zero failures. Filter/suite totals
overlap. Debug/release retain ten/eleven ignored tests, with the detector separately
armed. All 465 source/archive entries and modes were verified; diff check passes.
Final evidence: `target/gc-validation/2026-10-18-generated-concurrent-final/`.
Fault/repetition evidence: `target/gc-validation/2026-10-18-generated-concurrent-fault/`.
The earlier failed full run remains at `target/gc-validation/2026-10-18-generated-concurrent/`.

No throughput, pause, tail-latency or RSS budget is established by this correctness
workload. Remaining alias-transition/multi-mutator/embedding audits, total native
accounting, hosted CI/platform/soak coverage and numerical budgets remain open.
Production readiness remains unfinished.


## Native buffer retirement-node accounting — 2026-10-18

Deferred buffer payloads were charged to native pressure, but their separately
allocated intrusive retirement nodes were omitted. Nodes now use allocator-api2
boxes with the buffer owner's metadata allocator. The allocator retains the
independent counter through physical deallocation, including native-worker
ownership after queue detachment. Buffer-byte metrics retain their existing
scope. Nodes are still reserved in native construction; publication remains
allocation-free. This adds accurate metadata demand rather than a memory cap.

A paused-consumer regression checks exact requested node bytes while reserved
and detached, then zero after destruction; synchronous explicit release is also
covered. Temporarily replacing the node allocator with an unaccounted allocator
makes this regression fail. Source restoration and the passing regression were
verified afterward. Existing allocation-free publication checks also pass.

Full local validation: **1032 debug and 1031 release workspace passes**, one
armed detector, 15 tooling tests, 50 generated native and 402 runtime sanitizer
executions, and 652 big-endian executions. Zero failures; filters/suites overlap.
Ten/eleven debug/release ignores remain, with the detector run separately.
All 465 source entries and archived bytes/modes match after the fault control.
Evidence: `target/gc-validation/2026-10-18-retirement-node-accounting/`, including
`report.json`, `verification.json`, and `fault-control.json`. Diff check passes.

No fresh throughput/RSS benchmark or universal performance claim accompanies
this accounting correction. Shared Arc/control blocks, other native accounting,
remaining concurrent/embedding audits, wider platforms, soaks and numerical
budgets remain open; overall production readiness remains unfinished.


## Repeatable release validation and CI — 2026-10-18

Added `scripts/check_gc_release.py` and `.github/workflows/gc-validation.yml`.
One command runs the complete debug/release workspace, separately armed detector,
generated/runtime ThreadSanitizer checks, and selected s390x big-endian checks.
The runner retains command logs, tool/environment details, a source archive and
hashes; rejects empty results; excludes concurrent runner instances per checkout;
and terminates timed-out command groups. Fifteen Python tooling tests pass,
including archive integrity, source changes, lock contention, failure retention,
and termination of a descendant that ignores SIGTERM. Application sources are
included in the archive. See [usage and scope](release-validation.md).

The local `all` suite passed: **1031 debug workspace passes, 1030 release workspace
passes, one armed detector, 50 generated native sanitizer executions, 401 runtime
sanitizer executions, and 648 big-endian executions**, with zero failures. Counts
across filters/suites overlap. Debug/release retain ten/eleven ignored tests;
the release-only ignored detector is run separately with verification armed.
All 465 source entries and archive bytes/modes were reverified after completion.
Workflow lint and diff checks pass. Evidence:
`target/gc-validation/2026-10-18-release-gate/report.json` and `verification.json`.

Hosted CI has not run. The local sanitizer toolchain used the installed `nightly`
alias whose manifest date is 2026-03-12; CI selects that dated nightly explicitly.
This checkpoint adds repeatable validation, not a new performance result or a
claim of overall production readiness. Remaining audits, platform coverage,
long-running workloads and numerical performance budgets remain open.


## Rooted aggregate-source reuse and contended relocation — 2026-10-18

Aggregate try-writes now use existing ANF value-local storage directly. Its native
address is stable; reference interiors are already indirect caller-frame roots.
The nonparking runtime operation only reads it and does not retain the pointer.
Constants still receive staging. The contended fallback loads the RHS after
acquisition, preserving collector-updated references. An unoptimized emission
regression fails on the old redundant staging alloca/copy; normal/stress JIT
behavior also passes.

A bounded native regression forces a failed try and a blocked acquisition,
installs a real indirect shadow frame, performs moving collection, then checks
both the relocated destination and two updated RHS references. UnsafeCell models
collector-written value storage; a guard unlinks the frame before its stack
storage retires. Only the owner's address crosses threads for atomic blocked-
state observation, with lifetime bounded by its join. The fixture does not
execute generated fallback code under this rendezvous; emission/integration
checks cover generated code separately. Caller-error controls remove indirect
roots or reuse stale RHS bits. Production controls return a stale base or
reintroduce staging. All four fail, with source restored after each.

Final verification: **1031 workspace passes, zero failures, ten ignored**;
515 optimized runtime units plus nine doctests, armed detector, 49 optimized
compiler integrations, 648 big-endian executions, 50 generated native and 401
runtime ThreadSanitizer executions. Filters/extra checks overlap. All 166 source
pins and archive entries match, as do benchmark compiler/executable/runtime/
fixture/codegen hashes. Diff check passes. Evidence:
`target/gc-validation/2026-10-18-aggregate-source/report.json`.

All 128 benchmark outputs are correct. Racing aggregate/scalar and million-write
reference-bearing workloads have median paired wall ratios 0.980 and 0.993;
RSS ratios 1.005 and 1.000. Both compilers link identical runtime. No collections
occurred in these timings; racing retries/allocations and host activity differ.
The emission check proves removal of redundant staging, while these timings do
not establish universal speedup, active-copy throughput, pause reduction or
numerical budgets. No new whole-runtime Miri claim is made.

Full generated/runtime concurrent integration, embedding/root/shutdown audit,
total native accounting, wider platforms/soaks and numerical budgets remain open.
Production readiness remains unfinished.

## Concurrent aggregate replacement — 2026-10-18

Generated inline aggregate stores now try a non-safepointing transaction before
the rooted legacy fallback. The runtime takes the aggregate stripe and canonical
ObjectAccess, logs displaced references, copies untraced gaps and atomically
replaces traced words, marks nursery cards at canonical slots, then linearizes
before releasing canonical exclusion. Sorted constant per-value reference tables
avoid scanning unrelated owner fields. A contended attempt changes neither
payload nor SC order; the fallback reloads the ANF RHS after possible relocation.
Reads also retain canonical exclusion through their SC event so old/new aliases
remain ordered even when their aggregate stripes differ. Legacy ABI remains.

A real concurrent cycle pauses discovery before an overwritten child is found.
The write must finish through an old parent alias before copying resumes. Tests
check canonical snapshots, displaced-child SATB retention, destination card state
before final rescan, later nursery promotion and untouched neighboring fields.
A separate failed-try reference checks payload/order/allocation effects. Four
fault controls fail: absent SATB, absent card marking, old-alias storage and a
disabled active-copy path. Initial hook/coverage mistakes remain archived: a
shared child triggered a repeated rendezvous, and final-major card rebuilding
masked the first missing-card control. A one-shot hook and pre-rescan assertion
repair the test without removing the shared edge or later promotion check.

Final verification: **1029 workspace passes, zero failures, ten ignored**;
514 optimized runtime units plus nine doctests and separately armed detector,
48 optimized compiler integrations, 644 big-endian executions, 50 generated
native and 400 runtime ThreadSanitizer executions. Filters/extra checks overlap.
All 166 source pins/archive entries and benchmark compiler/executable/runtime/
fixture/codegen hashes match. Diff check passes. Evidence:
`target/gc-validation/2026-10-18-aggregate-write/report.json`.

All 128 benchmark outputs are correct. Racing aggregate/scalar and million-write
idle workloads have median paired wall ratios 1.008 and 0.974; RSS ratios 1.010
and 0.993. Both compilers link identical runtime; the baseline is the preceding
aggregate-read compiler. No collection occurred in these timings. Racing retry
counts/allocations and recorded host activity differ. These observations do not
establish universal speedup, active-copy throughput, pause reduction or numeric
budgets; active-copy correctness/progress comes from deterministic tests.

Full generated/runtime concurrent integration, embedding/root/shutdown audit,
total native accounting, wider platforms/soaks and numerical budgets remain open.
No new whole-runtime Miri claim is made. Production readiness is unfinished.

## Concurrent aggregate reads and target layout — 2026-10-18

Generated inline aggregate field reads now call `ai_managed_read`. The runtime
acquires the aggregate stripe before canonical ObjectAccess, retains both through
the native snapshot copy, then releases both before returning. Uncontended reads
can complete while copying continues. Contention retains rooted blocking and
relocation revalidation. Aggregate stores and the legacy lock/unlock ABI remain.
No borrowed access guard escapes the runtime operation.

The native-buffer integration exposed a target-layout mismatch: modules had no
host data layout before optimization. LLVM folded a value-enum raw-field address
to offset 12, while native emission used offset 16. Modules now install the host
triple and data layout before generation/optimization. The mixed i32/i64/reference
regression returns wrong fields (81) if that setup is removed. Before/after IR
and the initial integration failures are retained.

An actual-cycle regression pauses child discovery, mutates canonical parent
scalars, and requires both old/new-alias snapshots to complete before copying
resumes, preserving the reference and canonical scalar values. Three deliberate
faults fail: whole-cycle waiting, bypassed canonical access, omitted target
layout. The big-endian gate runs this test directly under QEMU with external
bounds rather than reexecuting the foreign binary.

Final verification: **1027 debug workspace passes; 1026 optimized workspace
passes plus the separately armed detector; zero failures**. Debug/release ignores
are ten/eleven. Optimized runtime: 512 units plus nine doctests. Also passed
42 focused debug integrations, 636 big-endian test executions, 50 generated native
and 398 runtime ThreadSanitizer executions (filters overlap). All 166 source pins
and archive entries match, along with both benchmark compilers/executables,
fixtures, codegen source and runtime library. Diff check passes. Evidence:
`target/gc-validation/2026-10-18-aggregate-read/report.json`.

All 128 benchmark outputs are correct. Racing aggregate/scalar and million-read
idle workloads have median paired wall ratios 0.997 and 0.942; RSS ratios 1.005
and 1.007. No collections occurred in those measurements. Racing CAS retries
and allocation counts differ, and host activity is recorded. These observations
do not isolate one change's causal cost or establish active-copy throughput,
pause reduction or numeric budgets. Active-copy progress comes from the separate
paused-copy regression. No new whole-runtime Miri claim is made.

Full generated/runtime concurrent integration, aggregate-store progress during
copying, total native accounting, embedding, broader platforms/soaks and numerical
budgets remain open. Overall production readiness remains unfinished.

## Prepare concurrent access locks before copying — 2026-10-18

First use of the 256 concurrent object-access stripes allocated 256 native mutex
backings on this host, even after an empty concurrent cycle. Collection now
prepares all stripes once per heap before publishing Copying. Preparation stays
inside cycle timing; ordinary STW heaps leave unused locks cold. No preparation
check is added to the mutator access path.

The regression starts with cold locks, runs an actual empty concurrent cycle,
then checks repeated preparation and every stripe for zero allocator traffic.
It fails on old source with 256 allocations. Missing/partial preparation faults
also fail. An isolated probe using the exact production lock implementation
measures 256 allocations / 16,384 requested bytes for first preparation and zero
allocation/free traffic for later preparation plus all locks. This front-loads
cost rather than eliminating it: small first cycles may prepare unused stripes.
These are host Rust allocator observations, not RSS or total native accounting.

Final verification: **1025 workspace passes, zero failures, ten ignored**;
511 optimized runtime units plus nine doctests, armed detector, five optimized
enum/managed-memory integrations, 632 big-endian executions, 50 generated native
and 397 runtime ThreadSanitizer executions. Filters overlap. All 166 source pins
and archive entries match; probe-source and release compiler/runtime hashes
match. Diff check passes. Evidence:
`target/gc-validation/2026-10-18-copy-locks/report.json`.

No fresh application performance, active-copy throughput, pause reduction or
whole-runtime Miri claim is made. Native mutex backing remains outside registry-
buffer metrics. Total native accounting, generated/runtime concurrent integration,
embedding, broader platforms/soaks and numerical budgets remain open.

## Immutable reference-enum access audit — 2026-10-18

Reference-enum destructuring reads immutable payload bytes, including leading
reference slots and references restored into nested value payloads. Inspection
of `Heap::process_slot_concurrent` confirms that concurrent discovery only reads
parent slots and copies children; parent-slot rewrites are deferred to the final
stopped-world root and to-space passes. Source payload storage remains valid
until that pause. These immutable reads therefore do not establish a concurrent
payload-write race, provided the owning mutator follows rooting/safepoint rules.
This exception does not apply to mutable struct, array, Atom or aggregate slots.

A draft routing enum references through phase-checked scoped loads was removed:
its regression enforced an emission strategy without reproducing wrong behavior.
The added phase/lock overhead was not justified by the audited protocol. Compiler
source was restored only after an exact byte comparison with the preceding
archive. All 119 runtime/gate and 161 application source pins and build hashes
from the preceding passing checkpoint match again. Draft tests, fault controls,
sanitizer logs and timing observations remain in
`target/gc-validation/2026-10-18-enum-access/`, explicitly marked as superseded.
They are not claimed as validation of a production change.

Concurrent integration still requires the remaining mutable access, identity,
publication and embedding inventory. This audit closes only the proposed
immutable enum-read issue, not overall concurrent or production readiness.

## Long-execution native pressure adaptation — 2026-10-18

A released native-memory spike previously left an oversized collection trigger
until execution shutdown. The same-entry regression fails on the old code:
after freeing an 8 MiB FFI spike, 1.25 MiB of channel growth schedules no collection.
Allocation checks now lower obsolete headroom to the larger of 1 MiB and twice
the sum of current accounted demand and the pending request. They never raise
unserviced budgets; pending large replacements preserve serviced headroom.
Concurrent checks may conservatively remove headroom. Metrics remain unchanged.

Thirteen focused pressure tests pass, covering both heap modes, buffer/registry
ledgers, pending requests, retained observers and concurrent coalescing. Three
deliberate faults fail. Final verification: **1024 workspace passes, zero
failures, ten ignored**; 510 optimized runtime units plus nine doctests, armed
detector, 12 optimized integrations, 628 big-endian test executions, 50 generated
native and 396 runtime ThreadSanitizer executions. Platform/race filters overlap.
All 119 runtime/gate and 161 application source pins match, along with compiler,
runtime, saved binaries, baseline report and fixture hashes. Diff check passes.

All 480 original and 64 follow-up application runs are correct. Original cohort
median paired wall ratios span 0.916–1.065 and peak-RSS ratios 0.993–1.016. The
slowest original case (routes, one worker, 1 MiB nursery) has a later 31-pair wall
ratio of 0.993 and RSS ratio 1.000. Both series are retained; recorded host
activity prevents a causal speedup or zero-cost claim. No numerical performance
budgets or new whole-runtime Miri guarantee are established.
Evidence: `target/gc-validation/2026-10-18-live-pressure/report.json`.

Record/control and other metadata accounting, remaining embedding/concurrent
integration, broader platform/soak evidence and numerical budgets remain open.
Overall production readiness remains unfinished.

## Zeroed metadata allocation path — 2026-10-18

The metadata allocator now preserves the base allocator's zeroed-allocation
implementation instead of falling back to allocate-and-clear. Both allocation
paths share checked requested-byte accounting and cleanup on counter overflow.
Grow/shrink behavior is unchanged. No application speedup is claimed.

Eight focused allocator tests pass. The new regression checks base delegation,
alignment, zero contents, exact charges, overflow cleanup and allocation failure;
removing delegation makes it fail. Optimized runtime verification passes 508 unit
tests and nine doctests. The full workspace passes **1022 tests, zero failures**.
Source pins match; evidence is in
`target/gc-validation/2026-10-18-zeroed-metadata/report.json`.
Earlier platform/race/application measurements predate this change. Native record
accounting, within-execution pressure adaptation, concurrent-mode integration and
broader production-readiness work remain open.

## Global and handoff root storage accounting — 2026-10-18

Heap-owned global root words and handoff vacancy indices now use the requested-byte
registry ledger. Staging preserves accounting ownership; incompatible replacement
storage is rejected before any move. Charges include staged/obsolete buffers until
physical free. Global and handoff backing survives execution shutdown, then releases
with the heap. Teardown tests compare the exact retained-root total rather than
mistaking legitimate heap-lifetime backing for leaked execution tables.

Three new references cover full/generational handoff allocation traffic, retained
root backing, pressure participation, independent heap lifetime and foreign stages.
The strict probe initializes its cfg(test) hook mutex before measuring the two
production allocations. Four deliberate faults fail.

Final verification: **1021 workspace passes, zero failures, ten ignored**;
507 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 616 big-endian executions, 50 generated native and 393 runtime
ThreadSanitizer executions. Platform/race filters overlap. All 119 runtime/gate
and 161 application source pins match, along with measured binaries, compiler,
runtime library, baseline report and fixture hashes. Diff check passes. No new
whole-runtime Miri claim is made.
All 480 original application runs and 64 follow-up runs are correct. Original
cohort wall ratios span 0.904–1.089; peak-RSS ratios span 0.980–1.009. The slowest
cohort's isolated 31-pair follow-up has a median paired wall ratio of 1.0215 and
peak-RSS ratio 0.9994. Both observations are retained; neither establishes zero
cost or numerical budgets. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-root-storage/report.json`.

Record/control and other metadata accounting, remaining embedding/concurrent
audit, pressure coverage, broader platforms/soaks and numerical budgets remain
open. Overall production readiness remains unfinished.

## Permanent root publication and accounting — 2026-10-18

Permanent-source vector growth now publishes a pinned temporary source before
parking all originally RUNNING registrations. Replacement backing is allocated
outside collector/registry locks. An allocation-free commit transfers the source
to permanent publication under collector exclusion, avoiding duplicate enumeration.
Obsolete/unused backing is freed while callers remain BLOCKED, then registrations
restore together under exclusion. Spare-capacity publication stays RUNNING and
allocates nothing. Permanent-source backing now participates in registry accounting
through physical free at heap release, including staged/obsolete storage.

Four references cover commit/fast-path allocation behavior, stale/foreign stages,
exact root enumeration, moving roots during allocation/disposal, nested caller
restoration, backing accounting and snapshot history. Seven deliberate faults
fail. Two older tests now declare permanent-source storage before the heap to
honor the API lifetime contract. Frame publication and pointer reload requirements
are documented.

Final verification: **1018 workspace passes, zero failures, ten ignored**;
504 optimized runtime units plus nine docs, armed detector, 28 optimized
integrations including reflection, 604 big-endian executions, 50 generated native
and 390 runtime ThreadSanitizer executions. Platform/race filters overlap.
All 120 runtime/gate/changed-test and 161 application source pins match, along
with measured binaries, compiler, runtime library and baseline report hashes.
Diff check passes. No new whole-runtime Miri claim is made.
All 480 application runs are correct. Observed cohort wall ratios span
0.958–1.044; peak-RSS ratios span 0.992–1.012. These do not establish numerical
budgets or universal improvements. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-permanent-roots/report.json`.

Full record/control and other metadata accounting, remaining embedding/concurrent
audit, pressure-policy coverage, broader platforms/soaks and numerical budgets
remain open. Overall production readiness remains unfinished.

## Native pressure policy across executions — 2026-10-18

A reused runtime could retain a pressure threshold sized for a previous
execution's released native buffers. A real FFI-spike/re-entry regression fails
before the fix. Shutdown now lowers obsolete headroom after native retirement
drains, using current buffer/registry demand under the pressure-service mutex.
It never raises the trigger without a collection, preserving outstanding pressure
and old-epoch drain behavior. The minimum-threshold path avoids the service lock.
Collection counts and peak metrics retain their original meaning.

Three references cover full/generational re-entry after a released 8 MiB spike,
retained demand and unchanged metrics, and delayed physical release with a
competing collection. Five deliberate faults fail, including early retuning and
forgiving unserviced demand. The latter was caught by the existing closed-epoch
pressure regression; the policy was corrected rather than weakening that test.
Final verification: **1014 workspace passes, zero failures, ten ignored**;
500 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 588 big-endian executions, 50 generated native and 386 runtime
ThreadSanitizer executions. Platform/race filters overlap. All 117 runtime/gate
and 159 application source pins match, along with measured binaries, compiler,
runtime library and baseline report hashes. Diff check passes. No new whole-runtime
Miri claim is made.
All 480 paired application runs are correct. Observed cohort wall ratios span
0.919–1.042; peak-RSS ratios span 0.983–1.018. These do not establish numerical
budgets or universal speedups. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-pressure-epochs/report.json`.

Complete native accounting, remaining embedding/concurrent audit, pressure-policy
coverage within long executions, broader platforms/soaks and numerical budgets
remain open. Overall production readiness remains unfinished.

## Off-lock native registry teardown — 2026-10-18

Execution shutdown now prepares empty atomic/channel/completion registries with
the heap's existing accounting owner, detaches contents with allocation-free
swaps under brief collector exclusion, waiting for every late forwarding write.
Record retirement and backing release follow outside registry/coordinator locks.
Caller registrations stay BLOCKED through disposal and the native retirement
drain. Repeated finish and later runtime entry preserve accounting and prepare
fresh retirement epochs.

Three references cover detachment without allocation or freeing, waiting through
collector exclusion, and a paused disposal interval that permits another
collection while all registries are unlocked.
They verify retained backing charges, moving global roots, record reclamation,
repeated finish and re-entry in full/generational heaps. Four deliberate faults
fail: RUNNING disposal, locked disposal, lost replacement accounting ownership,
and missing collector exclusion.
Final validation: **1011 workspace passes, zero failures, ten ignored**;
497 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 576 big-endian executions, 50 generated native and 383 runtime
ThreadSanitizer executions. Platform/race filters overlap. All 117 runtime/gate
and 159 application source pins match, along with measured binaries, compiler,
runtime library and baseline report hashes. Diff check passes.
All 480 corrected-implementation application runs are correct; observed cohort
wall ratios span 0.933–1.039 and peak-RSS ratios 0.977–1.014. These do not establish
numerical budgets or universal speedups. See GC_PERFORMANCE.md for scope.
The superseded draft lacked collector exclusion and is separately archived with
relocation notes; its green narrow checks are not final lifetime evidence.
Final evidence: `target/gc-validation/2026-10-18-registry-teardown/report.json`.

Record/control accounting, other native metadata, remaining embedding/concurrent
coordination audit, wider platforms/soaks and numerical performance budgets
remain unfinished.

## Allocation-free scoped extra roots — 2026-10-18

Scoped root registrations now borrow pinned stack descriptors through an
intrusive list instead of allocating descriptors and vector backing. Source
lifetimes remain tied to registration guards. The list mutex excludes enumeration
and unlink; collector exclusion still protects borrowed slot addresses through
late parallel forwarding writes. Sources stay published during native maintenance.
Mutator removal retains its RUNNING protection. No metadata allocation or disposal
is needed during registration/removal, including under collector exclusion.

Four registry-core references cover arbitrary unlink/reuse, empty/unwind cleanup,
forwarded slot updates and cross-thread publication. An allocation probe verifies
1024 Heap registration/scan/removal cycles with zero allocations/frees. Four core
references pass both isolated Miri modes; this is not whole-runtime Miri.
Final validation: **1008 workspace passes, zero failures, ten ignored**;
494 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 564 big-endian executions, 50 generated native and 380 runtime
ThreadSanitizer executions. Platform/race filters overlap. Four isolated registry
references pass both Miri modes. Three deliberate faults fail. All 115 runtime/gate
and 157 application source pins match, along with measured binaries, compiler,
runtime library and baseline report hashes. Diff check passes.
All 480 paired application runs are correct. Observed cohort wall ratios span
0.920–1.034; peak-RSS ratios span 0.984–1.016. These do not establish performance
budgets or a universal speedup. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-scoped-roots/report.json`.

Native record/control accounting, other native metadata, teardown storage
coordination, the remaining concurrent audit, broader platforms/soaks and numerical
performance budgets remain unfinished.

## Registry-driven native pressure — 2026-10-18

Native pressure decisions and post-collection thresholds now combine buffer and
registry backing bytes. Buffer/registry metrics retain separate scopes. Atomic
construction and thread spawn service accumulated pressure; spawn publishes its
environment handoff root before collection. Channel and FFI allocations use the
same combined pressure calculation. Registry backing is charged at allocation;
the next native constructor services accumulated demand. This is a growth trigger,
not a hard memory limit.

Three new regressions cover registry-only atomic churn in full/generational heaps,
closure handoff across pressure GC, and combined-ledger threshold adaptation.
A compaction fixture now prepares accounted backing without invoking constructor
service before arming its maintenance hook. Record/control allocation accounting
and other native metadata remain open.

Final validation: **1003 workspace passes, zero failures, ten ignored**;
489 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 544 big-endian executions, 50 generated native and 375 runtime
ThreadSanitizer executions. Platform/race filters overlap. Three deliberate
faults fail. All 113 runtime/gate and 155 application source pins match, as do
measured binaries, compiler, library and baseline report hashes. Diff check passes.
All 480 paired application runs are correct; observed cohort wall ratios span
0.964–1.043 and peak-RSS ratios 0.994–1.006. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-registry-pressure/report.json`.

## Registry backing accounting — 2026-10-18

Atomic, channel and completion table/vector backing is now accounted at the
allocator boundary, through physical freeing and across staged growth/compaction.
Commits reject storage prepared for another heap's counter. Allocation failure,
overflow, retained backing, concurrent updates and heap-independent lifetime have
regression coverage. New registry metrics remain separate from buffer metrics.
Record/control allocations, other metadata and native-pressure integration remain
unfinished.

Final verification: **1000 workspace passes, zero failures, ten ignored**;
486 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 532 big-endian test executions, 50 generated native plus 372 runtime
ThreadSanitizer executions. Platform/race filters overlap. Seven isolated
allocator references pass both Miri modes; this is not whole-runtime Miri.
Four deliberate fault controls fail. All 112 runtime/gate and 155 application
source pins match; diff check passes. All 480 paired application runs are correct;
observed wall ratios span 0.956–1.055, peak-RSS ratios 0.996–1.037. These do not
establish production budgets. See GC_PERFORMANCE.md for scope and costs.
Evidence: `target/gc-validation/2026-10-18-registry-accounting/report.json`.

## Component-specific native admission — 2026-10-18

Atomic, channel and completion publication now requests backing only for exhausted
components. Commit checks per-component liveness budgets and preflights every
required component before moving ownership. It replaces only components still
needing growth, preserving other storage, native record identity and pending
order/positions. Owner reservation follows total records even when no raw handle
has bound yet. Allocation and old/unused storage disposal remain off-lock/BLOCKED.

Seventeen references cover individual components, untouched storage addresses,
allocation-free commits/publication/future binding, raw histories, incomplete
stages, independent stale budgets and pending order. Six deliberate controls fail.
Final verification: **986 workspace passes, zero failures, ten ignored**;
472 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, worker-one/four reference rechecks, 476 big-endian executions and
50 generated native plus 357 runtime ThreadSanitizer executions. Filters overlap;
the dedicated profile is ignored during ordinary tests. No new whole-runtime
Miri claim is made. All 106 source/gate pins, 150 benchmark source pins and final
executable/library hashes match; diff check passes.

The shared native profile measures production publication with prepared inputs,
excluding constructors/OS-thread creation/setup/teardown. It completes 126 paired
process executions and 1008 measured publications. At 32768 existing records,
raw-channel membership admission changes from about 1.69 ms to 0.69 µs, and
completion pending admission from about 0.83 ms to 0.27 µs. Atomic owner growth
allocates one backing vector rather than two tables. GC_PERFORMANCE.md retains
all cases, allocation bytes, scope and limitations. These are isolated admission
improvements, not whole-program speedups. All 480 application runs are correct;
median paired cohort wall ratios span 0.983–1.045. Evidence:
`target/gc-validation/2026-10-18-component-admission/report.json`.

Still required: full native metadata accounting/admission, remaining embedding
and native coordination (including scoped-root descriptors and teardown table
disposal), concurrent-mode audit, broader platform/soak coverage and numerical
startup/throughput/pause/tail/RSS budgets. Overall production readiness remains
unfinished.

## Off-pause registry compaction and terminal joins — 2026-10-18

Completion, atomic and channel weak sweeps now remove ownership without shrinking
native backing. Sparse replacement storage is allocated outside collector
exclusion and registry locks. Commits recheck current liveness, stale-request
budgets and future binding capacity, move entries without allocation, and release
obsolete/unused storage after unlock. Mutator callers remain BLOCKED through
maintenance and restore together under the coordinator. Commits still briefly
serialize with GC on registry mutexes.

Explicit extra-root sources remain published while maintenance allows another
collection; unregistering borrowed storage waits for collector exclusion and all
late root writes. Regressions pause allocation/disposal and run another GC across
major/minor, STW, concurrent, native-pressure and extra-root paths, checking roots
and nested registration states. Five deliberate faults fail.

Optimized tests exposed transient retention of already-joined completion records
when a native observer held their state mutex. A monotone release/acquire flag now
publishes full OS/TLS join completion separately from body-exit notices. Terminal
GC/reaper probes need no state mutex; active joins still defer. The deterministic
held-state regression is red before the fix, and the original churn case passes
20 optimized repetitions. Native observers retain their own record ownership.

Final verification: **969 workspace passes, zero failures, nine ignored**;
455 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, worker-one/four rechecks, 408 big-endian executions and 50 generated
native plus 335 runtime ThreadSanitizer executions. Filters overlap. All 102
source/gate pins and 146 benchmark source pins, libraries and measured binaries
match. A manifest correction excludes a growing log from executable hashes;
original metadata and all raw measurements remain retained. Diff check passes.

All 480 final paired application runs are correct; cohort wall ratios span
0.888–1.025. Identical-source native profiles separate stopped-world telemetry,
GC-call latency and full teardown. They retain small costs as well as gains;
GC_PERFORMANCE.md gives observations, scope and archived intermediate versions.
Measured heap/completion sizes remain 4496/128 bytes on this target. Evidence:
`target/gc-validation/2026-10-18-registry-compaction/report.json`.

Still required: full native metadata accounting/admission, independent component
admission work, remaining embedding/native coordination (including scoped-extra
descriptor allocation/retirement and teardown table disposal), concurrent-mode audit, broader platform/soak
coverage and numerical performance budgets. Overall production readiness is
unfinished.

## Deferred completion-record destruction — 2026-10-18

Completed native thread records now transfer through prepared typed links to the
existing retirement service. GC immediately removes registry/pending/catalogue
ownership, then the native worker destroys record and registration metadata
outside collector and registry locks. Shutdown drains completion jobs through the
same global activity used by buffers, cells and channel controls. Admission
prepares ownership while BLOCKED, before publication; test fixtures use their
owning heap's service so teardown verifies the real drain contract.

Two regressions cover zero allocation/deallocation during non-compacting transfer,
deferred physical destruction, retained scalar observation and another collection
while the native destructor is paused. The real collection check runs in
semi-space and generational subprocesses. Restoring inline destruction fails the
reference. Tests distinguish immediate registry removal from native destruction.

Final correctness verification: 963 workspace passes, zero failures, nine ignored;
449 optimized runtime units plus nine docs, armed detector and 12 optimized
integrations. Broad gates pass 388 big-endian executions and 50 generated native
plus 329 runtime ThreadSanitizer executions. After fixture-only corrections, final
completion coverage passes 20 big-endian references and 40 ThreadSanitizer tests.
Overlapping filters repeat cases. All 480 paired application runs are correct;
median paired cohort wall ratios span 0.954–1.085. All source and benchmark
artifact hashes match. Costs and measurement scope are in GC_PERFORMANCE.md.
Evidence is retained in
`target/gc-validation/2026-10-18-completion-retirement/`.

Still open: off-pause registry compaction, full native metadata accounting and
admission, remaining embedding coordination, concurrent-mode audit, broader
platform/soak coverage and numerical performance budgets. Completion-specific
performance costs are tracked separately. Overall production readiness remains
unfinished.

## Cross-epoch native pressure correction — 2026-10-18

Fixed a confirmed bug: drain ignored old published native work after a newer
worker was created. All epochs now share lazy activity. Publication enters
before queue visibility; completion leaves only after destruction and nested
publication. Pressure prepares synchronization before collector exclusion,
drains before deciding whether to collect, then retries if work was published
before its idle threshold snapshot. Waiting does not pin the collector.

The old regression fails before the fix. Heap tests cover major/minor modes,
another collection during an old-worker wait and avoiding a redundant pressure
collection. References verify nested/foreign jobs, first-epoch shared identity,
publication exclusion and zero prepared allocator traffic. Four deliberate faults
are detected. Exact activity code passes three isolated Miri references under
default/explicit tree-borrows; this is not whole-runtime Miri.

Verified: **961 workspace passes, zero failures, nine ignored**; 447 optimized
runtime units plus nine docs and armed detector; 12 optimized integrations;
384 big-endian executions; 50 generated native plus 327 runtime ThreadSanitizer
executions (some overlapping filters repeat cases). All 100 runtime/gate hashes,
143 final application source hashes, archives, profile binaries and activity
core match. Normal runtime release build has no new warnings.

All 480 baseline, 960 initial paired and 960 final paired application runs are
correct. Final cohort wall ratios span 0.945–1.051. Bulk collection profiles are
roughly within 4% of baseline; the largest complete-cleanup case is 1.109 in the
final run. Heap grows by 16 inline bytes and shared synchronization is lazy.
No universal speedup or quantitative budget is established; see GC_PERFORMANCE.md.

Completion records, registry compaction, full metadata accounting/admission,
remaining embedding coordination, concurrent-mode audit, platform/soak coverage
and numerical budgets remain open. Evidence:
`target/gc-validation/2026-10-18-retirement-epochs/report.json`.

## Typed channel control retirement — 2026-10-18

Atomic cells and channel controls now share typed native ownership links. GC
removes/publishes whole dead controls without allocating retirement nodes or
destroying synchronization/scan metadata and inline backing on the collector.
The native worker unlinks first and releases same-worker backing under existing
pending controller jobs, avoiding a second mailbox round. Foreign epochs retain
their own publication; native observers retain their original storage lifetime.
Collector channel snapshots drop before weak-owner sweeping.

Verified: **954 workspace passes, zero failures, nine ignored**; 440 optimized
runtime units plus nine docs and armed detector; 12 optimized integrations;
360 big-endian executions; 50 generated native plus 318 runtime ThreadSanitizer
executions (overlapping filters repeat some cases). Three isolated Miri references
pass under default and explicit tree-borrows; this is not whole-runtime Miri.
Four deliberate faults are detected. All 99 runtime/gate source hashes, 142 final
application source hashes, runtime archives, profiles and Miri cores match.

All 480 baseline, 960 queued-backing prototype paired and 960 final paired
application runs are correct. Final cohort wall ratios span 0.952–1.061.
Bulk one-slot collection ratios are 0.582/0.483 at 4,096/32,768 controls;
complete cleanup is 1.034/0.999. Large backing is 0.681 collection / 1.026
cleanup. Construction/close ratios are 1.043–1.079, and small-batch teardown
remains higher. Control payload grows 208→232 bytes. See GC_PERFORMANCE.md for
full measurement scope; no universal speedup or numerical budget is established.

Completion records, registry compaction, full metadata accounting/admission,
remaining embedding coordination, concurrent-mode audit, platform/soak coverage
and budgets remain open. Cross-epoch native-pressure drain coverage also needs
a regression before it can be closed. Evidence:
`target/gc-validation/2026-10-18-channel-retirement/report.json`.

## Deferred atomic record destruction — 2026-10-18

GC removes dead atomic records and publishes their prelinked native ownership
batches without allocating retirement nodes or destroying cells inline. The
existing native retirement worker drains both scalar records and channel backing.
One prepared producer source per worker epoch covers cancellation, queued cells,
late old-epoch handoff and shutdown. Stable leading AtomicI64 addresses preserve
the raw ABI. A paused destruction test proves another collection can complete;
Weak tests distinguish registry removal from final native release.

Verified: **949 workspace passes, zero failures, nine ignored**; 435 optimized
runtime units plus nine docs and armed detector; 12 optimized integrations;
344 big-endian executions; 50 generated native plus 310 full-suite runtime
ThreadSanitizer executions and one updated owner-test recheck. Exact-core
isolated Miri passes under default/tree-borrows; this is not whole-runtime Miri.
Two deliberate faults are detected. All 97 runtime/gate source hashes and all
140 final application source hashes match. The broad gate snapshot differs only
in the internal async-drain test, covered by full optimized runtime and targeted
TSAN rechecks; production libraries and profile binaries are unchanged.

All 480 baseline, 960 initial paired, 64 focused follow-up and 960 final paired
application runs are correct. Bulk atomic collection paired ratios are
0.839/0.775 at 4,096/32,768 cells, with higher constructor/storage/teardown costs.
Record allocation requests grow from 24 to 48 bytes. Final application cohort
ratios span 0.862–1.067. The initial CSV ratio 1.155 is retained; its 31-pair
follow-up is 0.976. See GC_PERFORMANCE.md for scope and all small-workload costs;
no universal speedup or quantitative budget is established.

Completion/channel control destruction, registry compaction, full native metadata
accounting/admission, remaining embedding coordination, concurrent-mode audit,
platform/soak coverage and numerical budgets remain open. Evidence:
`target/gc-validation/2026-10-18-atomic-retirement/report.json`.

## Allocation-free collection caller parking — 2026-10-18

Collection coordinator entries now suspend current RUNNING registrations using
an ownership chain in existing thread records. Parking, nested parking and
restoration have zero allocator traffic at 0/1/17/257 registrations. Prior
BLOCKED scopes remain blocked; nested newly admitted registrations and unwind
restore correctly. Pending major/minor collections with one/four workers
relocate every sole scratch root across 17 owners and preserve payloads.

Verified: **943 workspace passes, zero failures, nine ignored**; 429 optimized
runtime unit passes plus nine documentation passes and the armed detector;
12 optimized integrations; 324 big-endian executions; 50 generated native plus
304 runtime ThreadSanitizer executions. All 95 runtime/gate source hashes and
four prior compiler source hashes match. The original allocator regression is
red before the fix. On this host ThreadState grows from 528 to 536 bytes; no
universal wall-time speedup or numerical pause guarantee is claimed.

GC-time native record destruction/registry shrinking, complete native metadata
accounting, remaining runtime/embedding coordination, concurrent-mode access
integration, platform/soak coverage and quantitative performance budgets remain
open. Evidence: `target/gc-validation/2026-10-18-caller-parking/report.json`.

## Compiler diagnostic robustness — 2026-10-18

Malformed Unicode tokens/escapes now consume complete UTF-8 scalars. Escape
parsing stops at EOF and requires a closing Unicode brace. Lexer errors preserve
the requested source ID. Diagnostic rendering normalizes synthesized byte spans
before slicing, and user EOF spans remain in the user file. Regression coverage
includes actual CLI exit status, filenames/carets, valid Unicode literals, and
12,000 deterministic parser fuzz inputs whose failures now exercise rendering.

Workspace verification passes **940 tests, zero failures, nine ignored**.
The optimized compiler library and CLI/fuzzer tests also pass. Logs are in
`target/gc-validation/2026-10-18-lexer-diagnostics/`.
The CLI's legacy offset-based prelude fallback still needs full SourceMap routing,
including retention of source maps on parse failure. GC record/registry cleanup,
native metadata accounting and the other readiness work below remain open.


## Cooperative handoff storage and reserved return capacity — 2026-10-18

Handoff admission now roots its environment in an inline frame before native
preparation parks the parent. Root/pool backing is allocated BLOCKED off-lock;
RUNNING commit preserves root words and indices without allocation. Obsolete
buffers are disposed BLOCKED before a new handoff is published. Capacity is
rechecked after GC or competing admission. Every allocated handoff slot reserves
its future return, so child transfer and failed-admission return cannot grow the
pool. Mutex backends are prepared before publication; reuse stays allocation-free.

References verify transfer/return/reuse allocator traffic, indices/values,
insufficient/obsolete stages and warm mutexes. Native tests use the temporary
frame as the sole environment root through major/minor GC, check its changed
address/value and exact counters, force undersized-stage retry, and exercise
real old-buffer disposal. Eight deliberate faults are detected, including
RUNNING preparation/disposal, missing rooting, stale reload, missing return
capacity, reversed words and unprepared mutexes.

Final gates: **935 workspace passes, zero failures**; 426 optimized runtime
unit passes, nine runtime documentation passes and armed detector; 12 optimized
integrations; 316 big-endian executions; 50 generated native plus 301 runtime
ThreadSanitizer executions. Compact-enum workspace stress finishes in 31.23
seconds under its unchanged deadline. All 94 runtime/gate hashes match.

The baseline snapshot matches all 89 runtime Rust files from the previous
verified checkpoint and passes 480 application runs. The final same-compiler
comparison passes all 960 runs. Paired application cohort wall ratios range
0.955–1.070; actual spawn/bind ratios are 0.980/1.034 at 32/256 threads. These
observations establish neither universal speedups nor performance budgets.
Source sets, runtime archives and profile hashes match their evidence.

Remaining: off-pause record/registry cleanup, full native metadata
accounting/admission, other native/runtime and embedding allocation coordination,
broader platform/soak coverage and quantitative performance budgets. Overall
production readiness remains unfinished. Evidence:
`target/gc-validation/2026-10-18-handoff-storage/report.json`.

## Handoff root retirement excludes late forwarding stores — 2026-10-18

Failed or uncalled child cleanup previously cleared its handoff root under the
root backing mutex. Parallel evacuation retains root addresses after enumeration
releases that mutex, so a later forwarding store could overwrite zero and
resurrect the retired root. The new reference reproduces this ordering and
fails before repair.

Cleanup now parks any current entry, waits for collection-coordinator exclusion,
clears the exclusively owned root, then publishes vacancy. Unregistered cleanup
does not inspect thread membership, preserving failed-admission cleanup when
that registry is poisoned. The root API contract now explicitly covers zero
stores and retained evacuation addresses. Successful rooted child transfer uses
its existing RUNNING/scratch protocol.

A real pending-GC test checks major/minor progress, exact counters, witness data,
final zero and vacancy. Three controls detect missing exclusion, exclusion taken
after clearing, and waiting while RUNNING. Failed admission, failed setup and
repeated reuse remain covered.

Final validation: **931 workspace passes, zero failures**; 422 optimized
runtime unit passes, nine runtime documentation passes and armed detector;
12 optimized integrations; 304 big-endian executions; 50 generated native plus
297 runtime ThreadSanitizer executions. Compact-enum workspace stress completes
in 34.12 seconds under its unchanged deadline. All 93 runtime/gate source hashes
match the validated checkpoint.

Still open: handoff/global-root storage growth while RUNNING, off-pause metadata
cleanup, full native metadata accounting/admission, broader platform/soak
coverage and quantitative performance budgets. Overall production readiness
remains unfinished. Evidence: `target/gc-validation/2026-10-18-handoff-root/report.json`.

## Prepared reaper catalogue and bounded probe cycles — 2026-10-18

Replaced catalogue B-tree allocation/removal under the monitor mutex with one
prepared weak node per native completion. Admission allocates before publication
while BLOCKED. Enqueue, duplicate enqueue, removal and weak batches update
private links under the existing monitor without allocating or destroying nodes.
Record retirement uses its owning monitor, including older shutdown epochs.
Shutdown closes/detaches the catalogue and clears links iteratively off-lock.

A new regression reproduces a progress issue: continuous appends could extend a
pass indefinitely and postpone older deferred records. Passes now snapshot the
starting queue length as a visit budget; removal advances the cursor and cannot
extend that budget. Each batch remains limited to 64 weak records. Six fault
controls detect allocation, lost cursor repair, strong retention, unbounded
cycles and incorrect epoch ownership. The link protocol is documented in
[SAFETY.md](SAFETY.md); Miri uses exact sources with unrelated joining stubbed.

Final validation: **929 workspace passes, zero failures**; 420 optimized
runtime unit passes, nine runtime documentation passes and armed detector;
12 optimized integrations; 300 big-endian executions; 50 generated native plus
295 runtime ThreadSanitizer executions; four isolated Miri references in each
of the default and explicit tree-borrow configurations. Compact-enum workspace
stress completes in 37.62 seconds under its unchanged deadline. Source sets,
profile sources/binaries and exact Miri source copies match the checkpoint.

All 960 final application executions pass. Cohort paired median wall ratios
range 0.917–1.045. Native enqueue/remove churn ratios are 0.588/0.343 at 64/4096
records; isolated scans are slower at 1.226/1.165. Actual spawn/bind ratios are
1.019/0.995. These observations do not establish universal speedups or budgets.
The node requests one 48-byte allocation; native records grow 72→96 bytes on
the measured macOS build. Rejected node-mutex and pre-budget variants remain in
evidence. See [performance measurements](GC_PERFORMANCE.md).

Still open: native handoff/global-root growth, off-pause record/registry cleanup,
complete native metadata accounting/admission, broader platform/soak coverage
and quantitative performance budgets. Overall production readiness remains
unfinished. Evidence: `target/gc-validation/2026-10-18-reaper-catalogue/report.json`.

## Allocation-free reaper probe batches — 2026-10-18

The completion worker previously allocated each ordered probe batch under its
collector-visible monitor mutex. Fixed stack storage now holds at most 64 weak
records; cursor ordering, weak ownership, backoff and joins outside locks remain
intact. No unsafe storage or shared scratch cache is introduced.

The new allocation-probed reference fails on the previous implementation and
passes for catalogue boundaries, exclusive cursors, full scans and expired weak
records. Four deliberate fault controls detect allocation, inclusive cursors,
wrong weak records and an increased work limit. Existing reaper tests pass with
one/four GC workers, including publication races, backlog draining and TLS exit.

Final validation: **926 workspace passes, zero failures**; 417 optimized
runtime unit passes, nine runtime documentation passes and the armed corruption
detector; 12 optimized integrations; 288 big-endian executions; 50 generated
native plus 292 runtime ThreadSanitizer executions. Compact-enum workspace
stress completes in 32.73 seconds under its unchanged deadline. Source, harness
and binary hashes match the measured checkpoint.

All 960 application executions validate their results. Application cohort
paired median wall ratios range 0.948–1.071, with no end-to-end speedup claim.
Isolated catalogue scans show ratios 0.741/0.705 at 64/4096 records over 1024
scans and seven alternating pairs; these exclude joins, worker scheduling and
GC pauses. See [performance evidence](GC_PERFORMANCE.md).

Still open: catalogue insertion/removal and shutdown destruction under the
monitor mutex; native handoff/global-root growth; coordinated off-pause
shrinking/destruction; complete native metadata accounting/admission; broader
workload/platform/soak coverage and quantitative performance budgets. Overall
production readiness remains unfinished.
Evidence: `target/gc-validation/2026-10-18-reaper-batches/report.json`.

## Cooperative thread completion admission — 2026-10-18

Thread startup now prepares native resources in BLOCKED regions outside the
completion-registry mutex. Reserved tables make publication and late owner
binding allocation-free; dense pending keys track their positions through GC
removal and shutdown. Staged migration preserves pending positions and reaper
identity, retries insufficient capacity, and rejects historical stages after GC
shrinks live records. Native mutexes are prepared before publication.

Five new tests cover storage models and major/minor progress through seven
native phases. Nine deliberate fault controls fail as expected. Final gates:
**925 workspace passes, zero failures**, 416 optimized runtime unit passes,
nine runtime documentation passes plus the armed corruption detector,
12 optimized integration passes, 284 big-endian executions, and 50 generated
native plus 291 runtime ThreadSanitizer executions. Compact-enum workspace
stress completes in 33.99 seconds under its unchanged deadline. Source and
profile-binary hashes match the validated checkpoint.

All 960 final application executions validate their results. Paired median
application wall-time ratios range 0.965–1.053 across cohorts. Fresh-context
thread admission itself has a measured cost: final paired ratios 1.057/1.050 at
32/256 threads; earlier samples vary. See [performance evidence](GC_PERFORMANCE.md).

Still open: handoff/global-root native growth, reaper catalogue allocation under
its monitor mutex, coordinated off-pause shrinking/destruction, complete native
metadata accounting/admission and broader platform/soak/performance budgets.
Overall production readiness remains unfinished.
Evidence: `target/gc-validation/2026-10-18-completion-registry/report.json`.

## Cooperative atomic registration and stale-stage replanning — 2026-10-18

Native atomic-cell construction previously allocated while RUNNING and could
grow registry tables under a collector-visible lock. Cell allocation and table
staging/disposal now run BLOCKED and off-lock. Publication rechecks current
capacity after collection or concurrent registration and moves live entries
without cloning cells. Heap initialization prepares the registry mutex backend.
The compiler already treats the constructor as relocating; scalar atomic
load/store/CAS/fetch-add remain nonrelocating and keep their existing paths.

Binding reserves space for every unbound record and preserves that reservation
through collection, so a newly allocated wrapper can bind without allocating or
parking. The owner index is a dense Vec: binding is one-shot and sweeping is the
only removal. Collection visits bound owners independently of unused capacity
reserved for raw execution-owned cells. This adds reserved key storage (eight
bytes per reserved key on these 64-bit targets); full native metadata accounting
and admission remain open. Burst retention stays bounded by live records, and
shutdown releases the backing.

An additional race was reproduced in both atomic and existing channel growth:
GC could retire records during staging, after which an oversized historical-peak
replacement was installed. Both new regressions fail before repair. Shared
`RegistryStage` retains the allocation request separately from rounded container
capacity. Install rejects requests above twice the current next-entry growth
target; existing BLOCKED off-lock paths discard/replan them. Insufficient stages
still retry after competing registration. Ordinary commit stays allocation-free.

New references check mutex preparation, zero allocator traffic during transfer
and late binding after sparse maintenance, and oversized-stage rejection after
retiring 257 owners. Native-phase tests force major/minor collection during cell
allocation, staging, disposal and a stale-stage retry; they check exact counters,
moved witness roots, native values and late binding. Minor retry setup supplies
fresh young work rather than counting an empty-nursery no-op. Seven deliberate
fault controls fail as intended, including RUNNING allocation, lock-held staging,
missing reservation, cloned live cells and historical-stage acceptance.

The reproducible `atomic_registration_profile` measures fresh native raw-cell
constructor loops, excluding RuntimeContext setup/shutdown and validating every
cell value outside timing. Seven alternating pairs at 1K/32K cells, sixteen
measured rounds after warmup, give paired ratios 1.718/1.818. Median loop times
are 0.0545→0.0940 ms and 1.726→3.111 ms: about 39–42 ns additional per raw cell.
This is a throughput cost for cooperative allocation, not a constructor speedup;
it excludes managed-wrapper construction/binding. No hot scalar-operation path
changed. A final 960-run native application comparison passes all independent
output/telemetry checks; cohort paired wall ratios are 0.960–1.041 (automatic),
0.978–1.048 (one) and 0.960–1.049 (four). Fresh-process wall time includes telemetry
and the time wrapper with warm filesystem caches. No universal budget is claimed.

Final validation: 920 workspace passes, zero failures and nine ignores; optimized
runtime 411 unit tests and nine doctests plus the armed bad-header detector;
twelve selected optimized integrations; 268 s390x checks; 50 generated-program
and 286 runtime ThreadSanitizer executions without reports (some filter overlap).
Compact-enum stress passes in 31.84 seconds with unchanged deadlines. All 134
profiled source hashes and both constructor-profile binary hashes match after
validation.

Reproduce the constructor profile with:

```sh
cargo build --release -p gcrust-rt --example atomic_registration_profile
GCR_GC_VERIFY=1 GCR_GC_POISON=1 target/release/examples/atomic_registration_profile 32768 16
```

Evidence: `target/gc-validation/2026-10-18-atomic-registry/report.json`. Earlier
measurements and gate logs before the stale-budget repair remain as intermediate
evidence. Completion-registry growth, coordinated off-pause shrinking/destruction,
constructor-cost optimization, full native accounting/admission, platform/soak
coverage and numerical startup/throughput/pause/tail/RSS budgets remain open.

## Resolve completed copy retries before shared publication — 2026-10-18

A BUSY-slot retry can become redundant as soon as its worker publishes the
claimed object. Returning those retries to the shared frontier needlessly wakes
helpers and breaks bounded local object continuations. After publication, the
collector now rewrites slots whose source forwarding header is complete and
removes those finished tasks. BUSY and unclaimed sources remain queued; no worker
waits on a copy owner. Already-updated and non-pointer slots need no further
visit, matching the ordinary slot visitor. Object/channel tasks preserve their
order. Both batch completion and streamed publication use this path. Atomic
claims, shared queue, active-worker accounting and notification predicates stay
unchanged.

The new `copy_graph_profile` example checks every node/edge/payload, cycle or tree
shape, exact live bytes and collection count outside timed GC. It covers one-root
cyclic and alias-free tree graphs at 1K/32K/262K nodes, automatic/one/four workers,
sixteen timed collections after warmup. A final 252-process checked comparison
uses seven alternating pairs per configuration; 180 normal-mode processes use
five pairs with verification/poisoning disabled. Both variants use identical
example/compiler options and preserved runtime inputs.

At 262K cyclic nodes, checked automatic copying improves from 45.52 to 7.22 ms
(median paired ratio 0.157); normal mode improves from 49.72 to 7.50 ms (0.151).
Checked forced-one/four ratios are 0.851/0.301. Alias-free tree controls are
mixed: checked 0.970–1.032, normal 0.969–1.024. These measurements do not establish
universal latency or memory budgets.

A 588-process root-enrollment matrix includes repeated registrations and
small/large sets. Its largest small-set forced-four ratio, 1.155, was followed
by a 225-process three-way repeat covering five configurations: candidate ratios
0.980–1.018, identical-baseline controls 0.988–1.050. Root timings include payload
validation, unlike the graph profile's GC-only timer. All 960 application runs
pass independent output/telemetry checks; cohort paired wall ratios range
0.951–1.053 (automatic), 0.950–1.033 (one) and 0.949–1.039 (four). Application wall
times include telemetry/time-wrapper costs and use warm filesystem caches.

Three new references pass under forced one/four workers: BUSY/unclaimed/forwarded
slots and task ordering, uncollected-generation boundaries, and Full/Compact
streamed alias graphs across nursery promotion and repeated major collections.
Six restored fault controls fail as intended. Resolution itself allocates
nothing. Inline-queue and store-only-claim prototypes were rejected after
measurements; their source/results remain in separate evidence directories.

Final validation: 915 workspace passes, zero failures and nine ignores; optimized
runtime 406 unit tests and nine doctests plus the armed bad-header detector;
twelve selected optimized integrations; 252 s390x checks; 50 generated-program
and 280 runtime ThreadSanitizer executions without reports. Compact-enum stress
passes in 32.01 seconds with unchanged deadlines. All 131 profiled source hashes
match after validation, and final graph binary hashes match both primary profiles.

Reproduce a checked graph run with:

```sh
cargo build --release -p gcrust-rt --example copy_graph_profile
GCR_GC_VERIFY=1 GCR_GC_POISON=1 target/release/examples/copy_graph_profile 262144 16 cycle
GCR_GC_WORKERS=4 target/release/examples/copy_graph_profile 262144 16 tree
```

Evidence: `target/gc-validation/2026-10-18-resolved-retries/report.json`.
Shared-queue allocation, off-pause registry maintenance/shrinking, native metadata
accounting/admission, additional platforms and long soaks, and numerical
startup/throughput/pause/tail/RSS budgets remain open. The extra retry-list pass on
alias-free work remains a possible optimization target.

## Reserve known global-root cardinality — 2026-10-18

Root-heavy minor collections exposed repeated hash-table growth during root
address enrollment. Global backing slots are distinct and always enumerated, so
the collector now reserves their known count before inserting addresses. Up to
sixteen slots retain the allocation-free inline path. Additional root sources
can still grow normally; repeated registrations deduplicate as before. Storage
remains job-local and is released after collection rather than retained in a new
cache. Automatic worker selection is unchanged: a 100-process policy sweep
found that adding workers did not improve this many-short-root-tasks workload.

The new `root_work_profile` runtime example aliases one fresh young leaf through
many distinct global slots. Every round checks all rewritten slots, payload,
exact promoted bytes, empty nursery and minor collection count. Only collection
is timed; initialization and validation are outside the samples. A final
168-process alternating comparison covers 16/4K/64K/1M slots, automatic/one/four
workers and seven pairs, sixteen timed collections after warmup. With verification
and poisoning enabled, automatic 1M-root collections improve from 54.95 to
23.89 ms (median paired ratio 0.435); 64K roots from 3.06 to 1.27 ms (0.413).
A further 120-process comparison disables verification and poisoning: automatic
1M-root collections improve from 52.93 to 22.22 ms (paired ratio 0.421), and 64K
from 3.00 to 1.23 ms (0.408). A 126-process small-set repeat uses 64 timed
collections per process and an identical-baseline control: candidate ratios
0.987/0.968 (automatic/one), sham ratios 1.001/1.001. The original shorter small
sample's 1.132 automatic ratio is retained in the evidence.

All 960 paired native application executions pass output and telemetry checks.
Cohort median wall ratios range 0.927–1.035 (automatic), 0.964–1.025 (one) and
0.924–1.134 (four). The 1.134 forced-four CSV outlier was repeated in a 63-process
three-way control: candidate/baseline 0.998 and identical-baseline/baseline 0.975,
with individual candidate ratios 0.797–1.241. These measurements establish no
universal latency or memory budget. Native application wall times include the
telemetry/time wrapper and use warm filesystem caches.

Five root-slot references pass at forced one/four workers. Allocation checks
prove one initial allocation and no enrollment growth for known large counts,
zero allocation for small counts, and preservation of additional/repeated root
sources. Two deliberate faults (omitted reservation and allocating on the inline
path) fail these checks. Evidence:
`target/gc-validation/2026-10-18-root-work/`.

Final validation: 912 workspace passes, zero failures and nine ignores; optimized
runtime 403 unit tests and nine doctests plus the armed bad-header detector;
twelve selected optimized integrations; 240 s390x checks; 50 generated-program
and 277 runtime ThreadSanitizer executions without reports. Compact-enum stress
passes in 32.23 seconds with unchanged deadlines. All 128 profiled source hashes
match after validation.

Reproduce an individual checked profile with:

```sh
cargo build --release -p gcrust-rt --example root_work_profile
GCR_GC_VERIFY=1 GCR_GC_POISON=1 target/release/examples/root_work_profile 65536 16
```

Set `GCR_GC_WORKERS=1` or `4` for explicit-policy controls. Root deduplication is
still serial; other root shapes, native-edge-heavy work, remaining collector
allocations, native metadata accounting, platform coverage and numerical
performance budgets remain open.

## Automatic workers account for remembered-object scans — 2026-10-17

Minor collections previously sized automatic parallelism from nursery occupancy
alone, even when scanning a large dirty old graph. Worker selection now adds the
traced bytes of each distinct remembered object: fixed references, interior
pointers and variable value tails. Raw byte tails contribute nothing; arithmetic
saturates. The existing card-overlap traversal and object deduplication supply
the estimate without another heap walk. Explicit worker overrides and automatic
operation on one CPU skip counting altogether. Major collection sizing is
unchanged.

The reproducible `remembered_work_profile` runtime example checks every old node,
shared promoted leaf, raw payload, remaining field and collection count. A
75-process policy sweep selected representative workloads, followed by a final
126-process comparison with seven alternating pairs per layout/policy and sixteen
measured collections after warmup. For automatic workers, 4 MiB of remembered
references improves from 4.83 to 4.00 ms (median paired ratio 0.833), and 32 MiB
from 25.74 to 11.50 ms (0.451). Small scans have ratio 1.030. Forced-worker controls
are mixed, including 1.071 for the large four-worker case; these observations
establish no universal timing budget. Earlier implementations and measurements
are retained alongside the final candidate.

All 960 paired application executions pass independent output and telemetry
checks against the previous source-occupancy policy. Median paired wall ratios
across application/nursery cohorts range 0.917–1.052 for automatic workers,
0.931–1.055 for forced one and 0.934–1.030 for forced four. Fresh-process timings
include telemetry and the external time wrapper, with warm filesystem caches;
these ranges include host/run variation.

Six policy references pass under automatic and forced one/four workers. Eight
fault controls detect ignored remembered work, duplicate-card counting, omitted
fixed/interior/value-tail references, raw-byte counting, arithmetic wrapping and
unnecessary estimation for policies that do not use it. Evidence:
`target/gc-validation/2026-10-17-remembered-work/report.json`.

Final validation: 911 workspace passes, zero failures and nine ignores; optimized
runtime 402 unit tests and nine doctests plus the armed bad-header detector;
twelve selected optimized integrations; 236 s390x checks; 50 generated-program
and 276 runtime ThreadSanitizer executions without reports. Compact-enum stress
passes in 32.11 seconds with unchanged deadlines. All 127 profiled source hashes
match after validation.

Root/native-edge-heavy workloads, additional platforms and long runs, numerical
latency/RSS budgets, remaining collector allocation work and native metadata
accounting remain open.

Reproduce a single correctness-checked profile with:

```sh
cargo build --release -p gcrust-rt --example remembered_work_profile
GCR_GC_VERIFY=1 GCR_GC_POISON=1 target/release/examples/remembered_work_profile 32768 16 16
```

Set `GCR_GC_WORKERS=1` or `4` for explicit-policy controls. The example reports
only timed collection samples after warmup; initialization and graph validation
are outside those samples. The saved `profile.py` orchestrates the alternating
baseline/candidate comparison using preserved binaries.

## Automatic worker sizing from application evidence — 2026-10-17

A native application baseline exposed excessive automatic GC parallelism: a
1 MiB nursery selected up to eight participants above the former 256 KiB cutoff.
Automatic selection now uses one participant per complete 2 MiB of collected
source, with at least one, capped by available CPUs/eight. Explicit
`GCR_GC_WORKERS` overrides retain their previous path.

The new `scripts/benchmark_apps.py` builds CSV aggregation, scheduling and routing
apps and validates every output against independent models. It preserves native
binaries, source/input archives and hashes, build times, raw telemetry/output,
per-process OS peak RSS and host context. Nonzero exits fail; timeout cleanup
terminates/reaps its process group. Trials rotate configurations and alternate
baseline/current order for each configuration, with a tested parity invariant.
Fresh-process wall time includes telemetry and the external time wrapper, under
warm filesystem caches. Runtime pause estimates remain per-run estimates.

The main baseline passed 240 executions; paired baseline/current measurement
passed 960 across automatic/1/4 GC workers and 1/4/8/16 MiB nurseries. Workloads:
48,000 CSV rows, 1,600 scheduling tasks and a 512-node/3,511-edge graph. At 1 MiB,
automatic CSV paired wall ratios are 0.644/0.625 (one/four application workers),
with median peak RSS about 0.55/0.58 MiB lower. At 4 MiB, ratios are 0.836/0.752;
at 8 MiB, 0.936/0.888. At 16 MiB they are 0.995/0.983. Explicit-worker controls
range 0.915–1.179; timings include run-to-run variation and establish no universal
budget. The source-occupancy heuristic still needs root/remembered-set-heavy and
other workload coverage.

Three policy references pass at forced one/four workers, including actual helper
pool initialization for explicit overrides; six fault controls fail as intended.
Eight harness tests pass. The post-profile source delta adds only a cfg(test)
reference; production bodies were verified identical. Compiler/runtime inputs
are frozen under the evidence directory's `tools/`. Final validation: 908 workspace passes, zero failures, nine ignores; optimized
runtime 399 units and nine doctests plus the armed detector; twelve selected
optimized integrations; 224 s390x checks; 50 native and 273 runtime
ThreadSanitizer executions without reports. Compact-enum stress passed in 35.37
seconds with unchanged deadlines. Eight Python harness tests pass. Source hashes
and the test-only post-profile delta are verified.

The explicit-four-worker CSV outlier (1.179) was checked in a separate 63-run
three-way control: median current/baseline 1.035, identical-baseline/baseline
1.013, with large individual spreads. This does not establish tail guarantees.
Evidence: `target/gc-validation/2026-10-17-auto-workers/report.json`.

## Inline copy claims — 2026-10-17

Evacuation keeps up to eight pending heap-borrowed claims in a 480-byte job-local
container. Extra claims reserve the previous 28 KiB vector once. The prefix stays
in place; inline and spilled segments drain directly into the copy loop. Claims
never enter reusable worker storage. Published headers retain their forwarding
state; partially consumed drains and unwinding restore every unpublished header.
No new unsafe operations or runtime dependencies were introduced.

Six references pass at one/four workers with poisoning and verification, plus
29 focused parallel collector tests (two existing ignores). Tests cover ordering,
repeat drains, direct drop, partial/unwind rollback, zero allocator traffic for
eight claims, and one full spill reservation reused within a job. Seven restored
fault controls fail, including wrong spill-address offset in moving collection.

Earlier combined-drain and prefix-migrating prototypes regressed spill-boundary
timing and were replaced. Small-root profiles now use 4,096 collections; large
and channel cases use 64, identically for baseline/candidate. Two final segmented
700-process matrices pass payload/live-size/count checks. The first has no ratio
above 1.10; confirmation has a forced-four-worker outlier at 1.417. A focused
21-pair three-way control gives median candidate/baseline 0.966 and identical-
baseline/baseline 0.996, with substantial spread in both. These results do not
establish a stable latency budget or prove the cause of outliers. Earlier samples
and source remain available. Concurrent CPU-intensive work was also observed.

Final validation: 905 workspace passes, zero failures, nine ignores; optimized
runtime 396 units and nine doctests plus the armed detector; twelve selected
optimized integrations; 212 s390x checks; 50 native and 270 runtime
ThreadSanitizer executions without reports. Workspace compact-enum stress passed
in 35.20 seconds with unchanged deadlines. Source hashes verified. Evidence:
`target/gc-validation/2026-10-17-inline-claims/`. Larger jobs still allocate claim
backing. Shared queues, other collector allocation, registry shrinking, metadata
accounting/admission and application pause/RSS/tail budgets remain open.

## Reusable worker metadata — 2026-10-17

Four evacuation vectors (task batch, discovered work, allocation layouts and
reserved addresses) now use a thread-local lease. Every return/unwind clears
logical contents. Heap-borrowed copy claims remain scoped to the current job.
Idle participants initialize only their task batch. Nested collections and
collections after cache TLS destruction use uncached local storage.

Eight one/four-worker poisoned/verified references cover exact cold requests and
zero warm allocator traffic, unwind cleanup, thread isolation, nested leases,
actual nested pointer-policy collection, actual TLS-teardown collection and
cross-heap Full/Compact reuse. Seven restored fault controls fail as intended.
On this target retained Rust buffer requests total 22 KiB per active OS thread,
or 2 KiB for a thread that remains idle; TLS headers and OS costs are additional.
Caches die with their OS thread; coordinator
storage may outlive individual heaps. Native metadata accounting remains open.

Two 644-process matrices use identical baseline/candidate workloads and check
payloads, live sizes and collection counts. First paired ratios: roots
0.780–1.122, channels 0.823–1.200. Repeat: roots 0.796–1.056, channels
0.842–1.114. The first matrix's two >1.15 channel outliers became 1.019/1.028
on repeat. Both sets are retained; local timing does not establish a universal
budget. Copy-claim, shared-queue and other collector allocations remain.

Final validation: 899 workspace passes, zero failures, nine ignores; optimized
runtime 390 units and nine doctests plus the armed detector; twelve selected
optimized integrations; 188 s390x checks; 50 native and 264 runtime
ThreadSanitizer executions without reports. Workspace compact-enum stress passed
in 31.23 seconds with unchanged deadlines. Final source hashes verified. Evidence:
`target/gc-validation/2026-10-17-worker-scratch/`.

## Inline root enrollment — 2026-10-17

Parallel evacuation now keeps up to sixteen unique writable root addresses
inline before spilling once into its existing hash set. This removes the
root-deduplication table allocation for small sets; it does not remove the shared
work queue or worker buffers. Distinct slots holding the same object remain
distinct work items. Repeated registration retains storage proportional to
unique slots rather than total visits.

Four references cover exact address sets and iterator hints, zero inline Rust
allocator traffic, no further allocation for repeated spilled addresses, and
Full/Compact alias-slot relocation around the spill boundary. They pass with
one/four workers, verification and poisoning. Six restored fault controls fail
as intended.

644 final profile processes check moving payloads, exact live size and collection
counts, using identical baseline/candidate workloads. Small-root paired ratios
range 0.544–1.049, large/repeated-root 0.961–1.022, channel controls 0.877–1.105.
These are local results, not universal budgets. An initial non-inlined candidate
regressed large sets and was corrected; a sorted-vector prototype was rejected
for visit-proportional storage and duplicate-heavy slowdown. Evidence is retained.

Final validation: 891 workspace passes, zero failures, nine ignores; optimized
runtime 382 unit tests and nine doctests plus the armed detector; twelve selected
optimized integrations; 156 s390x checks; 50 native and 256 runtime
ThreadSanitizer executions without reports. Compact-enum stress passed in 31.19
seconds with unchanged deadlines. Final source hashes were verified. Evidence:
`target/gc-validation/2026-10-17-root-enrollment/`.
Registry shrinking during collection remains open: it bounds sparse scans and
preserves allocation-free owner binding, so deleting it is insufficient.

## Raw-channel remembered references — 2026-10-17

Minor evacuation now omits clean unbound queue slots. Completed minor copying
clears their remembered marker without retiring their execution-owned records.
Diagnostics, preflight scans and major collections retain complete roots; heap
cards preserve young children of old queued messages. Young sends re-arm the
native queue marker. A poisoned/verified one/four-worker reference checks these
contracts, FIFO payloads, backing release and retained registration. Test-only
thread-local observation checks the real collector selects the proper root mode.
Seven independently restored fault controls fail as expected.

504 local baseline/candidate profile processes use identical workload source,
seven alternating pairs, raw/bound queues and one/four workers. Raw minor paired
ratios are 0.274–0.636; raw major 0.942–1.071. Bound controls are mixed (major
0.931–1.267, minor 0.916–0.995). These are local workload results, not production
budgets or a universal speedup.

Final validation: 887 workspace passes, zero failures, nine ignores; optimized
runtime 378 units and nine doctests plus the armed detector; twelve selected
optimized integrations; 140 s390x checks; 50 native and 252 runtime
ThreadSanitizer executions without reports. Workspace compact-enum stress passed
in 31.81 seconds with unchanged deadlines. Final source hashes were verified.
The profile source differs from final source only by test-only instrumentation.
The broader GC objective remains active.

Evidence: `target/gc-validation/2026-10-17-channel-unbound/`.

Channel remembered-reference checkpoint (2026-10-17): young sends mark native
queues; only completed minor collection clears surviving owned queues. Clean
queues require no minor conditional snapshot storage. Major/read-only edges and
raw unbound tracing remain complete. Old queued messages retain young children
through the heap card barrier. Three new poisoned/verified references and ten
restored fault controls pass, including weak-owner relocation/retirement while
the queue is clean.

Final validation: 886 workspace passes, zero failures, nine ignores; optimized
runtime 377 units + armed detector + nine doctests; twelve integrations;
136 s390x checks; 50 native + 250 runtime ThreadSanitizer executions without
reports. 252 local profile processes retain minor ratios 0.638–1.050 and major
0.942–1.101. The mixed result does not establish a general speedup or production
budget. Details: [GC_READINESS.md](GC_READINESS.md),
[GC_PERFORMANCE.md](GC_PERFORMANCE.md).

Channel owner metadata checkpoint (2026-10-17): contiguous channel storage and
group ends replace per-owner vectors and duplicate grouping maps. Source lookup
stores only copy-activated owners; old minor-GC owners need no lookup allocation.
At 1024 distinct owners, conditional constructor allocation calls fall from 1055
to five, read-only edges from 1035 to three. Initial-only retained workspace falls
from 40960 to 32768 bytes; a regression rejects the earlier 67592-byte iteration.
Shared-owner, mixed-generation and moving-GC checks pass, including 33 queues
per owner. Eleven restored fault controls pass.

Final validation: 883 workspace passes, zero failures, nine ignores; optimized
runtime 374 units + armed detector + nine doctests; twelve integrations;
124 s390x checks; 50 native + 244 runtime ThreadSanitizer executions without
reports. 378 local profile processes retain major ratios 0.657–1.015 and minor
ratios 0.613–0.822 versus the original. An initial compact-enum timeout, its
near-limit replay and unchanged passing reruns are retained. Numerical production
budgets remain open. Details: [GC_READINESS.md](GC_READINESS.md) and
[GC_PERFORMANCE.md](GC_PERFORMANCE.md).

Demand-driven registry staging checkpoint (2026-10-17): construction drops the
speculative planning lock; publication stages tables only on actual shortage.
The race regression now exhausts competing capacity and completes a second GC
during retry-stage allocation, correcting the earlier test's coverage gap.
Nine fault controls pass. Final gates remain 879 workspace passes, optimized
runtime 370 units + nine doctests, eight selected optimized integrations,
108 s390x checks and 23 selected ThreadSanitizer tests. Local small construction
improves about 3% versus planning; larger samples are slower, and no general
speedup or production budget is established. Evidence and limitations:
[GC_READINESS.md](GC_READINESS.md), [GC_PERFORMANCE.md](GC_PERFORMANCE.md).

Staged channel registry growth checkpoint (2026-10-17): allocation and disposal
of replacement registry tables run BLOCKED and off-lock; RUNNING migration and
insertion allocate nothing. Capacity is rechecked after native staging, with
retry on competing registration. Seven fault controls and final validation pass:
879 workspace tests, optimized runtime 370 units + nine doctests, eight selected
optimized integrations, 108 s390x checks and 23 selected ThreadSanitizer tests.
The one-slot construction/close profile regresses about 9%; larger samples are
noisy. Migration cost, collector-side shrinking, metadata accounting and wider
production budgets remain open. Details: [GC_READINESS.md](GC_READINESS.md) and
[GC_PERFORMANCE.md](GC_PERFORMANCE.md).

Channel synchronization checkpoint (2026-10-17): every capacity now constructs
native control/backing in a root-published BLOCKED region. A native-only helper
takes scalar type IDs and reserved backing charge, prepares the state mutex,
backing lock modes and both notification backends, then returns a complete control
for registration. No managed pointer enters that helper. The shared test-only
Rust allocator probe observes zero allocations/deallocations on first primitive
access before registry insertion and on uncontended send/receive/sender/diagnostic
paths afterward. Close allocates nothing while freeing backing. This does not
cover registry/index growth, exceptional diagnostics, OS internals or arbitrary
contention/GC paths.

Forced collection completes during both one-slot and large unpublished
construction, with reservations charged and witness roots published. Seven
restored controls detect cold not_empty/not_full/state initialization, missing
preparation, cold public operations and RUNNING small/large construction.

The new small-constructor transition exposed an invalid atom-test schedule:
concurrent scanning waited for a constructor that could wait for that same phase.
The repaired schedule keeps alias/SATB/closure/witness checks in the held scan,
releases it before construction, reloads managed arguments and acknowledges
RUNNING readiness before the next cycle. The original 20-second watchdog, eight
cycles and payload/byte-count assertions remain. Ten debug and ten optimized
runs pass; two restored dependency/acknowledgement controls hit the watchdog.
An initial compact-enum 60-second timeout passed an isolated saved-binary replay
in 5.859 seconds and the final unchanged integration; its deadline is unchanged.

Final gates: 875 workspace tests passed, zero failed, nine intentional ignores;
367 runtime units and nine doctests are included. Optimized: 366 default runtime
units + one armed detector + nine doctests; 63 selected integrations. Actual
s390x: 100 checks, including both channel-sync references. ThreadSanitizer:
50 native runs and 228 runtime test executions without reports (filters overlap).

112 profiling processes retain noisy first-use/construction and operation samples.
Paired construction/close ratios span 0.423–1.199; send/receive ratios span
0.798–1.155. No speedup, numerical pause/throughput/RSS or startup budget follows
from these samples. Registry/index allocation, other native synchronization and
metadata domains, hard admission and broader production budgets remain open.

Evidence, initial failures, replay, restored controls, exact samples and final
logs/hashes: `target/gc-validation/2026-10-17-channel-sync/`.

Batched native-retirement checkpoint (2026-10-17): registry sweep and execution
shutdown group already reserved backing nodes into one O(1) mailbox publication
per same-monitor group. Separate item/batch links permit iterative destruction
without allocation during group publication. Exclusive controls are unwrapped;
retained native observers keep storage until their final individual handoff.
Weak-only observers do not prevent grouping. Charges remain until actual free.
Tests observe 64 retired buffers in one sweep/shutdown publication, retained
observer fallback, mixed monitors and three collections behind a stalled release.

Local std source inspection found lazy pthread Condvar allocation even from
notification. Worker construction now prepares notification before publication,
inside its native BLOCKED body. A no-worker current-thread Rust allocator probe
checks group building/publication: zero allocations and zero deallocations.
Removing initialization makes that test fail. This does not cover registry/control
metadata destruction or other native synchronization paths. Shutdown observation
now completes GC before inspecting locks, avoiding the short publication race.

Final gates: 872 workspace tests passed, zero failed, nine intentional ignores;
364 runtime units and nine doctests are included. Optimized: 363 default runtime
units + one armed detector + nine doctests; 63 selected integrations. Actual
s390x: 92 checks, including all three native-retirement references. ThreadSanitizer:
50 native runs and 222 runtime test executions without reports (filters overlap).
Eleven restored fault controls detect skipped sweep/shutdown grouping, lost item
or batch links, wrong counts, early observer detachment, inline free, weak-only
fallback, missing flush, mixed monitors and cold notification allocation.

Final local profiling compares the verified individual-handoff worker with the
current implementation: 98 processes, seven alternating pairs per case. Paired
collection ratios are 0.904 for 256 × 32776-byte buffers and 1.066 for 32 × 1 MiB;
complete-cleanup ratios are 1.148 and 1.104. Steady construction/close ratios span
0.964–1.004. Fewer handoffs do not prove a general speedup: publication happens
later, more bytes may remain queued at GC return, and total cleanup can cost more.
Application, startup, tail/pause, RSS, hard admission and broader native metadata
budgets remain open. Bounded grouping/overlap tuning is further performance work.

Evidence, baseline provenance, source/binary hashes, exact samples, fault controls
and final logs: `target/gc-validation/2026-10-17-retirement-batches/`.

Native channel-retirement checkpoint (2026-10-17): implicit destruction of
large queue backing transfers a preallocated native node to a lazy native-only
worker. Enqueue allocates nothing and frees no payload. The worker detaches
batches before destruction, outside mailbox/registry/collector locks. Explicit
close/drain release stays synchronous and BLOCKED. Charges follow backing through
queued/in-flight release and native observers, ending after deallocation.
Execution shutdown drains and joins its worker while BLOCKED; native observers
can retain a native-only monitor after Heap destruction, with worker termination
once the last producer and pending work end. A reused execution starts a fresh
worker. Retained native backing does not extend managed object lifetimes.

Validation caught and repaired a growth-policy regression: adapting while dead
backing was queued inflated future demand. A separate pressure-service mutex now
serializes collect/drain/adapt decisions. Callers publish their roots and become
BLOCKED before any pressure lock wait. Collector exclusion is released during
native drain, then reacquired to adapt from actual remaining backing and restore
same-thread registrations together. The unchanged serial retention assertion is
1,310,720 bytes. Saved failing optimized/debug binaries peak at 4,980,768 and
5,505,056 bytes; identical fixture source with the final native/JIT implementation
peaks at 786,464 normally and 524,320 under stress, ending at zero.

Final gates: 867 workspace tests passed, zero failed, nine intentional ignores;
359 runtime units and nine doctests are included. Optimized: 358 default runtime
units + one separately armed detector + nine doctests; 63 selected integrations.
Actual s390x: 84 checks. ThreadSanitizer: 50 native runs and 217 runtime test
executions without reports (selected filters overlap). Ten restored fault controls
detect inline implicit free, early uncharge, mailbox-held free, RUNNING shutdown,
worker leak, joining a retained observer, RUNNING pressure drain/lock wait, stale
growth tuning and collector exclusion retained through deferred drain.

Final performance samples contain 98 processes and separate first use, steady
construction, collection and complete cleanup. They show overhead and substantial
variation, not a broad speedup or latency certification; nearby host load is
roughly 17–20 on 12 active CPUs. Batched retirement handoffs are the next measured
optimization. Registry/control/mailbox/stack accounting and allocation policy,
hard admission, other native domains and application/pause/RSS/platform budgets
remain open. Earlier large-backing destruction backlog is addressed; the overall
production objective remains open.

Evidence, initial failed gates, saved regression binaries, final logs, profiles,
controls and source hashes: `target/gc-validation/2026-10-17-native-retirement/`.

Sparse channel-root checkpoint (2026-10-17): collector, preflight and inspection
walks now visit the occupied ring range, using at most two slices rather than
visiting every reserved slot. Send/receive publish head/count with slot changes
in a safepoint-free RUNNING transaction under queue state. The collector reads
only during a world pause; this is not a concurrent queue snapshot. Scanning
never takes the state mutex that a BLOCKED waiter may hold. Native capacity and
its charge are unchanged; range metadata is part of unmetered control storage.

Final gates: 859 workspace tests passed, zero failed, nine intentional ignores;
351 runtime units and nine doctests are included. Optimized: 350 default runtime
units + one armed detector + nine doctests; 40 selected integrations. Actual
s390x: 80 checks. ThreadSanitizer: 50 native runs and 209 runtime test executions
without reports (selected filters overlap). Five restored fault controls detect
missing send/receive publication, omitted wrapped segments, capacity scanning
and acquisition of the waiter-held state mutex.

126 profiling processes provide seven alternating baseline/current pairs for
nine capacity/occupancy combinations. For 131072 slots and one queued message,
local single-worker collection medians are 5998.323 µs before and 1.068 µs after.
Paired send/receive ratios span 0.971–1.033 across the cases. This is quiescent
collection of a fixed live graph and a separate empty-queue send/receive loop;
it does not establish dense large-queue throughput or application tail budgets.

Evidence: `target/gc-validation/2026-10-17-channel-scan/`. Collector-sweep,
shutdown and transient-observer final destruction of large backing, registry
metadata allocation, hard native admission and broader runtime budgets remain
open. Earlier capacity-scan backlog entries are addressed by this checkpoint.

Channel native-buffer checkpoint (2026-10-17): large queue construction now
reserves its native charge before allocation, captures scalar type metadata while
RUNNING, and builds zeroed backing while BLOCKED before registry publication.
Empty close, final-sender close and closed-queue drain detach backing under queue
locks, then release large backing after unlocking while BLOCKED. The last received
message transfers to a scratch root and reloads after collecting cleanup.

Final gates: 857 workspace tests passed, zero failed, nine intentional ignores;
349 runtime units and nine doctests are included. Optimized runtime: 348 default
units + one separately armed detector + nine doctests; 53 selected optimized
integrations. Actual s390x: 76 selected checks. ThreadSanitizer: 50 native runs
and 205 runtime test executions without reports (selected filters overlap).
Six restored fault controls detect RUNNING construction/release, late reservation,
stale/unrooted popped values and a retained queue-state lock.

Seven alternating baseline/current pairs across seven capacities check 98
processes. Median paired construction/empty-close ratios range from 1.014 to
1.088; the 1 MiB backing case is 1.053. These include registry insertion and
exclude managed wrappers/final execution teardown. They establish neither an
application latency budget nor RSS or admission limits.

Evidence: `target/gc-validation/2026-10-17-channel-buffers/`, including gated
source hashes, logs, restored controls and local profiles. Collector-sweep,
shutdown and transient-observer final destruction can still release large native
backing outside this mutator protocol. Capacity-proportional queue scans,
registry metadata, hard admission and broader production budgets remain open.

Native FFI ownership/accounting checkpoint (2026-10-17): current generated
foreign buffers use opaque v2 ownership tokens with separately aligned payloads.
Payload allocation sizes stay unchanged; ownership metadata is charged separately.
Large allocation/free bodies run BLOCKED on native data only. The shared queue/FFI
ledger is heap-owned, independent of the channel registry; leaf allocation and
counter queries are checked while that registry lock is held. Live/pending
reservations remain charged until payload and ownership metadata are deallocated.
Native pressure uses the existing coalesced full-closure collection policy;
callers reload their captured managed source after allocating native storage.
Legacy v1 allocation/release remain noncollecting and unmetered.

The new counters exposed a real argument-evaluation leak: the saved conditional
early-return binary exits 42 with 96 native bytes still live. The identical source
with the final compiler exits 42 with zero live native bytes. Return, break and
continue now free only the scopes they escape and restore their stack marks;
inner-loop exits preserve surrounding call buffers. Cleanup can collect, so
traced return locals are reloaded before unlinking. Direct argument returns now
stop call/coercion emission instead of producing invalid LLVM IR.

Final gates: 851 workspace tests passed, zero failed, nine intentional ignores;
344 runtime units and nine doctests are included. Optimized runtime: 343 default
units + one separately armed detector + nine doctests; 49 selected optimized
integrations. Actual s390x: 72 selected checks. ThreadSanitizer: 48 native runs
and 195 runtime test executions without reports (selected filters overlap).
Six restored fault controls detect RUNNING allocation/free, early uncharge,
omitted pressure collection, stale source reuse and missing scope cleanup.

Seven alternating allocation/free pairs across seven sizes check 98 processes.
The local 8-byte median is about 15 ns legacy versus 38 ns accounted/owned;
1 MiB timings are near parity in the final paired sample. This is a quiescent
allocation/release loop with initialization and counter checks, not a full-call,
contention, RSS, admission-limit or tail-latency budget. Initial prefix storage
crossed allocator size boundaries; final payload requests match legacy geometry.

Evidence, replay helpers, profiles, source snapshots and hashes:
`target/gc-validation/2026-10-17-ffi-buffers/`. Hard admission, other native-memory
domains, small-request OS latency and broader production budgets remain open.

Earlier cooperative-copy checkpoint follows; its allocation/accounting backlog
is addressed for current generated FFI buffers by this newer checkpoint.

Cooperative FFI-copy checkpoint (2026-10-17): generated String/scalar-array
foreign copies now call versioned helpers. Large scalar pieces hold canonical
object access only through 32 KiB of matching-width SC copying; immutable String
pieces copy at most 256 KiB. Both root/reload the owner and poll between pieces.
The bounded noncollecting leaf threshold remains 32 KiB, and legacy copy symbols
remain noncollecting. No Thread/startup layout changes or unchecked text are used.

Tests cover pending-before-copy and mid-copy GC, both heap modes with poison,
all scalar widths and boolean masking, unaligned native buffers, UTF-8 split
inside code points, empty/boundary lengths and canaries. Read-only progress tests
have no caller roots. Generated tests cover concurrent collection, caller reloads,
original owner identity after field replacement, and old-runtime link rejection.
Six restored fault controls detect missing polls, stale owners, lost offsets,
missing caller reload and object access retained across a poll. A periodic test
pattern that initially hid lost offsets was replaced with a nonperiodic model.

Final gates: 843 workspace tests passed, zero failed, nine intentional ignores;
339 runtime units and nine runtime doctests are included. Optimized runtime:
338 default units + one separately armed detector + nine doctests; 46 optimized
integrations. Actual s390x: 68 selected checks. ThreadSanitizer: 44 native runs
and 189 runtime executions, no reports. The prior immediate-Weak destruction
assertion in completion churn was corrected to distinguish registry unlink from
transient native Arc borrows, while retaining immediate pruning and bounded
actual-destruction checks. Final full and optimized reruns passed.

A same-binary, seven-pair profile checks all 448 processes across 32 cases against
independent byte/checksum models. Median 1 MiB new/legacy time ratios: String
1.061; scalar 8-bit in/out 1.006/1.001; 64-bit in/out 1.004/1.009. Initial tiny
copies paid 20–30% overhead; outlined large paths reduced this. Increasing only
the immutable String piece from 32 to 256 KiB reduced its 1 MiB overhead from
about 22% to 6%. These are copy-helper profiles with no requested collection,
not complete foreign-call, allocation, RSS or tail-latency budgets.

Evidence, replay helpers, source snapshots/hashes and profiles:
`target/gc-validation/2026-10-17-ffi-copy/`. Native allocation/free latency,
accounting/admission and the remaining production roadmap are still open.

Earlier native FFI/toolchain checkpoint follows. Its copy-progress item is now
addressed by the checkpoint above; its native-allocation work remains open.

Native FFI and build-toolchain checkpoint (2026-10-17): foreign String and
scalar-array argument copies now use scoped, eight-byte-aligned native storage
rather than input-sized stack allocations. Generated callers free buffers after
native return and mutable-array copy-back, including nested argument calls.
A 4 MiB String plus 4 MiB mutable array passes repeated calls with a 1 MiB
worker stack and one/four GC workers; restoring stack allocation reproduces
SIGBUS on supported LLVM 21. Empty and odd-size allocation checks also pass.

The compiler build now enables llvm-sys strict version checking. Default
llvm-sys discovery had accepted LLVM 23 despite Inkwell's LLVM 21 bindings,
causing an opcode-inspection crash and narrow signed constant miscomparison.
Strict discovery selects supported LLVM 21.1.8; README documents the prefix.
Propagated signed constants now use the signed LLVM constructor consistently.
The integer miscomparison control fails on LLVM 23 but passes on LLVM 21;
it is not a separately reproduced arithmetic defect on the supported backend.

Verification on LLVM 21.1.8: full workspace 835 passed, zero failed, nine
intentional ignores (including 335 runtime units and nine runtime doctests).
Focused release verification: 22 FFI, 14 arithmetic and four float-parsing
runtime tests passed. Source restored exactly after causal controls; 36 focused
debug tests passed again after restoration. Float parsing is included in these
functional gates; new performance/platform/sanitizer certification is not claimed.
Native allocation accounting/latency and cooperative scalar-copy progress remain
open, alongside the broader production roadmap. Evidence and source hashes:
`target/gc-validation/2026-10-17-native-ffi/`.

Owned String-input checkpoint (2026-10-17): fixed reproduced preparation
progress defects in substring, raw/newline printing, legacy read-file paths and
checked host path/text. Shared copying pins/reloads the source across native
growth and 32-KiB chunks. Selected substring boundaries are checked before
polling. Large UTF-8 validation owns native bytes only and runs BLOCKED;
checked host calls root both arguments before either copy can park, then reload
the second root. Existing relocating entry-point ABI and Thread/startup record
layouts are unchanged. No unchecked Rust str is introduced.

Six verifier/poison regressions cover both heap modes, early/mid-copy requests,
nonzero substring offset, printing, oversized read paths, either host argument
large, malformed-text/range semantics, and the collector call returning inside
native validation before that phase completes. Five causal controls detect
missing polls, stale source addresses, lost range offset, missing second host
root and unblocked validation; all restore exact source bytes.

Gates passed: 825 debug; 824 optimized default + one armed detector (825 total);
nine remaining intentional ignores. Serial runtime: 331 units + nine doctests;
20 targeted integrations; six poison checks. Actual s390x: 60 selected checks.
ThreadSanitizer: 40 native runs + 181 runtime executions without reports.
385 frozen inputs and 425 archived current files verified, with 423 baseline
files / 383 baseline inputs. Measured runtime sources match final code.

Seven alternating pairs per case, same saved compiler and distinct clean-built
runtime archives: all 140 measured processes match independent output models.
Small ASCII/Unicode substring paired time ratios 1.036/1.023; large 1.001/1.004.
Constructor ratios: ASCII concat/join 1.015/1.012, Unicode 1.000/0.998. CSV
one/four workers: 0.953/1.011, with identical per-configuration managed
allocation counts/bytes across all runs. Scoped ranges and previous source,
artifact and helper variants are retained. An inline hint left the helper call
in place; timing differences are not credited as a causal inlining gain.

Evidence, replay/build/profile/report scripts and archives:
target/gc-validation/2026-10-17-owned-string-inputs/.
Production ARM64 assembly also shows the RUNNING branch entering BLOCKED before
UTF-8 validation. This bounds copy work, not allocator/OS latency, total GC pause
or aggregate memory admission. Float parsing, FFI buffers/terminal formatting,
managed zeroing, native metadata/stacks/admission, outcomes/cancellation,
embedding/platform audits and soak/CI remain open. Full production-readiness
work remains active.

# Previous verified string-scan state

Versioned string-scan checkpoint (2026-10-17): reproduced large equality and
hash scans returning without servicing an already requested collection. New
large scans root/reload immutable input bytes and poll every 32 KiB. Compiler
root-cache checks cover the collecting calls. Original ai_str_eq/ai_str_hash
symbols remain noncollecting for older binaries; generated code guards byte
count and uses those only for bounded small inputs. Large inputs use versioned
ai_str_eq_v2/ai_str_hash_v2 symbols, rejecting incompatible custom archives.
Thread/AotLayout/startup record layouts are unchanged.

Five runtime regressions cover early/mid-scan collections in both heap modes,
late unequal bytes, chunk boundaries/reference FNV and legacy noncollection.
Generated JIT/native concurrent-allocation tests verify live caller roots;
compiler IR checks guards/reloads, and linker tests reject missing scan symbols.
Four fault controls detect missing polls, stale source pointers and omitted
caller reloads; source bytes are restored exactly.

All gates passed: 819 debug; 818 optimized default + one armed detector (819);
nine remaining intentional ignores. Serial runtime: 325 units + nine doctests;
20 targeted integrations; five verifier/poison regressions. Actual s390x matrix:
56 selected checks. ThreadSanitizer: 40 instrumented native runs + 175 runtime
executions, no reports. The new fixture executes with GC stress off/on; the
initial script built it without executing it, which was corrected and the full
sanitizer gate rerun. All 383 final input hashes and 423 archived current files
are verified, with a 420-file/380-input baseline.

Seven alternating end-to-end compiler/runtime pairs per workload, independently
checked outputs: small hash/equal/mismatch time ratios 1.027/1.042/1.034;
large ratios 0.998/1.018/0.963. CSV one/four workers: 0.996/1.037, with scoped
ranges retained. Initial uniform caller checks regressed small keys 14–19% and
were rejected; their artifacts/measurements are retained. Final measured
compiler/runtime sources match the verified implementation.

Evidence, replay/build/profile/report scripts and archives:
target/gc-validation/2026-10-17-string-scans/.
This establishes bounded scan work between polls, not whole-runtime pause,
RSS, admission or hard real-time guarantees. Older binaries retain their old
scan behavior until recompiled. Other native loops, aggregate resource budgets,
metadata/stacks, outcomes/cancellation, embedding/platform audits and soak/CI
remain open. Full production-readiness work remains active.

# Previous verified bounded-copy state

Bounded native string-copy checkpoint (2026-10-17): reproduced join ignoring
a requested collection during native preparation. Concat/join now poll while
copying bounded chunks, root/reload every source across possible parks, preserve
the exact observed element under concurrent slot replacement, and bound native
output extent by the largest managed arena. Large native buffer growth runs
BLOCKED. Large managed output copying roots its private partial result and
reloads it after collection, guarding each destination write.

Five focused regressions pass with verifier/poison in both heap modes where
applicable. Four causal controls detect missing polls, stale source/output
aliases and missing extent bounds, with exact source restoration. Gates: 811
debug checks; 810 optimized default plus one armed detector (811 total); nine
remaining intentional ignores. Serial runtime: 320 units plus nine doctests;
18 targeted integrations; five poison regressions. Actual s390x matrix: 52
checks. ThreadSanitizer: 38 native runs and 170 runtime executions, no reports.

Seven alternating pairs per case preserve complete expected outputs. Constructor
paired time ratios: ASCII concat 0.866, join 1.002; Unicode concat 0.938, join
1.005. CSV relative to the already-fast previous runtime: one worker 1.014,
four workers 1.037. These scoped costs/gains are not whole-language performance
or pause-budget guarantees. Measured implementation matches final source;
only the join documentation comment changed after timing.

Evidence: target/gc-validation/2026-10-17-bounded-string-copy/, including logs,
fault controls, performance samples, source hashes, report and replay scripts.
380 frozen inputs; 420 current source/document files, 419 baseline files.
Per-result extent is not aggregate native-memory or live-set admission.
Managed allocation zeroing/OS allocation latency, other native loops,
metadata/stacks, outcomes/cancellation, embedding/platform audits and soak/CI
remain open. Full production-readiness work remains active.

# Previous verified string-range state

Profile-guided string-range checkpoint (2026-10-17): fixed repeated whole-input
UTF-8 validation under `ai_str_substring`. A native CSV sample found 741/753
active-worker samples there; other threads mostly waited. Each small field
slice rescanned the complete file buffer. Range copying now checks endpoints,
including empty cuts inside code points, owns only selected bytes and validates
that copy before allocating managed output. Work scales with copied bytes;
no unchecked Rust text or per-string cache is introduced.

Ordinary text construction accepts &str. Concat/join use a private native UTF-8
buffer constructor with explicit validity/lifetime preconditions: concatenating
complete valid String sequences/separators preserves UTF-8. All copy-out precedes
GC allocation. An initial constructor design revalidated already-valid output
and regressed large Unicode concat/join about 3.9x; it was rejected. Final
construction controls are near baseline. Thread/AotLayout/string-entry ABI
source and compiler root spilling are unchanged.

Three new verifier/poison regressions cover Unicode/reference/clamp cases,
selected malformed text/unrelated-byte behavior, and repeated relocation through
substring/concat/join. Two causal controls remove boundaries or reintroduce full-
source validation; both fail and restore exact bytes. Final gates: 806 debug
workspace checks; 805 optimized default plus the armed detector (806 total);
nine remaining intentional ignores. Serial runtime: 315 units + nine doctests;
targeted integrations: 18; poison checks: three. Selected s390x matrix: 48
checks, debug/release x one/four workers. ThreadSanitizer: 38 native runs and
165 runtime executions without reports. All 379 frozen input hashes match;
419 current source/doc files archived with a 417-file baseline under
`target/gc-validation/2026-10-17-string-allocation/`.

Seven alternating native runtime pairs on four quoted-Unicode 5000-row CSV
files matched an independent model and identical per-configuration allocation
counts/bytes in all 28 timed runs. Default adaptive GC workers, 1 MiB nursery,
64 MiB tenured spaces; same compiler, distinct clean-built runtimes with all
copied source hashes verified. One application worker: median 1510.26 ms to
23.08 ms, paired time ratio 0.01531 (0.01472–0.01618). Four workers: 431.44 ms
to 17.51 ms, ratio 0.04072 (0.03864–0.04173). About 65x/25x on this specific
local workload, including startup/I/O/parsing/grouping/threads/shutdown.

Large ASCII/Unicode constructor controls compare complete outputs over 2000
concat/join operations. All 56 timed processes return expected checksums. Median
time ratios: ASCII concat 1.002, join 1.017; Unicode concat 0.994, join 1.004.
Raw samples/ranges, input model, source/library/binary/assembly hashes and replay
scripts are retained. A build that copied a transient fault-control module was
rejected before timing; final builds run sequentially with full source checks.

These results do not establish general application, RSS, tail-latency or platform
release budgets. Large native concat/join copies still have no intermediate
managed safepoints and output-size/admission preflight remains open; do not call
that a reproduced deadlock. A proper polling design must root/reload array and
separator and keep managed borrows out of polls. Resource/metadata/stack/admission
budgets, outcomes/cancellation, broader embedding/foreign/generated-mutation/
platform audits, scheduled soak/CI and release budgets remain open. Full GC
production-readiness/high-performance work remains active.

Checked variable-length local-allocation checkpoint (2026-10-17): Heap now
reuses validation of immutable canonical descriptors, skipping structural
comparison by identity while still requiring equality for distinct descriptors.
Per-request length multiplication, padding, alignment and isize limits remain
checked. Standalone/public descriptor validation remains full validation.
`ai_gc_alloc_varlen` tries an initialized, non-collecting local request up to
8192 bytes before publishing the compiled frame for the collecting fallback.
Stress, exhaustion, large/direct-tenure cards and exactly-once profile counts
retain their previous contracts. Thread/AotLayout/entry ABI source is unchanged.

Five new regressions verify byte/value initialization and exact local boundary,
stress collection, counter/root preservation through mixed local/large paths,
forged descriptor rejection/equivalent owned-borrowed layouts, and native-size
arithmetic against an independent u128 oracle. Three controls (stress bypass,
missing structural guard, omitted length initialization) fail and restore exact
source bytes. The s390x gate now includes these selected runtime checks.

Final gates: 803 debug workspace checks; 802 optimized default plus the armed
detector (803 total); nine remaining intentional ignores. Serial runtime: 312
unit checks plus nine doctests. Verifier/poison targeted checks: five. Selected
s390x debug/release x one/four-worker matrix: 36 checks. ThreadSanitizer: 38 native
runs and 162 runtime executions without reports. All 377 frozen input hashes
match; 417 source/doc files archived in
`target/gc-validation/2026-10-17-varlen-fast/`, alongside a 416-file baseline
snapshot, three fault controls, replay scripts, binary hashes and raw evidence.

Native allocation-loop profiling used seven alternating pairs, seven samples
per case/process, excluded warmups, checked payload/count/checksum and equal
allocation/collection counts. Default-worker median candidate/baseline ratios:
0.596 zero-byte tails, 0.741 64-byte tails, 0.946 1024-byte tails, 0.782 16-value
tails and 0.507 flattened64. Single-worker results are recorded separately.
Large-object results are mixed; no consistent improvement claimed for that path.

A separate native CSV workload used four quoted-Unicode 5000-row files and an
independent grouping model. All 28 measured runs matched the model and the
versions' per-configuration allocation counts/bytes. The same compiler linked
distinct runtimes, built in fresh target directories; disassembly confirms the
new local branch only in the candidate. Median paired wall-time ratios were
0.999 for one application worker (range 0.980–1.009), and 1.005 for four workers
(range 0.999–1.008). **No consistent end-to-end CSV speedup was observed.**
The initial copied-source timestamp-cache app setup was rejected before timing;
its identical runtime hashes are retained as historical invalid setup.

This is scoped allocation-loop improvement, not whole-application performance
certification. Metadata/stack/resource/admission budgets, outcomes/cancellation,
broader embedding/foreign/generated-mutation/platform audits, scheduled soaks/CI
and measured application RSS/throughput/pause budgets remain open. Selected
emulated runtime checks do not certify a full big-endian language platform.
Full GC production-readiness/high-performance work remains active.

Bounded concurrent SATB checkpoint (2026-10-17): repaired global log retention
that grew with overwrite count despite bounded thread-local buffers. An actual
concurrent-copy reproduction performed 65,536 repeated overwrites of one
referent and retained 131,072 global values before the final pause (at least
1 MiB of retained payload). It now retains one value in a 2 KiB chunk, a 16 KiB
bitmap and 96 bytes of chunk-vector capacity for a 1 MiB source arena.

Heap configures source-address deduplication before enabling Copying. The bitmap
retains each source address once regardless of tag variants; null/non-source
values need no global retention. Values pack into 256-entry chunks, preventing
sparse flushes from retaining a full local allocation per unique referent. Seen
bits survive drains and reset next cycle. Decoding runs outside the queue mutex;
publication has fixed 256-address scratch and no managed safepoint or object-
stripe acquisition. Standalone queues preserve unfiltered transfer semantics.
Cold queue stats report current/last-cycle retention and capacity.

Four new regressions cover actual overwritten-reference reachability and next-
cycle reclamation, tag/range/bitmap boundaries and drains/reset, sparse packing
with eight concurrent producers, and callbacks outside the queue mutex. Disabling
bounded cycles or chunk packing independently fails the relevant regression;
both controls restore exact source bytes. Existing buffer-transfer/reuse checks
continue to pass.

Final gates: 798 debug workspace checks; 797 optimized default plus the armed
detector (798 total); nine remaining intentional ignores. Serial runtime: 307
units plus nine doctests. Verifier/poison SATB checks: seven. Selected s390x
runtime matrix: 16 passes across debug/release and one/four workers.
ThreadSanitizer: 38 native runs and 157 runtime executions without reports.
All 375 frozen input hashes match; 415 source/doc files are archived under
`target/gc-validation/2026-10-17-bounded-satb/`, with logs, pressure stats, two
fault controls, replay instructions and report. Runtime ABI source is unchanged.

The global bound is at most ceil(source capacity / 8) retained unique values,
rounded chunk storage and chunk-vector capacity, plus a reusable bitmap of
approximately source capacity / 64 bytes (Vec minimum/rounding applies). Existing
per-mutator local buffers, transient replacement and fixed decoding scratch are
separate costs. Producer/thread/native metadata/stack limits, hard admission,
whole-process RSS and application throughput/pause budgets remain open, as do
broader embedding/generated-mutation/platform audits, soak and CI. Selected
emulated runtime tests do not certify a complete big-endian language platform.
No global throughput/pause improvement or complete production readiness claimed.

Dynamic layout-offset ownership checkpoint (2026-10-17): fixed permanent leaks
in both the JIT layout bridge and native runtime entry. Each formerly leaked
per-shape interior-pointer offsets; repeated embedded executions accumulated
allocations beyond runtime/table destruction. `TypeInfo` now borrows static
slices or owns dynamic vectors through `Cow<'static, [u16]>`. Native decoding
moves vectors into the table instead of retaining cloned offset buffers.

Descriptors are Clone rather than Copy; owned clones have independent storage.
Static interior layouts use the const `for_header_with_interior_ptrs` constructor;
`with_interior_ptrs` remains a runtime builder. Existing tests now share/clone
explicitly. Allocation and collector paths continue borrowing canonical entries,
with no per-object descriptor clone. The Rust descriptor representation/API
changes; Thread/AotLayout ABI source, metadata encoding and native entry signature
are unchanged.

The regression observes actual deallocation of exact offset buffers across 32
JIT-style runtime lifetimes and 32 native entries. A retained Heap owner keeps
metadata valid after context destruction; final Heap destruction frees it.
Clone independence and equality with static descriptors are checked. Both former
leaking constructions independently fail with `offset allocation leaked`, with
exact source restoration and a final passing lifetime check.

Final gates: 794 debug workspace checks; 793 optimized default plus the armed
detector (794 total); nine remaining intentional ignores. Serial runtime: 303
units plus nine doctests. Verifier/poison graph oracle: eight checks. The selected
s390x header/runtime matrix passes 16 debug/release executions at one/four workers.
ThreadSanitizer: 38 native runs and 153 runtime executions without reports.
All 374 frozen input hashes match; 414 source/doc files are archived under
`target/gc-validation/2026-10-17-owned-layouts/`, with logs, replay instructions,
manifest, two detected fault controls and report.

This repairs one metadata lifetime defect; complete metadata/stack accounting,
hard resource/admission budgets, outcomes/cancellation, broader embedding/foreign/
generated-mutation/platform audits, soaks and CI remain open. Selected emulated
runtime evidence is not full s390x compiler/JIT/AOT/FFI certification. No whole-
process RSS bound or throughput/pause improvement is claimed. The full production-
readiness/high-performance goal remains active.

Built-in header byte-order checkpoint (2026-10-17): fixed an actual big-endian
forwarding collision. Original s390x binaries treated fresh Compact/Full objects
with type ID 32768 as forwarded because a leading native u16 occupied bit 63.
Both built-ins now keep IDs in a numeric u64 word's low 16 bits; generated inline
Full initialization uses two numeric i64 words. Type-ID slots are 0/8 on
little-endian and 6/14 on big-endian. Header sizes and little-endian object bytes
are unchanged; Compact's Rust alignment is eight on validated 64-bit targets.

Four new runtime regressions exhaust all 65,536 IDs and cover high-ID shared
cycles, tagged references, Full tags, Heap copy modes and standalone SemiSpace.
A compiler regression checks actual emitted header constants against independent
little/big LLVM data layouts. Restoring either former representation makes its
regression fail; both fault controls restore exact source bytes.

Final gates: 793 debug workspace checks; 792 optimized default plus one armed
detector (793 total); nine remaining intentional ignores. Serial runtime: 303
unit checks plus nine doctests. Targeted integrations: 23; verifier/poison: four.
The pinned Docker/QEMU gate passes 16 actual s390x executions across debug/release
and one/four workers with verifier/poison enabled. ThreadSanitizer passes 38 native
runs and 153 runtime executions without reports. All 373 frozen build/test input
hashes match; 413 source/doc files are archived under
`target/gc-validation/2026-10-17-header-endian/`, with logs, two fault controls,
source manifest, replay instructions and report. Thread ABI source is unchanged.

Selected emulated-runtime checks do not certify a full s390x language platform,
big-endian JIT/AOT/FFI, the full cross-target suite or physical-hardware performance.
Native metadata/stack budgets, outcomes/cancellation, broader embedding/foreign/
generated-mutation/platform audits, soaks and CI remain open. No throughput,
pause-time or RSS improvement is claimed; full production readiness remains active.

Completion-triggered native thread cleanup checkpoint (2026-10-17): fixed a
confirmed gap where finished threads retained unjoined OS handles while managed
owners remained live or raw handles retained execution lifetime. The bounded
32-spawn regression failed to reach completion without GC/explicit join; it now
finishes with zero collections and preserves repeated scalar results.

Runtime admission creates a publication owner and unique exit guard, captured
before native launch. The handshake handles either publication order, races,
setup unwind and dropped uncalled closures without waiting for the other side.
Notices weakly index the execution-owned reaper and hold no Heap reference.
Runtime records advertise this capability, so GC does not poll their active
bodies even after owner death. Legacy records retain GC's orphan-index fallback.
The worker actually joins OS/TLS exit outside collector/registry locks before
caching results under the existing completion mutex. Live managed/raw aliases
retain their metadata lifetimes and repeated/concurrent observation semantics.

Four new regressions cover no-GC live/raw completion, five handshake/cleanup
orderings with repeated races, no active-body polling before/after owner GC, and
real TLS delay with live-owner relocation and shutdown waiting. Existing churn
checks now observe actual completion before metadata retirement, rather than
mistaking an empty index for completed bodies. Final gates: 788 debug workspace
checks; 788 optimized including armed detector (787 default); nine remaining
intentional ignores. Serial runtime: 299 unit checks plus nine doctests; targeted
thread integrations: 29; poison/verification: 28. Six fault controls are detected
with exact restoration. ThreadSanitizer: 38 native runs and 149 runtime executions,
no reports. All 370 frozen input hashes match; 410 source/doc files archived in
`target/gc-validation/2026-10-17-completion-notices/`. Thread ABI source and exported
spawn C signature are unchanged; the opaque record gains a private capability field.

The active main entry or pending admitted parent covers child publication through
native TLS exit. Worker-TLS admissions use the next reaper after detachment;
shutdown still drains descendants before code retirement. A blocking native TLS
cleanup can delay the reaper, subsequent records and shutdown. No finite cleanup
latency, RSS, throughput or pause improvement is claimed. Native metadata/stack
accounting, configurable admission/resource budgets, typed outcomes/cancellation
and remaining embedding/foreign/generated mutation/platform/soak/CI audits stay
open. The full production-readiness/high-performance goal remains active.

Native queue-buffer pressure checkpoint (2026-10-17): fixed a confirmed
collection-scheduling gap where 64 abandoned promoted Channel owners retained
4 MiB of native buffers while managed old space stayed nearly empty and never
requested a major. Registered buffer charges now follow actual storage lifetime,
including transient native Arc borrows after registry removal, without retaining
Heap. Empty close or final closed receive releases buffer storage and charge.

Native queue construction checks a 1-MiB initial growth threshold before native
allocation. After pressure collection the threshold becomes max(1 MiB, twice the
sum of current accounted buffers and pending request), avoiding repeated cycles
for live/raw queues. Requests recheck under collector exclusion. Generational
service traces the full root closure and performs major plus nursery evacuation
in one pause; dead young references cannot keep old native owners alive.
Constructor witnesses are pinned/reloaded, and generated calls already use the
relocating root-cache protocol. Native scalar atomic operations stay noncollecting.

Five runtime regressions and two new integrations cover old-owner churn,
Full/Compact and identity/tagged references in both heap modes, complete graph
relocation, native borrow/Heap lifetimes, close/drain, concurrent requests,
JIT/AOT normal/stress execution, and benchmark metric propagation. Final gates:
784 debug workspace checks; 784 optimized including the armed detector (783
default); nine remaining intentional ignores. Serial runtime: 295 unit checks
plus nine doctests. Targeted integrations: 11; poison/verification checks: five;
four missing-fix controls detected with exact restoration. ThreadSanitizer:
38 native runs and 145 runtime executions, no reports. All 369 input hashes match;
409 source/doc files are archived under
`target/gc-validation/2026-10-17-native-buffer-pressure/`.

The captured 32-queue fixture reports 786,464 B accounted native-buffer peak,
2,216 B sampled managed-heap peak, ten pressure cycles, and zero native-buffer
bytes after shutdown. Metrics JSON and `gcr bench` expose live/peak buffer bytes
and pressure cycle count. This is accounting for registered slot buffers, not
RSS; it excludes allocator overhead, native control/registry metadata, atomic
cells, thread stacks and other native allocations. The adaptive trigger is not
a hard cap; large/concurrent requests can exceed it. No quiet timing or broad
throughput/pause improvement is claimed. Configurable release limits, broader
native-resource accounting and measured application budgets remain open, along
with outcomes/cancellation and embedding/foreign/generated mutation/platform/
soak/CI audits. The full production-readiness/high-performance goal stays active.

Idle-context embedding checkpoint (started 2026-10-16, verified 2026-10-17):
three bounded reproductions confirmed GC/shutdown deadlocks when idle or active
outer contexts shared an OS thread with another execution. Contexts now register
idle after initialization and activate for scoped entries. Nested entries park
outer mutators and restore bindings on return/unwind; foreign blocking scopes
preserve their state. Shutdown activates its context before parking for native
joins. Runtime registration retires roots under collector exclusion before poll
storage. ABI storage is interior mutable; raw borrowing avoids an exclusive
reference across the collector-written poll flag. The Thread ABI layout is unchanged.

`RuntimeContext::enter` is the synchronous native embedding API; `thread_ptr()`
only borrows an address. JIT/AOT entry and native test callers use explicit entry
scopes. Child env handoff transfers occur while active. Six new bounded
regressions cover the three deadlocks, moving roots between entries, nested/
foreign scopes and unwind restoration. See [threads.md](threads.md).

Final gates pass 777 debug workspace checks and 777 optimized checks including
the separately armed detector (776 default passes), nine remaining intentional
ignores. Runtime: 290 unit checks plus nine doctests. Targeted integrations: 53;
poison/heap-verification checks: 24. ThreadSanitizer: 36 native runs and 139 runtime
executions, no reports. Five deliberate missing-fix controls are detected, with
exact source restoration. All 366 frozen inputs are unchanged; 406 source/doc
files are archived in `target/gc-validation/2026-10-16-idle-contexts/`.
Intermediate logs precede the final pointer-borrow review and are not final gate
evidence; the final debug suite ran after all controls restored source.

No speed, pause, tail-latency or RSS improvement is claimed. Native pressure
scheduling/budgets, typed outcomes/cancellation, broader embedding/foreign/
generated mutation/platform/soak/CI audits and language production gates remain
open. These synchronous scopes do not define asynchronous callback or cross-thread
context lifetimes. The full production-readiness/high-performance goal stays active.

Native orphan reaping checkpoint (2026-10-16): safe Thread owners collected
without observation no longer retain their finished OS handles until execution
exit. GC indexes weak orphan records; an execution-owned worker drains the index
in batches of at most 64, retrying unfinished/contended records without another
GC. Native joins and panic-payload destruction run outside collector/registry
locks. Shutdown stops/joins the worker while BLOCKED and drains descendants
admitted by its TLS cleanup before returning/code unloading. Other heaps remain
independent. Raw unbound handles retain execution lifetime.

Final gates pass 771 debug workspace checks and 771 optimized workspace checks
including the separately armed detector (770 default passes), nine remaining
intentional ignores. Runtime: 284 unit checks plus nine doctests. Eighteen
poisoned/verified checks and twelve current fault controls pass. ThreadSanitizer
passes 36 native runs and 133 runtime executions, no reports. All 365 source/test
hashes remained unchanged. Thread ABI source and spawn/join/bind entry bodies
match the prior checkpoint. Evidence:
`target/gc-validation/2026-10-16-orphan-reaper/`.

The rejected fixed queue stranded overflow until another GC; the passing backlog
regression covers 257 orphans without a further collection. Worker lifecycle
coverage includes child/worker TLS blocking with live GC, observer retry, native
panic reentry/fatal cleanup, 128 actual unobserved spawn lifetimes and a late TLS
admission with final Heap Arc release. Existing completion tests assert immediate
registry retirement, then allow a background native borrow to release without
another GC. No throughput/pause/RSS improvement is claimed. Resource-pressure
scheduling, measured native/application budgets and remaining admission, outcome,
embedding, foreign, generated-mutation, platform, soak and CI audits stay open.
The full production-readiness/high-performance goal remains active.


Channel ownership checkpoint (2026-10-16): safe native queues now follow the
managed Channel wrapper identity. Conditional queue edges reclaim abandoned
self/cross-channel cycles; runtime operations pin the owner before parking.
Tracing covers all copy paths, concurrent final closure, exact promotion
preflights and heap inspection. The mutable type witness does not own storage;
raw unbound queues retain execution lifetime. A raw-handle type-mismatch fixture
now retains its original wrapper through the call, as required by that borrow.

Final gates pass 758 debug workspace checks and 758 optimized workspace checks
including the separately armed detector (757 default passes; nine remaining
intentional ignores). Optimized runtime: 272 unit checks plus nine doctests.
Seven poisoned/verified ownership checks and twelve fault controls pass. The
sanitizer gate passes 34 native runs and 121 runtime executions with no reports.
All 362 final source/test hashes match the archive. Runtime/compiler inputs did
not change during the gates; the corrected boundary fixture is included in both
final workspace runs. Evidence:
`target/gc-validation/2026-10-16-channel-ownership/`.
No measured throughput/pause/RSS improvement is claimed. Never-joined thread
resources, raw retention, application budgets and remaining platform, embedding,
generated-mutation, soak and CI audits remain open. The overall goal stays active.


Atomic native ownership checkpoint (2026-10-16): safe AtomicI64 is now a GC
reference wrapper with a weakly owned stable native SC cell. Collected owners
retire cells before shutdown; raw unbound cells retain execution lifetime.
Atomic/completion table storage shrinks after bursts and is released at shutdown.
Final gates pass 749 optimized workspace checks including armed detector, nine
remaining intentional ignores; 266 debug runtime units plus nine doctests; five
poisoned/verified ownership checks; expanded sanitizer gate, 32 native runs and
108 runtime executions, no reports. All 359 source/test inputs match the final
hashes. Native atomic operation bodies and Thread ABI are unchanged; wrapper
representation and constructor allocation change. No measured speed/RSS claim.
Evidence: `target/gc-validation/2026-10-16-atomic-ownership/`.
The prior full debug workspace (741) predates this follow-up. Channel lifetime,
never-joined/raw resources, budgets and remaining readiness audits stay open.


Failed-admission handoff checkpoint (2026-10-16): ThreadHandoff owns the closure
env root before registration and releases/reuses its slot after failed admission.
Successful transfer disarms the token before publishing vacancy. The bounded
regression passes 128 failures across two heap modes with no retained envs or
dead registrations; fatal cleanup now covers four subcases. Final gates pass
741 full debug workspace checks (nine intentional ignores), 262 optimized
runtime units including armed detector plus nine doctests, three poisoned/verified
tests and the full sanitizer gate: 30 native runs / 104 runtime executions,
no reports. All 357 source/test inputs match; ABI/allocation source fingerprints
are unchanged from the prior checkpoint. No throughput/RSS improvement claimed.
Evidence: `target/gc-validation/2026-10-16-admission-handoff/`.
The prior full optimized workspace (740) predates this follow-up. Other admission,
outcome, native resource and performance/platform/embedding audits remain open.


Owning runtime registration checkpoint (2026-10-16): fixed leaked child
registrations after failed setup. Main and child owners retire registration
before boxed poll storage. Failed deregistration and explicit/automatic child
draining are fatal before unsafe storage/code retirement. Final gates pass 740
optimized workspace checks including the armed detector, nine remaining ignores;
261 debug runtime units plus nine doctests; two poisoned/verified regressions
with three fatal cleanup subcases; full sanitizer gate, 30 native runs and 103
runtime executions, no reports. All 357 source/test input hashes stayed unchanged.
Thread ABI and allocation entry-point bodies match the prior checkpoint.
Evidence: `target/gc-validation/2026-10-16-runtime-retirement/`.
Admission before ownership, native-resource lifetimes, typed outcomes and broader
performance/platform/embedding audits remain open; the goal is not complete.


Inspection-unwind retirement follow-up (2026-10-16): fixed coordinator poisoning
after safe inspection unwinds, which could prevent poll-target detachment. That
checkpoint passed 259 debug runtime units plus nine doctests, 259 optimized
runtime checks including the armed detector plus nine doctests, 17 integrations
and eight poisoned/verified unwind checks. Full-workspace counts below predate
this follow-up. Broader child/runtime unwind cleanup remains open. Evidence:
`target/gc-validation/2026-10-16-coordinator-unwind/`. The final sanitizer gate passes 30 native runs and
101 runtime executions, with no reports; all 356 source/test inputs remained
unchanged across final gates.

Joined-completion ownership checkpoint (2026-10-16): safe Thread metadata weakly
follows its traced Thread wrapper and retires only when joined and owner-dead.
All copy paths update owners before reset; GC does not wait on native joins.
Hash-indexed ownership excludes raw completion history from tracing, and a weak
shutdown worklist handles descendants despite pruning. Repeated/raw join
contracts remain documented in threads.md.

Joined-completion checkpoint: 735 full debug workspace checks, nine intentional ignores; 256
optimized runtime unit checks including armed detector plus nine doctests; 17
integrations; six poisoning/verification regressions; full sanitizer gate:
30 native runs / 99 runtime test executions, no reports. All 355 non-Markdown
source/test input hashes stayed unchanged across final gates. Source and logs:
`target/gc-validation/2026-10-16-completion-ownership/`. The earlier 727-check full
optimized workspace predates this change; no new full optimized workspace gate
is claimed. Never-joined records, native AtomicI64/channel retention, remaining retirement
unwind cleanup and remaining RSS/latency/readiness audits stay open.


Bounded telemetry checkpoint (2026-10-16): recent history is capped at 1,024 events
with independent lifetime aggregates, exact short-run quantiles and bounded
histogram estimates with explicit accuracy metadata for longer runs. Full traces
stream from startup through bounded buffers. Both sinks are finalized despite
errors; failures are visible, persistent and do not trigger a buffered retry at
heap teardown. Recent snapshot API semantics and embedding contracts are in
GC_PERFORMANCE.md.

Final source: 727 optimized workspace checks including the separately armed
detector; 250 debug runtime unit checks plus nine doctests and 14 targeted
integrations; full sanitizer gate: 30 native runs and 93 runtime test executions,
no reports. All 354 non-Markdown validation-input hashes stayed unchanged across
the final gates. Source, logs, five negative controls and the teardown regression:
`target/gc-validation/2026-10-16-bounded-telemetry/`. The older 716-check full debug
checkpoint predates this change; no new full debug workspace run is claimed.
Event-history growth is resolved. Native/RSS and application-latency budgets and
the remaining readiness audits stay open.


Allocation-profile saturation checkpoint (2026-10-16): native/generated updates,
live/retired merges and report totals saturate at u64::MAX. Boundary regressions
exercise actual generated TLAB allocation before/after optimization and fail
with old updates restored. The safepoint fixture now requires a major through
live promotion pressure instead of relying on dead nursery pressure.

Full workspace debug: 716 passes, nine intentional ignores. Optimized runtime:
242 unit checks including the separately armed detector, plus nine doctests;
optimized generated-counter boundary: one pass. Full sanitizer gate: 30 native
runs and 85 runtime test executions, no reports. This does not constitute a full
optimized-workspace gate. Evidence and matching source archive:
`target/gc-validation/2026-10-16-allocation-profile-saturation/`. Event retention
was subsequently repaired above; long-run operating budgets and the remaining
GC_READINESS audits stay open.


GC telemetry arithmetic follow-up (2026-10-16): duration conversion and cumulative
GC timing/reclaimed/promoted totals saturate at u64::MAX (documented lower bounds)
instead of wrapping or panicking. Both boundary regressions fail with the old
arithmetic restored. Validation: 241 debug runtime unit tests, nine runtime
doctests and five optimized telemetry tests. Evidence:
`target/gc-validation/2026-10-16-telemetry-overflow/`. The subsequent checkpoints
repair event retention and allocation-site overflow.


Combined heap-peak repair, 2026-10-16: `peak_heap_bytes` now retains combined
active old/nursery occupancy sampled under one pause, including no-GC inspection
and final concurrent-pause growth. The benchmark label is “sampled heap peak”;
the sample includes TLAB reservation slack and excludes inactive copy space/RSS.
Negative controls catch omitted young occupancy and final-pause samples. Passed
239 debug and 239 optimized runtime checks including detector, 11 integrations
and the complete sanitizer gate (30 native/77 runtime executions), no reports.
Source/failure/pass checkpoint:
`target/gc-validation/2026-10-16-combined-heap-peak/`. Event retention and continuous
resource budgets remain open; older full-workspace evidence predates this change.

GC telemetry repair, 2026-10-16: concurrent pause metrics exclude the running-copy
interval; additive elapsed-cycle fields preserve that duration in GC logs,
runtime metrics and `gcr bench`. Text/JSON percentiles now share nearest-rank
indexing. Actual delayed-copy and rank negative controls fail with the old
calculations. Passed 236 debug and 236 optimized runtime checks including detector,
11 benchmark/log integrations and the full sanitizer gate (30 native/74 runtime
executions), no reports. Source/failure/pass evidence:
`target/gc-validation/2026-10-16-gc-telemetry/`. Event-history retention and combined
heap accounting remain open; older full-workspace gates predate this repair.

Generational pressure matrix, 2026-10-16: the mutation ledger now combines
automatic minor collection and concurrent majors on a 1 KiB nursery/4 KiB old
heap, alongside its semi-space case. Three seeds, 1/2/4/8 workers and five trials
passed 120 heap-mode runs: 30,720 concurrent cycles, 1,076,731 allocations and
12,284 minors. All 233 debug runtime tests and targeted allocation sanitizer
tests passed. Only test coverage changed; production build inputs match the
preceding full source checkpoint. Delta/counter/log evidence:
`target/gc-validation/2026-10-16-generational-pressure/`. Long-running resource
and latency budgets remain open.

Promotion-liveness repair, 2026-10-16: dead nursery occupancy no longer forces
an unnecessary major. When promotion does need old-space reclamation, a complete
root closure filters nursery roots for the preceding major, preventing dead young
wrappers from retaining old garbage and causing false exhaustion. Major/minor
remain one pause; exact raw/tagged Compact/Full graph and event checks pass,
including verifier/poison runs. Passed 233 debug and 233 optimized runtime checks
including detector, 18 integrations and the full sanitizer gate (30 native/71
runtime executions), no reports. Source/failure/pass checkpoint:
`target/gc-validation/2026-10-16-promotion-liveness/`. Larger pressure soaks and
pause/scratch-memory budgets remain open; older full-workspace gates predate it.

Allocation-boundary coverage, 2026-10-16: exact-fit and one-byte-over payloads
now verify full rounded nursery placement. A constructor with no leading pointer
slots verifies its sole embedded reference survives minor and repeated major
collection; omitting embedded-reference remembering fails the regression.
Passed 229 debug runtime tests and six targeted optimized/sanitizer cases each,
no reports. Production runtime/heap hashes match the preceding full sanitizer
checkpoint; only tests changed. Retained delta/parent evidence:
`target/gc-validation/2026-10-16-allocation-boundaries/`.

Oversized allocation follow-up, 2026-10-16: runtime objects too large for the
nursery now use available tenured storage without futile collection attempts.
Exhaustion retains nursery cleanup before old-space reclamation. Fresh
reference-bearing old objects are remembered for plain constructor stores.
Four regressions cover count reductions, independent young edges, reclamation,
dead nursery back edges and stress; negative controls reproduce lost edges/OOM
or unnecessary collections. Passed 227 debug and 227 optimized runtime checks
including detector, 18 integrations and the full sanitizer gate (30 native/65
runtime executions), no reports. Source/log checkpoint:
`target/gc-validation/2026-10-16-oversized-allocation/`. No elapsed-time speedup
or production-budget claim; earlier full-workspace evidence predates this change.

False-OOM repair, 2026-10-16: allocation exhaustion no longer treats a competing
concurrent cycle as sufficient when SATB floating garbage leaves no room for the
requested object. It performs a fresh collection in that case and still coalesces
when room is available. Deterministic 4/8 KiB regressions check both paths; the
old branch reproduces OOM. Forty optimized pressure runs covered 10,240 concurrent
cycles and 328,923 allocations. Passed 223 debug and 223 optimized runtime checks
including detector, 21 integrations, the full sanitizer gate (30 native/61 runtime
executions) and expanded-case sanitizer checks. Source and failure/pass evidence:
`target/gc-validation/2026-10-16-allocation-capacity/`. Broader older full-workspace
results predate this repair; long soaks and production budgets remain open.

SATB consumption follow-up, 2026-10-16: the final pause now consumes queued
chunks directly and drains local residual values in place, avoiding flattened
payload copies and local replacement allocations. An actual-copy test now
requires a SATB-only 24-byte object to survive; skipping queue consumption fails
with exactly 24 bytes missing. Passed 221 debug and 221 optimized runtime checks
including detector, eight integrations and 44 collector-filter sanitizer tests,
no reports. Source/log checkpoint:
`target/gc-validation/2026-10-16-satb-consumption/`. Whole-cycle memory and pause
performance budgets remain open; broader earlier gates predate this change.

SATB buffering repair, 2026-10-16: internal reference stores/CAS now use the same
threshold-flushing path as MutatorThread barriers, and local drains preserve
reserved capacity. Actual-copy overflow and capacity regressions pass; restoring
either old behavior fails its negative control. Passed 219 debug runtime tests,
219 optimized runtime checks including detector, nine selected integrations and
the full sanitizer gate (30 native and 57 runtime executions), no reports.
Retained source/log checkpoint: `target/gc-validation/2026-10-16-satb-chunks/`.
This bounds local chunks; whole-cycle SATB memory and throughput budgets remain
open. The earlier full-workspace checkpoint predates this repair.

Tagged-reference matrix, 2026-10-16: randomized Compact/Full major/minor graph
checks now cover raw and low-bit-tagged references, exact tag preservation and
unchanged immediate/null roots. All eight bounded cases passed optimized runs
at 1/2/4/8 workers and ThreadSanitizer. Dropping relocation tags makes all four
tagged cases fail. After restoration, all 218 debug runtime tests passed with
four ignores. Only test coverage changed; the collector matches the preceding
full-workspace checkpoint. Evidence is retained under
`target/gc-validation/2026-10-16-tagged-graph/`; other pointer encodings and
concurrent publication remain open.

Full workspace checkpoint, 2026-10-16: **687 debug checks passed**, zero
failures, nine ignores. The complete optimized workspace passed **686 checks**,
zero failures, ten ignores; explicitly enabling the release heap-corruption
detector added one pass, for **687 optimized checks**. Remaining ignores are four
explicit benchmarks and five documentation examples. These totals include the
compiler, runtime, integration and documentation suites. Source hashes stayed
unchanged throughout optimized validation. Retained source archive, command and
environment details, exact counts, ignore inventory and hashed logs are under
`target/gc-validation/2026-10-16-workspace-checkpoint/` (`report.json`). This
supersedes the older full-workspace count without closing the open concurrent
publication, embedding/platform/soak or production performance requirements in
[GC_READINESS.md](GC_READINESS.md).

Aggregate measurement follow-up, 2026-10-16: three source-archived paired
comparisons measured a median 2.233× improvement for the uncontended idle path
over always-blocking acquisition (250,000 two-word updates, exact result checks).
Load was elevated at 16.0–22.2 on 12 CPUs; active copying, contention and
application budgets remain open. The runner now accepts explicit workload
selection; its defaults are unchanged. Final restored source passed all 214
debug runtime unit tests, with four ignores including the new benchmark.
Commands, raw samples and limitations: [GC_PERFORMANCE.md](GC_PERFORMANCE.md).

Thread-handoff storage repair, 2026-10-16: cleared environment handoff roots are
now reused per heap, so root-table growth follows peak simultaneous handoffs
rather than historical spawns. The child roots the relocated environment before
clearing and recycling its slot. A bounded regression forces 64 child collections
and checks one extra slot; append-only allocation fails that regression. Passed
214 full debug runtime unit tests, the optimized regression, ten selected thread
integrations and the complete sanitizer gate (30 native executions, 52 runtime
test executions), without reports. This is a root-storage/scanning bound; no
application speedup is claimed. Completion-handle ownership retains its existing
execution lifetime.

String-join repair, 2026-10-16: mutable Array<String> entries now load through
canonical scoped SC access before immutable bytes are copied. A scheduled
eight-cycle regression reproduces the old behavior (`leftleftleft` instead of
`rightleftright`) when direct source-array loads are restored. The fixed version
passed debug/optimized runtime regression and ThreadSanitizer, 213 full debug
runtime unit tests, and 12 string/prelude integrations. This closes that access
gap; the broader concurrent publication and performance requirements remain open.

Promotion-oracle follow-up, 2026-10-16: the seeded graph matrix now also checks
minor promotion, remembered edges and following major reclamation for both
headers. It verifies graphs before and after the major and separately models
promotion through otherwise unreachable tenured objects. All four graph cases
passed optimized verification at 1/2/4/8 workers and ThreadSanitizer. Omitting
dirty cards makes both promotion cases fail at seed 1, cycle 1. After restoration,
the complete debug runtime suite passed 213 tests, zero failures, three existing
ignores. The graph matrix now covers 576 collection calls per invocation. Its
concurrent mutation/publication and long-soak limits remain open.

Seeded graph follow-up, 2026-10-16: an independent logical-ID oracle now checks
192 major collection cycles per invocation, alternating stop-the-world and
concurrent paths with Compact/Full headers, embedded references and wide arrays.
Optimized verification passed at 1/2/4/8 workers; ThreadSanitizer passed without
reports. A deliberately omitted embedded-reference descriptor made both header
tests fail on stale edges, confirming the oracle detects that tracing defect.
After restoration, the full debug runtime suite passed 211 tests, zero failures,
three existing ignores. Graph mutation occurs between collections; concurrent
publication, randomized promotion and long soaks remain open. Details:
[GC_READINESS.md](GC_READINESS.md#seeded-graph-oracle).

Aggregate-access follow-up, 2026-10-16: uncontended access while the collector
is idle now acquires its stripe without scratch-root publication or a managed
blocking transition. The try-acquire cannot wait; contention and active copying
retain the rooted blocking path and post-relocation stripe validation. The
running mutator prevents copying from starting before its next safepoint. SC
unlock ordering is unchanged. Passed 209 debug and 209 optimized runtime unit
tests (including the separately enabled detector), plus 18 selected integration
tests covering mixed SC aggregate snapshots in JIT/AOT and normal/stress modes,
generational collection, concurrency and operand rooting. The full sanitizer
script also passed: 30 instrumented native normal/stress executions and 47
runtime test executions, without reports. Throughput improvement
has not been measured; this change removes a known transition from the idle path.

Latest performance follow-up, 2026-10-16: parallel evacuation now retains bounded
local continuations (128 published-object tasks / 32 KiB copied work), while
large-copy batches and BUSY retries return to the shared queue. Producer
accounting includes retained work. Current checks passed: 209 debug runtime unit
tests; 209 optimized runtime unit tests including detector; nine compile-fail
doctests; eight selected optimized integrations; 43 sanitizer test executions
(40 distinct names) across the full parallel module and collector filters,
without reports. Final source matches the validated hash after paired experiments.

Three paired comparisons favored the candidate for small-object copying at four
and eight workers, but load rose to 42–75 on 12 CPUs and other results varied
sharply. This remains a load-qualified optimization candidate. Quiet confirmation,
generated access overhead and application latency/RSS budgets are open:
[GC_PERFORMANCE.md](GC_PERFORMANCE.md).

Earlier full-workspace verification passed **681 tests**, zero failures, eight
existing ignores, before subsequent test/header/queue follow-ups. It was not
repeated here; dated checkpoints distinguish current targeted evidence from
older full gates. General publication/runtime, metadata/code lifetime, generated
full concurrent mutation, embedding/platform/soak and production performance
requirements remain open: [GC_READINESS.md](GC_READINESS.md).

Historical checkpoint, 2026-10-06: **520 workspace tests passed**, zero failures,
eight existing ignores; **46 selected optimized tests passed**. Native
ThreadSanitizer passes all three new apps and four runtime/library fixtures in
normal/stress modes. Three bounded CSV/graph applications are implemented;
allocator/root contracts, enum payloads, integer parsing, native archive ownership
and benchmark error reporting have repairs with regression coverage. Ordinary
Vec/Map remain unsynchronized; separate concurrent collections remain planned.
See the dated checkpoints at the end for commands, limits and remaining work.

## Earlier assessment

Assessment and application work: 2026-10-03. Repository HEAD: `5c29ee7`;
the application and its supporting changes are in the working tree. Host: Darwin ARM64; Rust/Cargo 1.96.0. The committed baseline includes the prior widget,
`.gitignore`, and benchmark-artifact changes.

The reproduced defects below have fixes and regression coverage. Production
readiness remains unproven; remaining gates are in [PRODUCTION.md](PRODUCTION.md).

## Tests observed after fixes

`cargo test --workspace`: 427 passed, zero failures, five ignored runtime doc
examples. Application/module/concurrency release and CLI release verification: 58 tests passed with
`GCR_GC_VERIFY=1`. The preceding bug-fix baseline passed 41 release checks.

| Suite | Observed result |
| --- | --- |
| Compiler/frontend unit tests | 164 passed |
| Runtime unit tests | 110 passed; 5 runtime doc examples ignored |
| AOT executables | 4 passed |
| FFI | 19 passed |
| Operand rooting / stack / native transitions | 9 passed |
| Arithmetic boundaries / diagnostics | 5 passed |
| Project CLI / native stress selection | 6 passed |
| LLDB debugger / DWARF | 5 / 3 passed |
| Allocation profiling / benchmark command | 6 / 4 passed |
| Parser fuzz / GC ABI / generational | 5 / 1 / 4 passed |
| Heap command / diff / snapshot | 5 / 2 / 1 passed |
| Modules / reflection / stdlib integration | 8 / 14 / 9 passed |
| Checked I/O / parallel library / JIT and native search app | 7 / 1 / 3 passed |
| Comprehensive concurrency stress | Full default 50 iterations pass in debug and release |
| Shared-memory / threads / randomized thread stress | 15 / 13 / 3 passed |
| Example runner, bundled macOS Bash | 23 expected results pass in normal mode |
| Separate bounded stress example runs | 22 pass; full binary-tree stress run incomplete |

Validation commands:

```sh
cargo test --workspace
GCR_GC_VERIFY=1 cargo test --release --test search_app --test stdlib_parallel --test stdlib_io --test modules --test concurrency_stress --test project_cli --test aot --test ffi --test operand_roots
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

## Application milestone

[gcr-search](../apps/gcr-search/README.md) indexes a directory into a persisted
text snapshot and answers case-sensitive literal queries with JSON Lines output.
The application is written in gc-rust. Checked host I/O returns typed results,
closes native resources inside calls, and supports real argv. Project execution
forwards arguments after `--` without treating them as compiler flags. String joining now uses linear assembly.

This workload exposed and repaired two compiler defects: statement-ending
returns lost their diverging type in blocks/conditional branches, and layout
conversion discarded canonical module names in nested fields. Regression tests
cover returning match/if branches and module-defined struct/enum/container fields.

Native reference comparisons cover Unicode, newline-containing names, CRLF,
empty trees, binary/excluded files, symlink cycles, deterministic replacement,
source changes after indexing, malformed inputs, and truncated snapshots. These
run under collect-on-every-allocation GC with subprocess deadlines. Full-repository
index/query execution is observed in normal mode through JIT and AOT with identical
query output. Search uses bounded parallel batches and ordered output. Persistent
worker pools, watch mode,
posting indexes, and HTTP/UI remain future work.

## Further defects exposed by parallel library work

Value arrays previously allocated/indexed using inconsistent strides and omitted
embedded GC references. They now store value elements in individually traced
boxes; this preserves value semantics at the cost of an allocation per store.
Nested value-enum references now occupy shared leading pointer slots rather than
untraced union bytes. Their raw payloads are 8-aligned and sized from the actual
nested layouts. Closure environments include embedded captured-value references
in their GC metadata and align scalar capture storage consistently with codegen.

`Sync` generic bounds now accept the structural sharing rules used by spawn;
qualified nominal types remain canonical in structural checks. Named functions
can be passed as managed callbacks, including generics with contextual types.
Argument lowering preserves already-inferred generic hints, including partial
function signatures with a concrete input and inferred output.

Parser condition/scrutinee restrictions no longer leak into delimited call
arguments, groups, or closure blocks. Build options are parsed completely,
independent of output/debug flag order, and unknown or duplicate flags are
rejected. Native/JIT argv preserves non-UTF-8 program arguments so the checked
library reports errors; invalid compiler arguments receive a diagnostic.

Execution-local JIT arguments are inherited immutably by child threads. Explicit
manifest `run --jit` forwards program stdout and exit status like native runs;
native link configuration is rejected in that mode. Tests cover independent
embedding contexts, invalid UTF-8 arguments, nested enum-valued worker results,
worker-count bounds, and ordered Unicode output under stress in both backends.

The proposed target is shared mutable managed objects, SC managed accesses,
structured threads/tasks and cooperative async cancellation; see
[concurrency.md](concurrency.md). Current alias/higher-order capture checks are
inconsistent and ordinary codegen does not meet the race-safety contract. Sync
restrictions remain transitional. Root-owned completion state now supports
repeated/concurrent joins; see the 2026-10-04 checkpoint below.

## Implemented, with limits

The implementation contains monomorphized generics, value aggregates, heap
structs/enums, traits, closures, Option/Result, strings, Vec/HashMap, modules,
Thread<T>, atoms/atomics/channels, generational GC, and source/heap tooling.
Passing component and stress tests support those particular behaviors.

Nested, tuple and struct patterns and exhaustiveness are now implemented; the
October 12 method-prelude checkpoint is recorded in the project status pad.
FFI aggregate handling
uses AAPCS64 classification and rejects aggregate returns larger than 16 bytes.
The manifest handles metadata and native linking, not managed dependency
resolution. These are unsupported capabilities, not closed bug fixes.

Heap sizes are startup-configurable: 16 MiB nursery and 256 MiB tenured setting
by default. Allocation exhaustion aborts. Array/Vec indexing and legacy
array_get_unchecked now check bounds and unwritten reference/value slots.
The privileged native-handle/RawPtr safety boundary remains open. Defined
float-to-integer casts and checked char casts are described in
[SAFETY.md](SAFETY.md); general allocation size/offset arithmetic still needs audit.

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

## Concurrency contract work (2026-10-03)

[concurrency.md](concurrency.md) proposes version 1 semantics and records runtime
implementation gaps. `tests/concurrency_contract.rs` adds synchronized mutable
payload publication via start/join, channels and atomic signaling, checked in
JIT and AOT with moving-GC stress. It does not certify arbitrary managed races.

An additional lowering issue surfaced while constructing that fixture: an
inferred spawn closure with `if condition { return 81; }` followed by mutations
and a final `0` reports `expected (), found i64` at the early return. Rewriting
the body as an integer-valued if/else compiles. This inference issue was repaired at the 2026-10-04 checkpoint below;
it is separate from the concurrency memory-model implementation.

The new publication test passes in debug and release (`GCR_GC_VERIFY=1`), each
executing both JIT and native AOT with GC stress. Commands:
`cargo test --test concurrency_contract` and
`GCR_GC_VERIFY=1 cargo test --release --test concurrency_contract`.
The previously recorded 427/58 totals precede this added test.

## Execution ownership and closure inference — 2026-10-04

Checkout: `3a3d579` plus local changes. Host: Darwin ARM64, rustc 1.96.0.

`Thread<T>.join()` and scalar `thread_join` now support repeated and concurrent
observations through aliases. Root-owned native completion records keep borrowed
handle addresses stable. The first observer consumes the OS handle exactly once;
later observers acquire the completion mutex and obtain its published outcome.
All managed observers enter the GC blocking protocol before acquiring that mutex.
Managed result cells remain traced through surviving Thread aliases, preserving
reference identity and value-result copies.

JIT and AOT drain root children and newly spawned descendants before diagnostics
and code retirement. Root exit clears retired main frames and parks its mutator,
so child collection can proceed while draining. Completion metadata is released
at that boundary, even if an embedder keeps the heap. Metadata grows with total
spawns until root exit; nested scopes and earlier reclamation remain future work.
A nonterminating child can delay shutdown indefinitely. Fallible spawn, typed
failure/cancellation and the broader managed race-safety contract remain pending.
Native panic retains its previous scalar-zero behavior.

Closure lowering now infers a return constraint from explicit returns and checks
it against subsequent returns and the body result. Nested closures save/restore
that constraint. The original spawn-closure early return executes correctly;
return-only branches and incompatible value/unit returns have regression coverage.

Observed verification:

- `cargo test --workspace`: 434 passed, zero failures, five ignored runtime doc
  examples. Two runtime completion tests were added afterward and pass separately
  in debug and release; no new aggregate workspace total is claimed.
- `GCR_GC_VERIFY=1 cargo test --release --test thread_completion --test
  concurrency_contract --test concurrency_stress --test threads --test shared_mem
  --test threads_stress --test operand_roots --test search_app --test
  stdlib_parallel`: 52 passed. Comprehensive stress used its default 50 iterations.
- Final `cargo test --test thread_completion` and its release equivalent: six
  passed in each profile (four bounded JIT/AOT fixtures, incompatible-return checks,
  and the subprocess helper). The final repeat fixture retires an original alias
  before joining a surviving one.
- `cargo test -p gcrust-rt completion_tests` and
  `GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt completion_tests`: two passed
  each, covering completion before first join and native-record reclamation with
  an externally retained heap.
- Negative controls: restoring the original runtime and matching JIT entry path
  makes the repeat-join subprocess abort (`failed to join thread: No such process`).
  Disabling closure return inference reproduces `expected (), found i64`.
  Restored implementations pass the completion suite again.
- `git diff --check`: clean.

Existing cross-platform, sanitizer, service-soak and full binary-tree GC-stress
coverage gaps remain. The next implementation step is managed SC access,
aggregate coherence, collection semantics and GC/barrier coordination.

## Collector coordination audit and repairs — 2026-10-04

Checkout `3a3d579` plus local changes, preserving the preceding ownership/inference
work. Darwin ARM64, rustc 1.96.0. The proposed heap-wide managed-access mutex was
withdrawn before implementation; it is not part of these changes.

Repaired issues:

- Card dirtying used plain racing byte stores and an incorrect benign-race claim.
  Cards now use AtomicU8 relaxed loads/stores. Safepoint publication supplies the
  ordering before STW scanning. A test concurrently marks shared/distinct cards.
- The coordinator wrote the poll flag atomically, but JIT polls were plain volatile
  loads. Runtime flag storage is now AtomicU8 with unchanged ABI offsets; generated
  loads are atomic acquire and volatile. IR checks cover optimized/unoptimized code.
- Minor GC never raised other mutators' JIT polls, including the major-before-minor
  branch. The new allocation-free worker reproduction timed out after ten seconds
  before repair. Minor collection now requests every worker before waiting and
  clears polls before resumption. Promotion capacity is checked after the world
  stops; required major/minor phases share a pause instead of resuming/reparking.
- Major collection cleared destination cards even though copied tenured objects
  still referenced nursery objects. A native unit reproduction failed before repair:
  the next minor left the child's pointer in the reset nursery. Destination cards
  are now rebuilt while scanning copied objects, with no additional heap pass.
  Tests cover fixed, interior and multi-card varlen references in both major paths.

Verification observed:

- `cargo test --workspace`: 440 passed, zero failures, five ignored doc examples.
- `GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt`: 113 passed, one detector
  test ignored by its release annotation, plus five ignored doc examples. Running
  that detector explicitly with `--ignored --exact` and GC verification passes.
- `GCR_GC_VERIFY=1 cargo test --release --test gc_safepoints --test gc_abi_smoke
  --test generational --test concurrency_stress --test threads --test threads_stress
  --test thread_completion --test shared_mem --test operand_roots --test
  concurrency_contract --test search_app --test stdlib_parallel`: 59 passed.
  Comprehensive stress retains its default 50 iterations. The safepoint fixture
  uses 1 MiB nursery/tenured settings, verifies both minor and major collections,
  and enforces deadlines for JIT/native subprocesses.
- `git diff --check`: clean.

These totals describe the coordination-repair checkpoint. The following parallel
collector checkpoint supersedes its collector implementation and target choice.

## Parallel collector checkpoint — 2026-10-04

User selected parallel stop-the-world moving collection with concurrent application
mutators. Major and minor evacuation now use the same parallel engine. Collector
workers claim objects before allocating a destination copy, preserve the original
header without non-atomic access to its claimed word, and release-publish forwarding
addresses. Only the copy owner queues scanning. Batched queue transfers amortize
locking; termination requires an empty queue and no active producer batches.
Idle workers sleep. Dirty-card objects are deduplicated before minor scans, and
major scans preserve destination nursery cards. All worker joins precede space
swap, nursery reset and mutator resumption. Failed cycles cancel queue waiters and
propagate their panic; partially relocated heaps must not resume execution.

Destination reservations omit zero-fill before copying. The shared allocator's
cursor rounding/addition now checks overflow; a regression confirms an exhausted
cursor cannot wrap back into live objects. No external-project bug encountered.

`GCR_GC_WORKERS=N` sets total workers including the coordinator. Default: available
CPU parallelism capped at eight, with coordinator-only collection below 256 KiB
in the collected generation. Helpers are scoped OS threads per parallel cycle.
Details and explicit remaining costs are in [gc.md](gc.md).

Verification on Darwin ARM64, rustc 1.96.0:

- `cargo test --workspace`: **445 passed**, zero failures; one explicit throughput
  benchmark and five documentation examples ignored.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt --lib`:
  **118 passed**, zero failures, two ignored (throughput benchmark and annotated
  detector). The detector passes when explicitly selected with `--ignored --exact`.
  Runtime suites also passed with one and two workers during implementation.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release --test gc_safepoints
  --test gc_abi_smoke --test generational --test concurrency_stress --test threads
  --test threads_stress --test thread_completion --test shared_mem --test operand_roots
  --test concurrency_contract --test search_app --test stdlib_parallel`:
  **59 passed**, zero failures, including default comprehensive stress iterations.
- New checks cover eight-way copy contention with exactly one destination,
  Full/Compact headers and byte payloads, cyclic/shared graphs with exact live
  occupancy through repeated major collections, duplicate root registrations,
  outstanding-work termination, and tagged cyclic nursery promotion from a
  multicard old object followed by major collection.
- `git diff --check`: clean.

Copy-heavy measurement:
`GCR_GC_WORKERS=N cargo test --release -p gcrust-rt parallel_copy_throughput --
--ignored --nocapture`, separately for N=1,2,4. The fixture has a single root into
4,095 binary-tree objects with 16 KiB byte payloads (67,256,280 live bytes).
Nine collections per run; discard two warmups and report the median of seven.

| Workers | Median collection pause |
| --- | --- |
| 1 | 2.316 ms |
| 2 | 2.086 ms |
| 4 | 1.674 ms |

Four workers improve this fixture's median by approximately 28% (1.38×). This is
one warmed copy-heavy microbenchmark, not evidence of general mutator scaling,
small-object throughput, or workload tail latency. Logs: `/tmp/gcr-parallel-copy-1.log`,
`/tmp/gcr-parallel-copy-2.log`, `/tmp/gcr-parallel-copy-4.log`.
Verification logs: `/tmp/gcr-parallel-final-workspace.log`,
`/tmp/gcr-parallel-final-runtime.log`, `/tmp/gcr-parallel-final-release.log`,
`/tmp/gcr-parallel-final-detector.log`.

Remaining work: persistent collector workers; worker-local copying allocation
and mutator TLABs with accurate extents/tails; broader scaling measurements;
generated SC managed accesses and coherent aggregate snapshots. The experimental
concurrent moving machinery remains unsupported. No sanitizer certification or
arbitrary managed-race safety is claimed by this checkpoint.

## Persistent workers and batched copying — 2026-10-04

Supersedes the scoped-worker/per-object-reservation implementation above.
Each heap lazily owns a persistent collector worker pool. Dispatch lends a job to
helpers synchronously and does not return or propagate a job panic until every
participant acknowledges completion. Worker counts may change between cycles;
excess helpers remain idle. Heap teardown shuts down and joins all helper handles.
No helper holds an owning heap reference. Tests cover worker reuse, changing
participation, coordinator/helper panic handling, and actual thread teardown.
Startup failures occur before job publication and release dispatch locks so
already-created helpers can shut down normally.

Workers reuse their batch/discovery/claim/layout/address buffers within a cycle.
They gather exclusive copy claims, reserve exact destination extents in one cursor
CAS per batch, initialize/publish copies, then publish discovered work. References
to BUSY copies are deferred as tasks rather than blocking with unpublished claims.
A deterministic two-worker test forces crossing claims; neither worker waits on
the other before publishing. Reservation failure leaves the destination cursor
unchanged. This avoids unused copying-buffer tails and preserves exact live sizes.

The new small-object benchmark exposed a genuine allocation-contention regression
that the earlier byte-copy benchmark had hidden. Persistent workers with per-object
reservation measured 4.755/11.249/30.387 ms at one/two/four workers on a single-root
262,143-node tree (10,485,720 live bytes). Batching changed this to
5.654/4.489/3.760 ms: an approximately 88% reduction at four workers, with about
19% additional single-worker overhead. Both measurements are recorded in the pad
cell `collector-allocation-regression-2026-10-04`.

Release microbenchmarks with persistent workers and batch reservation:

| Workers | 64 MiB payload graph | 10 MiB small-object graph |
| --- | --- | --- |
| 1 | 2.231 ms | 5.654 ms |
| 2 | 1.391 ms | 4.489 ms |
| 4 | 1.320 ms | 3.760 ms |

The four-worker speedups relative to the current one-worker backend are 1.69×
and 1.50× respectively. These are warmed fixture medians, not application pause
percentiles or general allocation-scaling claims. Command:
`GCR_GC_WORKERS=N cargo test --release -p gcrust-rt throughput -- --ignored
--nocapture --test-threads=1`, separately for N=1,2,4; nine collections per fixture,
two warmups discarded, median of seven. Logs: `/tmp/gcr-pool-bench-{1,2,4}.log`
(before batching), `/tmp/gcr-batch-bench-{1,2,4}.log` (after batching).

Verification:

- `cargo test --workspace`: **450 passed**, zero failures; two explicit throughput
  fixtures and five documentation examples ignored.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt --lib`:
  **123 passed**, zero failures, three ignored (two benchmarks and the annotated
  detector). The prior explicit detector run remains applicable; its implementation
  is unchanged.
- The same 59 selected release JIT/AOT checks listed above, with four workers and
  GC verification: **59 passed**, including default comprehensive stress iterations.
- One/two-worker release runtime suites pass as well. Pool-specific tests pass
  after the final startup-failure cleanup change.
- `git diff --check`: clean.

Logs: `/tmp/gcr-persistent-final-workspace.log`,
`/tmp/gcr-persistent-final-runtime.log`, `/tmp/gcr-persistent-final-release.log`,
`/tmp/gcr-persistent-runtime-{1,2}.log`, `/tmp/gcr-persistent-pool-final.log`.

Remaining costs/work: shared queue synchronization, remaining batched cursor
contention, root/card enumeration, cold pool startup, copying buffers with local
cursors, mutator TLABs with initialized extents/tail accounting, generated SC
managed accesses and coherent aggregate snapshots. No external-project bug was
encountered, and no sanitizer certification is claimed.

## Mutator TLABs and initialized allocation ranges — 2026-10-04

Runtime JIT/AOT allocation calls and Rust MutatorThread allocations now use
thread-local buffers for objects up to 8 KiB. Buffer targets start at 2 KiB,
grow on refill to a 32 KiB cap, accommodate larger current objects, and shrink
to remaining capacity. Large objects and public Heap allocation retain exact
shared reservations. Collect-on-allocation stress bypasses buffering.

Each buffer records an initialized prefix; active and retired tails are excluded
from heap walks. Descriptors survive mutator teardown until allocator reset.
Reset generations and allocator identity invalidate stale local buffers across
nursery reset and space flips. Major collection preserves nursery buffers and
updates their initialized references. Snapshot walks, nursery-as-major-roots
scanning, prewalk validation and remembered-card indexing honor these boundaries.
Physical occupancy includes reserved tails; initialized walks and allocation-site
bytes exclude them. No generated-code ABI changed.

Rust MutatorThread is now neither Send nor Sync, enforced by its type and a
compile-fail test. Its roots, profiling and local cursor belong to the registering
OS thread. A remaining low-level ThreadState ownership API issue is recorded in
the project pad (`thread-state-owner-api-audit-2026-10-04`): public ownership-sensitive
operations still need a complete API audit/enforcement pass. Runtime call sites
use the owning thread; the handle restriction alone is not certification of all
low-level embedding APIs or arbitrary managed races.

Two allocation/GC bugs found and repaired:

- AllocWindow exported a cursor address stored inline in a movable allocator.
  The cursor now has boxed stable storage; a move regression verifies its exported
  address and reads it after allocation. Window ABI is unchanged.
- Dirty-card indexing could skip an object crossing a leading card boundary when
  another object began later in that card. A native reproduction failed before
  repair and passes afterward. Index entries now identify the first initialized
  overlapping object in each card. Evidence: `/tmp/gcr-card-boundary-negative.log`,
  `/tmp/gcr-card-boundary-fixed.log`.

Final validation:

- `cargo test --workspace`: **457 passed**, zero failures. Three explicit benchmarks
  and five documentation examples ignored; the ownership compile-fail test passes.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt --lib`:
  **129 passed**, zero failures, four ignored (three benchmarks and the annotated
  detector, whose unchanged implementation passed explicitly earlier).
- Expanded selected release checks with four workers and GC verification:
  **73 passed**. The prior 59-check selection plus `heap_cmd`, `heap_snapshot`,
  `heap_diff`, and `alloc_profile` covers snapshots, CLI heap walks and profiling.
- Explicit rooted-chain regression passes with `GCR_GC_VERIFY=1 GCRUST_PREWALK=1`,
  exercising external major/prewalk entry as well as mutator major/minor paths.
- Tests poison active/retired tails, verify new fields are zeroed, invalidate
  partially used buffers after reset, preserve rooted chains across repeated
  major/minor cycles, check adaptive growth and stable cursor addresses, and
  reproduce the crossing-card reference loss.
- `git diff --check`: clean.

Logs: `/tmp/gcr-tlab-verified-workspace.log`, `/tmp/gcr-tlab-verified-runtime.log`,
`/tmp/gcr-tlab-verified-release.log`, `/tmp/gcr-tlab-prewalk.log`.

Allocation fixture after adaptive buffering and stable cursor storage:

| Workers | Objects allocated | Shared cursor | TLAB |
| --- | --- | --- | --- |
| 1 | 250,000 | 1.384 ms | 2.019 ms |
| 2 | 500,000 | 17.591 ms | 2.119 ms |
| 4 | 1,000,000 | 114.859 ms | 2.235 ms |

Each worker allocates 250,000 32-byte objects into a 64 MiB semi-space heap; no
collection occurs during timing. Both paths initialize objects. Five fresh-heap
runs per mode, median elapsed time; timing covers the synchronized allocation
phase and worker joins. Post-run heap walks verify the exact object count.
Command: `GCR_ALLOC_BENCH_WORKERS=N cargo test --release -p gcrust-rt
mutator_tlab_allocation_throughput -- --ignored --nocapture --test-threads=1`,
separately for N=1,2,4. Logs: `/tmp/gcr-tlab-adaptive-allocation-{1,2,4}.log`.
Four-worker TLAB time is about 51× lower than this contended shared-cursor baseline;
single-worker time is about 46% higher. This isolates allocation contention and
does not establish application speedups, GC tail latency or retained-heap scaling.

Remaining work: low-level owner/collector API enforcement, generated SC managed
accesses and coherent aggregate snapshots, collector-local copying cursors and
broader scheduling/workload measurements. No external-project bug encountered.

### Allocation stress audit (2026-10-04)

`--gc-stress` / `GCR_GC_STRESS=1` collect before every managed runtime
allocation and bypass TLABs. Fixed request coalescing under contention and
empty-nursery skips: each stress request now acquires its own collection pause,
servicing safepoints while waiting. The four-mutator regression asserts exactly
400 collections for 400 allocations in both semispace and nursery paths.
Restoring the old behavior fails the regression (188 versus 400 collections).
Raw heap allocation primitives remain noncollecting; embedding callers use
registered mutators. Release runtime validation with four collector workers
and heap verification: 130 passed, zero failures. Release JIT/native integration
checks cover project CLI stress, thread completion, threads, parallel library,
and GC ABI paths; all pass. Logs: `/tmp/gcr-every-allocation-runtime.log`,
`/tmp/gcr-every-allocation-integration.log`,
`/tmp/gcr-every-allocation-negative.log`.

Prebuilt final-source workspace run: **458 passed**, zero failures. Log:
`/tmp/gcr-every-allocation-workspace-prebuilt.log`. A preceding run exposed
a separate native-runtime archive rebuild race in concurrent CLI tests
(missing `libgcrust_rt.a` after a stale rebuild); recorded as open in status
pad cell `native-runtime-rebuild-race-2026-10-04`.

## Application and atomic contracts checkpoint — 2026-10-04

`apps/gcr-logstats` adds a real parallel UTF-8 log analyzer. Fixed workers claim
input occurrences using AtomicI64 fetch-add and publish immutable summary
references through Atom CAS. Counts include files, bytes, lines, ERROR lines and
WARN lines. Worker/file limits and errors are documented in its README. All
workers are joined; read failure produces no partial success summary.

Native AtomicI64 cells previously leaked via Box::into_raw. They are now stable
root-execution-owned Arc allocations, released after all children/descendants
drain, including when an embedder retains the heap. Aliases remain valid through
registry growth. Cells still accumulate until root exit; earlier reclamation is
pending for long-lived roots.

Atom<bool> previously emitted invalid atomic i1 load/CAS operations (LLVM verifier
reproduction: /tmp/gcr-atom-bool-before.log). Bool construction now initializes a
canonical full byte; operations use byte atomics and convert to/from language
bool. Float atoms use integer-bit atomic operations; CAS distinguishes signed
zero and matches unchanged NaN bits. Inline aggregates/unit get frontend errors,
and atom intrinsic arity is checked before indexing arguments. The scalar/type
regressions run JIT and native, normal and every-allocation GC, with bounded child
processes. These repairs do not implement general managed race semantics.

Current low-level ThreadState record_alloc and alloc_site_counters already have
unsafe signatures and documented owner/quiescence requirements. The earlier pad
finding about safe record_alloc no longer describes its current signature;
public frame_chain and the broader owner/collector API enforcement remain open.

Verification completed across midnight, 2026-10-04/05, on Darwin ARM64:

- `cargo test --workspace`: **462 passed**, zero failures; three explicit runtime
  benchmarks and five documentation examples ignored. The ownership compile-fail
  test passes. Log: `/tmp/gcr-app-final-source-workspace.log`.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release --test atom_types --test
  logstats_app --test shared_mem --test thread_completion --test search_app --test
  concurrency_contract --test concurrency_stress`: **29 passed**, zero failures;
  comprehensive stress uses its default 50 iterations. Log:
  `/tmp/gcr-app-final-source-release.log`.
- Atomic native-cell lifetime regression passes separately in debug and release;
  log `/tmp/gcr-atomic-lifetime-release.log`.
- A native gcr-logstats run over three actual compiler/test logs agrees with a
  serial byte/line reference (3 files, 11,249 bytes, 247 lines). Evidence:
  `/tmp/gcr-logstats-real-logs.json`. This is correctness evidence, not a benchmark.
- `git diff --check`: clean. After the test builds, an ineffective Inkwell
  cmpxchg alignment setter was removed; it always returned an ignored error, so
  generated alignment remains LLVM's natural operand alignment. The external
  accessor limitation is recorded in the project status pad.

Application milestones are in PLAN.md. General managed SC accesses/aggregate
coherence, low-level ownership enforcement, channel-close semantics, structured
outcomes/scopes, cancellation, async, the native archive rebuild race, and release
platform/soak gates remain open. No sanitizer or production certification claimed.

## Managed memory and GC handoff checkpoint — 2026-10-05

Generated mutable scalar/reference fields and array slots use naturally aligned
SC loads/stores, including byte storage for bool. Inline value-aggregate fields
use 64 address stripes per heap. Acquisition roots the object, parks while
waiting, reloads its relocated address and revalidates the stripe before copying.
The critical region performs only the complete aggregate copy and embedded
reference barriers. Unlock performs an SC RMW as the aggregate linearization
event. There is no allocation, safepoint or foreign call while a stripe is held.
Array value elements remain immutable traced boxes, with atomic slot publication.
Native channel slots and string-array reads use SC atomics; FFI scalar buffer
copies perform SC accesses per element (not a whole-array snapshot).

Plain accesses remain for privately initialized constructors, immutable
headers/counts/string bytes/captures/boxed value elements, and collector-only
relocation during a world pause. Arbitrary managed sharing is not yet certified:
collection metadata/structural mutation, unpublished or null reference slots,
unchecked library accesses and the low-level owner/collector root API remain
open. Transitional Sync capture checks have not been removed.

Two GC handoff defects are repaired. Stop requests arriving between ThreadState
registration and poll-flag installation are retained under a registration mutex.
Pause release clears polls, releases the collection request and resumes states
under the registry lock, before blocked mutators can resume or retire poll
storage. Three startup-order tests and a 32-round/16-worker retirement test cover
these interleavings. An earlier selected release stress run hit a native allocator
SIGTRAP; the retirement audit found a concrete defect, but the original trap's
cause is not independently established. The final full default stress passes.

Verification on Darwin ARM64:

- `cargo test --workspace`: **468 passed**, zero failures, eight ignored
  (three explicit benchmarks and five documentation examples).
  Log: `/tmp/gcr-managed-workspace.log`.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release --test managed_memory
  --test concurrency_stress --test concurrency_contract --test shared_mem
  --test thread_completion --test search_app --test logstats_app --test ffi
  --test operand_roots`: **56 passed**, zero failures. Stress uses its unchanged
  default 50 iterations. Log: `/tmp/gcr-managed-final-release.log`.
- An additional optimized/unoptimized LLVM structural check verifies atomic
  scalar loads/stores and aggregate regions without collecting calls; passed.
  Log: `/tmp/gcr-managed-ir-check.log`. It was added after the two suite builds.
- `scripts/check_thread_sanitizer.sh` instruments generated native code, the
  Rust runtime and rebuilt Rust std. Managed-memory normal/stress fixtures and
  four native poll/retirement regressions pass using Apple's external sanitizer
  runtime. JIT is covered by behavioral tests, not sanitizer instrumentation.
  This gate currently supports macOS ARM64 only.

Rust nightly's bundled sanitizer crashed during dyld initialization before any
native unit test ran. The external-runtime gate explicitly links Apple's runtime
because Rust supplies `-nodefaultlibs`; it retains full instrumentation and uses
no suppressions. Startup evidence and the ownership/collection backlog are in
the project status pad. The full implementation plan remains in progress.

## Collection boundary and initialization checkpoint — 2026-10-05

Legacy direct-return array reads now validate the actual immutable backing-array
length before emitting an inbounds element address. This protects prelude
Vec/Map reads when separately observed mutable metadata no longer describes the
array. Invalid reads abort with the existing bounds diagnostic. The legacy
`unchecked` names are retained but no longer bypass this check.

Array constructors reject negative lengths and multiplication overflow before
computing the allocator count. Variable-length allocation checks the complete
TypeInfo size, count/header overhead, alignment and isize pointer-offset limit.
A native unit test checks agreement with ordinary layout sizing across headers,
fields, byte/value tails and alignments, and rejects overflowing tails.

Unwritten reference and value-array slots are internal null storage, with no
universal source-language default. All generated read paths now fail with an
uninitialized-element diagnostic before a null reference or fabricated inline
aggregate can escape. Native string-array join uses the same rule. Scalar arrays
retain zero/false initialization. See language.md for the observable contract.
These are currently diagnostic aborts; typed failure propagation belongs to the
outcomes/structured lifetime work. Collection operations still are not atomic
transactions, and native channel storage ownership remains open.

Validation:

- `cargo test --workspace`: **470 passed**, zero failures, eight existing ignores.
  Log: `/tmp/gcr-collection-workspace.log`.
- Release selection with four collector workers and heap verification:
  collection_boundaries, managed_memory, concurrency_stress, ffi, search_app,
  logstats_app: **27 passed**, zero failures. Full default concurrency workload.
  Log: `/tmp/gcr-collection-release.log`.
- Allocation-size unit test passed separately; log:
  `/tmp/gcr-allocation-size-unit.log`.
- `scripts/check_thread_sanitizer.sh`: native generated managed-memory fixture
  passes normal/stress GC and all four poll/retirement regressions pass.
  Log: `/tmp/gcr-collection-tsan.log`. No JIT sanitizer coverage is claimed.
- `git diff --check`: clean.

The audit confirmed Channel::new(0) allocates no slots while native metadata
clamps capacity to one. Mutable buffer/control fields can also mix queue state
with unrelated storage. Canonical rooted native queue ownership, positive
capacity, resource retirement and proper close/outcome semantics remain required
in PLAN sections 2/3. Details are recorded in project status pad cell
`channel-backing-buffer-contract-2026-10-05`. The full goal remains active.

## Channel ownership and close checkpoint — 2026-10-05

Native fixed-address queue slots replace the managed-buffer/control split.
All major/minor collector paths and heap root enumeration scan these slots.
The managed zero-length witness array carries only the generic type; modifying
it cannot change queue storage. Runtime array/message type checks prevent valid
handles from different generic channels from causing representation confusion.
Capacity must be positive and representable. Public Channel<T> boxes initialized
scalar, reference and inline-value messages; the previous scalar-channel codegen
pointer cast failure is removed.

The public signatures change to `send(T) -> Result<(),T>` and `recv() -> Option<T>`.
A closed send returns the original value, preserving reference identity. Receive
reports None only after close and draining. `recv_value()` is a convenience for
protocol-required messages and fails explicitly on closed/drained queues. Close
is an idempotent state transition with notifications to every sender/receiver.
FIFO order and close/send commitment linearize under the queue state mutex.
Synchronized waiter counts let tests prove participants actually parked before
closing. No GC finalization is needed for close.

Closed drained slot storage is reclaimed immediately. Slot-storage locking occurs
only in RUNNING, noncollecting regions, so root enumeration under a world pause
can inspect it without waiting for a parked owner. Stable handle metadata stays
root-execution-owned until children drain; a weak-ownership regression verifies
actual reclamation with an externally retained heap. Earlier metadata reclamation,
async/cancellation-aware channels, execution outcomes/scopes and locks remain open.

The unchanged selected release workload passes **29 tests**, with four collector
workers and heap verification: channel_contract, collection_boundaries,
concurrency_contract, concurrency_stress (default full 50 iterations), shared_mem,
search_app, logstats_app and thread_completion. Native ThreadSanitizer passes the
managed-memory and complete channel fixtures in normal/every-allocation GC, all
four poll regressions and the native channel retirement test. The channel fixture
covers two actually blocked senders/receivers, repeated close, a 100-message FIFO
pipeline, 64 close/send races, reference identity, inline values/floats/bools,
witness replacement and message roots surviving moving GC. The example channel
program still reports 109900.

Logs: `/tmp/gcr-channel-final-release.log`, `/tmp/gcr-channel-final-tsan.log`,
`/tmp/gcr-channel-example.log`. The first workspace attempt failed five CLI tests
with ENOENT during concurrent replacement of target/debug/gcr by the sanitizer
compiler build; it is not counted as a pass. The sanitizer gate now builds its
compiler in target/tsan-compiler, isolated from in-flight workspace executables.
Final workspace validation is recorded below once complete. The full PLAN goal
remains active.

Final channel workspace run: **472 passed**, zero failures, eight existing ignores.
Log: `/tmp/gcr-channel-final-workspace.log`. A subsequent frontend-only hardening
requires RawPtr control arguments for close/clone/drop and adds malformed-call
regressions; the focused channel suite passes separately, including the unchanged
JIT/AOT normal/stress fixture. Log: `/tmp/gcr-channel-frontend-final.log`.

## Root registration and runtime retirement checkpoint — 2026-10-05

Raw root enumeration is now an unsafe trait operation with explicit storage,
quiescence and pointer-policy requirements. Raw stack-frame/TLS registration is
unsafe because forgetting its guard can outlive stack storage; callback APIs keep
the guards private and clean up on unwind. Owned scopes unlink their exact frame,
including when scopes are dropped out of registration order.

ThreadState root storage is private and owner-checked. Scratch tokens cannot move
between threads; publication/reset, GC state transitions, allocation and polling
entry points now state their unsafe caller obligations. Classic Mutator allocation
and SATB reference publication likewise require valid layouts/references.
Deregistration detaches the poll byte before native Thread storage is freed, even
when an external Arc retains the ThreadState. Regressions exercise wrong-owner
access, unwind cleanup, scope unlinking and retained-state poll operations.

This does not complete the low-level allocation/collector API audit or collection
operation consistency. Sync capture restrictions remain. Final validation for this
checkpoint is recorded in the project status pad after the running gates finish.

Validation after the root API changes: `cargo test --workspace` passes **480
 tests**, zero failures, eight ignored (three benchmarks and five documentation
 examples). The optimized channel/boundary/concurrency/shared-memory/search/logstats/
 completion suites pass **30 tests**. `scripts/check_thread_sanitizer.sh` passes
 managed-memory and channel fixtures in normal/stress GC plus six poll regressions
 and the channel metadata retirement regression. Native AOT is instrumented;
 JIT sanitizer instrumentation remains unavailable. `git diff --check` is clean.

## Raw allocator extent checkpoint — 2026-10-05

Legacy bump, shared atomic bump and TLAB reservations validate descriptor/length
arithmetic before reserving memory. Bump cursor padding and end calculation use
checked additions. Invalid sizes/alignment shifts return null without advancing
cursors or publishing buffers. Two native regressions check rejection, unchanged
initialized prefixes and successful subsequent allocations. The wider allocator
alignment/header and collector ownership API audit remains open.

## Alignment and reset checkpoint — 2026-10-05

Legacy bump allocation now aligns absolute addresses and records object starts so
walking skips mixed-alignment padding. Existing over-alignment support is preserved;
a mixed ordinary/64-byte-aligned externally backed arena has a traversal/reset
regression. Atomic managed arenas and TLABs accept eight-byte alignment and reject
other descriptors before reservation. Batch validation rejects unsupported alignment,
zero sizes and non-word extents. Checked TypeInfo sizes reject sub-word alignment.
Arena reset now has an unsafe exclusive-access/retired-allocation contract.
The broader collector API and collection consistency audit remains open.

Validation: **147 runtime unit tests and three compile-fail documentation tests
pass**, zero failures; three benchmarks/five examples ignored. Five optimized
allocator/layout regressions and eight optimized application/concurrency checks
pass. Application checks include JIT/AOT serial-model comparisons under moving-GC
stress. `git diff --check` is clean. Exact commands/logs are in the status pad.

## Allocator/collector contracts and collection policy — 2026-10-06

Ordinary Vec/HashMap remain unsynchronized collections. Shared mutation,
including through backing-array aliases, requires external synchronization.
Concurrent reads require safe publication and unchanged storage. Unsynchronized
operations have no collection-level consistency guarantee; the managed-access
and boundary checks still prevent native out-of-bounds access, fabricated
references and torn aggregates. Concurrent versions are separate planned APIs
following locks and structured lifetimes. PLAN/concurrency/language documentation
and prelude comments now reflect this direction.

Header initialization validates physical size, alignment, type-id offset and
allocation layout before reservation. ObjHeader is an unsafe representation
contract. Descriptor validation rejects malformed headers/interior-pointer
slots, and heap allocation requires the registered descriptor. Invalid mutator
requests return null before stress GC or TLAB publication. Zero-sized arenas
are rejected before native allocation. Type registration uses exclusive &mut
Heap access, validates layouts, assigns ids and checks exhaustion; mutation of a
plain Vec through a shared reference is removed. Direct heap allocation, frame
walker installation and allocation-window remapping now carry explicit unsafe
coordination/storage contracts.

The legacy collector scans exact object starts across alignment gaps and uses
the first header word for forwarding. New cyclic-graph and Full-header regressions
exercise these fixes through relocation. Dynamic root frames use actual Cell
storage, rather than casting plain u64 storage to mutable cells. Escaping guards
require unsafe lifetime management; callback registration keeps the guard
private, rejects duplicate registrations and cleans up on unwind.

Validation:

- `cargo test --workspace`: **498 passed**, zero failures, eight existing ignores.
  Log: `/tmp/gcr-contract-workspace.log`. A subsequent same-size/different-type-id-
  offset header regression passes separately in debug and release. Its final
  runtime-suite evidence is recorded in the project status pad.
- Optimized JIT/AOT selection with four collector workers and heap verification:
  **35 passed**, zero failures. Includes the full default concurrency workload,
  channel/boundary/managed-memory/completion tests, FFI, search and logstats.
  Log: `/tmp/gcr-contract-release.log`.
- `scripts/check_thread_sanitizer.sh`: managed-memory/channel native fixtures
  pass with normal/stress moving GC; six poll regressions and channel retirement
  pass. AOT/runtime/std are instrumented; JIT sanitizer coverage is not claimed.
  Log: `/tmp/gcr-contract-tsan.log`.
- Changed Rust regions formatted; final `cargo check --workspace --tests` passes
  with the six pre-existing compiler warnings. `git diff --check` is clean.

This checkpoint does not complete the full PLAN or remove transitional Sync
capture restrictions. Live shared-runtime type-table extension is not provided
by the exclusive-registration API. Reentrant locks, separate concurrent
collections, structured outcomes/scopes, cancellation, async and release/service
gates remain work. At this checkpoint, mutable enum payload bindings and runtime archive build/link
ownership remained separately recorded defects; the application checkpoint below
resolves the payload-binding defect.


## Application-driven compiler/library repairs — 2026-10-06

Three additional gc-rust applications are implemented with runnable input samples:
[gcr-csvreport](../apps/gcr-csvreport/README.md), a fixed-worker CSV aggregator;
[gcr-buildplan](../apps/gcr-buildplan/README.md), a dependency scheduler;
and [gcr-routes](../apps/gcr-routes/README.md), a directed cheapest-route planner.
Each validates a bounded CSV schema, checks numeric arithmetic, and emits complete
JSON to stdout or checked atomic file output. Ordinary collections remain
unsynchronized; the reporter shares only immutable inputs, an atomic work
counter, and a bounded completion channel. Separate concurrent collection APIs
remain future work.

Application development exposed and repaired:

- Switch match lowering discarded mutable payload-binding flags.
- Match branches and nested enum constructors lost contextual generic types.
- Guarded inline-enum matches generated reference-enum reads and crashed codegen.
- Reference-enum match bindings ignored inline values; payload sizing ignored
  scalar alignment and nested references were not traced. Construction, shared
  payload extraction, descriptor counting and sibling reflection offsets now
  agree; nested references use shared leading pointer slots.
- Decimal integer parsing silently wrapped out-of-range inputs. It now accepts
  exactly the signed-64-bit range using checked negative accumulation.

[CSV and heap APIs](csv-heap.md) are reusable standard-library implementations.
CSV encoding quotes leading BOM field text, rejects zero-field records, and
bounds escaped output. Minimum heaps handle both inline and reference elements.

`tests/app_primitives.rs` and `tests/real_apps.rs` cover focused compiler/library
regressions and independent grouping, scan-scheduler and Bellman-Ford models.
The larger workloads use 4,800 rows across eight files, 512 dependency tasks,
and 512 route nodes with 3,511 edges under a 1 MiB nursery. Native/JIT checks
include every-allocation GC, four collector workers, malformed input, numeric
endpoints/overflow, zero-duration tasks, zero-cost routes and output preservation.

Verification at this checkpoint:

- Full workspace: **513 passed**, zero failures, eight existing ignores before
  the final unit-payload/CSV-boundary additions. Subsequent focused checks cover
  those additions, including native/JIT every-allocation GC.
- Optimized compiler/library/application/generational/reflection/channel/managed
  selection: **36 passed**, zero failures.
- Native ThreadSanitizer: managed-memory/channel/nested-payload fixtures and all
  three apps pass in normal/stress modes; seven runtime poll/retirement checks
  also pass. Rust runtime/std and generated AOT code are instrumented.
- CSV tests accept the exact 1 MiB field and 16 MiB encoded-output limits and
  reject one byte over, with checked diagnostic positions. `GCR_STRESS_HEAP_MB`
  now configures the per-space stress heap for both JIT/native (default 8 MiB),
  allowing these real limits to be checked with collection on every allocation.

Final commands and follow-up results are recorded in the project status/app
pads. Logs: `/tmp/gcr-apps-workspace.log`,
`/tmp/gcr-apps-release.log`, `/tmp/gcr-apps-tsan.log`.

Reflection still omits inline reference-enum payloads: contiguous ValueMeta
cannot describe their split reference slots. Scalar/reference sibling metadata
is correct and checked by regression. Extending projection metadata and renderers
remains recorded work, alongside concurrent collections, structured lifetimes,
cancellation and service gates. Runtime archive ownership is repaired below.


## Native archive ownership and benchmark errors — 2026-10-06

Development AOT runtime production now uses a dedicated Cargo cache derived from
this compiler's baked target/profile path. Cargo checks all inputs with `--locked`;
source mtime guesses are removed. A process-shared file lock remains held through
Cargo production, native linking and debug-symbol extraction. Ordinary workspace
builds cannot replace this owned archive. Runtime build failure stops compilation
instead of linking a stale archive. Explicit `GCRUST_RUNTIME_LIB` overrides remain
caller-owned, and installed compilers retain packaged-archive lookup.

Regressions compile four native programs concurrently with an unrelated invalid
`CARGO_TARGET_DIR`, then run each under moving-GC stress. A controlled failed Cargo
producer proves that an existing native output is preserved and stale archives
are not used. Unit coverage checks custom target/profile/triple cache paths.

Benchmarks now return failure and preserve JSON diagnostics for build/launch
failures, signals and missing/invalid runtime metrics. Variant metrics use distinct
files and missing counters are not fabricated as zero. Bare-program nonzero
integer return codes remain legitimate benchmark results. Regressions cover build
errors, controlled aborts, missing metrics, and the existing metric/schema cases.

One interim workspace rerun had an intermittent benchmark native-build failure;
a retry passed. The old test discarded the build diagnostic, so the exact original
linker cause cannot be established. Full results are now retained on failure;
archive ownership defects are independently reproduced and tested. Final gate
results are recorded below and in the project pads.


Final validation for the combined checkpoint:

- `cargo test --workspace`: **520 passed**, zero failures, eight existing ignores.
  Log: `/tmp/gcr-apps-workspace-complete.log`.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release --test app_primitives
  --test real_apps --test generational --test reflect --test channel_contract
  --test managed_memory --test runtime_archive --test bench_cmd`: **46 passed**,
  zero failures. Log: `/tmp/gcr-apps-release-complete.log`.
- `scripts/check_thread_sanitizer.sh`: four native fixtures and three apps pass
  in normal/stress modes (14 instrumented executions); seven instrumented runtime
  poll/retirement checks pass. Log: `/tmp/gcr-apps-tsan-complete.log`.
- A compiler built in `target/tsan-compiler` successfully builds the nested-payload
  fixture without an archive override, using its own AOT cache, and the executable
  passes four-worker verified GC stress. Log: `/tmp/gcr-custom-target-build.log`.
- Final workspace build/check succeeds with five existing compiler warnings;
  changed Rust regions are formatted and `git diff --check` passes.

This is local macOS ARM64 evidence. Reflection projections, concurrent collection
APIs, structured lifetimes, cancellation, async and declared release/service
resource budgets remain open.

## Safety contract and numeric casts — 2026-10-13

Base checkout `40321f8`, branch `inline-closure-alloc`, plus uncommitted changes.
Host: macOS 26.5.2 ARM64; rustc 1.96.0. The comprehensive roadmap is now in
[ROADMAP.md](ROADMAP.md); item 1 starts with [SAFETY.md](SAFETY.md).

Float-to-integer conversions use LLVM saturating intrinsics instead of plain
poison-producing conversions. Lowering rejects pointer/aggregate casts and
invalid bool/char conversions. Integer-to-char conversion checks the original
integer before narrowing and aborts for invalid Unicode scalar values.

Regression validation temporarily restored the old codegen/lowering sources:
the float IR, invalid-cast and char-validation regressions each failed; repaired
sources were then restored. Runtime float boundary checks alone passed the old
compiler, illustrating why poison-sensitive IR verification is also required.

Completed validation:

- `cargo test --workspace`: **591 passed**, zero failures, eight existing ignores.
  Log: `/tmp/gcr-safety-workspace.log`.
- `GCR_GC_VERIFY=1 cargo test --release --test arithmetic --test rust_like_api
  --test stdlib_prelude --test operand_roots --test ffi`: **72 passed**, zero failures.
  Log: `/tmp/gcr-safety-release.log`.
- `cargo check --workspace --tests` passes with four existing compiler warnings.
  Log: `/tmp/gcr-safety-check.log`. `git diff --check` is clean.

Arithmetic checks cover f32/f64 casts to all eight integer widths, NaN,
infinities, signed/unsigned limits, fractions and signed zero; invalid casts
diagnose before codegen. Char checks cover surrogate endpoints, negative and
oversized inputs, truncation hazards and valid boundary values in optimized
JIT/native execution. No new sanitizer run is claimed for this checkpoint.

Item 1 remains open: privileged RawPtr/handle capabilities, the complete managed
access/caller audit, embedding ownership, constructor publication and the full
numeric/size boundary matrix. Thread/channel/atomic handles still share RawPtr;
cast rejection alone does not prevent wrong-kind handle use. Do not remove Sync
restrictions or infer blanket memory safety from this checkpoint.

## Reflection, unit values, snapshots and assignment places — 2026-10-15

Roadmap item 1 remains in progress. This checkpoint adds matching-width SC
reflection reads and checked aggregate rejection, consistent zero-payload unit
values/assignments, fallible reflection metadata encoding, GC-aware snapshot
coordination/output and single evaluation of compound assignment locations.
The detailed invariants and remaining audit scope are in [SAFETY.md](SAFETY.md).

Distinguishing regressions cover compound receivers/indexes with side effects,
RHS mutation of the original access path, operator traits and moving GC; unit
fields interspersed in heap/inline records and tuples, zero-size arrays and unit
function tails; reflection limits preserving native output; a collector already
waiting for the snapshot caller; multi-megabyte snapshot output held in an
undrained pipe; and failed stderr writes.

Validation:

- `cargo test --workspace`: **639 passed**, zero failures, eight existing ignores.

- Optimized regression selection: **72 passed**, zero failures. Command:
  `cargo test --release --test compound_places --test unit_values
  --test collection_boundaries --test managed_memory --test reflection_contract
  --test snapshot_progress --test type_safety --test unsafe_boundary
  --test definite_init --test string_safety --test operand_roots --test rust_like_api`.
- `bash scripts/check_thread_sanitizer.sh`: **30 native executions and 16 runtime
  tests passed**, no reports. Twelve fixtures and three apps run in normal and
  verified GC stress modes. Runtime checks include snapshot lock coordination,
  scoped TLS binding, collector, channel and poll/retirement paths.
- Logs: `/tmp/gcr-oct15-verified-workspace.log`,
  `/tmp/gcr-oct15-verified-release.log`, `/tmp/gcr-oct15-verified-tsan.log`.

Evidence is local macOS ARM64. The full managed-access/publication/barrier and
embedding ownership inventories remain open. Fatal diagnostic output paths
(including ai_panic) still need owned-copy/blocking/error handling review, recorded
in the project status pad. Transitional Sync restrictions remain in place.

## Fatal output and mutator registration — 2026-10-15

The fatal diagnostic defects recorded in the preceding checkpoint are repaired.
Terminal diagnostics own their text, park before stderr output and abort without
Rust unwinding when writes fail. Invalid reflection indexes use this path.
JIT/native optional heap-dump, GC-stat and allocation-profile output tolerates
stderr failure and preserves the program result, including dump-file failure
reporting. Tests distinguish failed stderr from a Rust panic using a panic-hook
sentinel and prove GC progress with a multi-megabyte panic in an undrained pipe.

A further embedding deadlock is repaired: registering another mutator on an OS
thread during collection now suspends that thread's existing RUNNING states
before waiting for gc_lock. Registration restores old/new states under the lock;
world pauses share the suspension helper. The deterministic subprocess now
forces both world-pause and registration waiters behind an active collector.

Completed focused validation:

- Diagnostic/heap/reflection/arithmetic/string optimized selection: **49 passed**.
- Full runtime crate: **175 unit tests and 8 compile-fail doctests passed**,
  eight existing ignores across runtime tests/documentation.
- Thread, completion, channel and concurrency integration selection: **26 passed**.
- ThreadSanitizer gate: **30 native executions and 17 runtime tests passed**,
  no reports. The final gate rebuilt all native fixtures with the registration
  change and exercised its deterministic interleaving and failed diagnostic
  write in instrumented runtime tests.
- Logs: `/tmp/gcr-diagnostic-workspace.log`, `/tmp/gcr-diagnostic-release.log`,
  `/tmp/gcr-diagnostic-tsan.log`, `/tmp/gcr-registration-runtime.log`,
  `/tmp/gcr-registration-integration.log`, `/tmp/gcr-registration-release.log`,
  `/tmp/gcr-registration-final-tsan.log`.

The final optimized concurrency selection passed all **5 tests**, for 54 selected
optimized tests in this batch. `cargo test --workspace` passed **643 tests**, zero
failures and eight existing ignores. `git diff --check` passes.

Item 1 remains open; this checkpoint does not close the managed access,
publication/barrier, embedding ownership and reference-specialization audits.

## Mutator retirement, collection and root tokens — 2026-10-15

Repaired same-OS-thread deadlocks in safe mutator retirement and mutator-triggered
major/minor collection. Every current RUNNING sibling is suspended before
waiting for the collector lock, then restored while that lock is held. Already
BLOCKED states retain their scopes. Actual collection counters distinguish a
completed collection from a world pause that merely held the lock; normal
requests can reuse a completed matching collection while stress requests retain
their per-request collection behavior.

The bounded coordination matrix covers 12 cases: major/minor, normal/stress,
uncontended, collector-contended and non-collecting-world-pause contention. A
root held only in a sibling's Rust frame survives movement. Retirement has a
separate forced active-census subprocess case. Runtime tests retain counter,
poll-storage retirement and existing blocked-state regressions.

The legacy Mutator root container also rejects stale handles after slot reuse,
cross-instance handles and stale/replaced scope prefixes. Moving the container
and collector updates preserve live handles; repeated restores to a surviving
prefix remain valid. Exhaustion cannot wrap generations into reused identities.
Its no-op safepoint is still separate from registered runtime execution.

Validation completed:

- Optimized GC/thread integration selection: **48 passed**. Command:
  `cargo test --release --test managed_memory --test concurrency_stress
  --test concurrency_contract --test threads_stress --test threads
  --test thread_completion --test snapshot_progress --test diagnostic_progress
  --test private_arrays --test compact_enums --test generational
  --test operand_roots --test channel_contract`.
- `cargo test --release -p gcrust-rt mutator`: **14 passed**, one existing ignore.
- Full runtime after token changes: **180 unit tests and 8 compile-fail doctests
  passed**, eight existing ignores across unit tests/documentation.
- GC ThreadSanitizer gate: **30 native executions and 18 runtime tests passed**,
  no reports. Includes the new collection/retirement interleavings. Root-token
  tests added afterward are validated in debug/release, not claimed as part of
  that sanitizer filter selection.
- Logs: `/tmp/gcr-mutator-coordination-workspace.log`,
  `/tmp/gcr-mutator-coordination-release.log`, `/tmp/gcr-mutator-coordination-tsan.log`,
  `/tmp/gcr-coordination-tokens-runtime.log`, `/tmp/gcr-root-tokens-release.log`.

`cargo test --workspace` passed **648 tests**, zero failures and eight existing
ignores. The optimized selections total **62 passed**; `git diff --check` passes.
Item 1 remains open: the complete managed-access, publication/barrier and
embedding API inventories are still required before production-readiness claims.

## Temporary roots across competing collection — 2026-10-16

Mutator-triggered collection previously exposed per-call extra roots only to its
own collection. When it parked behind an existing collector, that collector
could reclaim/move objects held only by those sources; a normal request could
then return early with stale root values. Scoped registration now publishes
those sources before parking. Major/minor collectors and world-pause enumeration
include active registrations, and a private lifetime-bound guard removes each
entry before its borrowed source binding expires. Empty lists do not allocate a
registration. Permanent source insertion requirements and per-call retirement
are explicit in the contract.

The bounded eight-case matrix covers uncontended, major/minor-contended and
world-pause paths in normal/stress execution. It checks relocation, payload,
active snapshot visibility, removal on early/normal return and GC after source
destruction. Temporarily restoring prior per-call-only visibility passed the
uncontended case and failed the competing-major case; the repaired sources were
restored and passed the complete matrix.

Completed verification:

- Optimized runtime: **180 unit tests and 8 compile-fail doctests passed**;
  nine existing ignores in that invocation. The detector test skipped in release
  was then run explicitly with `GCR_GC_VERIFY=1` and `--ignored`: **1 passed**.
  This covers 181 optimized runtime unit tests in total, with no failures.
- Optimized integration: **42 passed**. Selection: operand_roots, threads,
  concurrency_stress, concurrency_contract, managed_memory, snapshot_progress,
  diagnostic_progress, heap_cmd, heap_diff and generational.
- ThreadSanitizer: **30 native executions and 19 runtime tests passed**, no
  reports. The scoped-root subprocess matrix is included by the collector filter.
- Logs: `/tmp/gcr-scoped-roots-workspace.log`,
  `/tmp/gcr-scoped-roots-release-runtime.log`,
  `/tmp/gcr-scoped-roots-release-detector.log`,
  `/tmp/gcr-scoped-roots-release-integration.log`, `/tmp/gcr-scoped-roots-tsan.log`.
  Distinguishing regression: `/tmp/gcr-scoped-roots-negative.log`, repaired rerun:
  `/tmp/gcr-scoped-roots-restored.log`.

`cargo test --workspace` passed **649 tests**, zero failures and eight existing
ignores. The scoped-root matrix also passed with GCR_GC_WORKERS=4 in debug,
release and ThreadSanitizer builds. Additional logs:
`/tmp/gcr-scoped-roots-four-workers-debug.log`,
`/tmp/gcr-scoped-roots-four-workers-release.log`,
`/tmp/gcr-scoped-roots-four-workers-tsan.log`. `git diff --check` passes.

Roadmap item 1 is still open; the complete managed-access/publication/barrier and
embedding ownership audits remain required. Low-level collect still requires exclusive quiescent access;
scoped registration is not a general execution ownership API.

## Active GC goal: root inventory and measured allocation performance — 2026-10-16

The concurrent collector's two pauses now scan the same complete registered-root
families as STW collection and snapshots, including JIT/permanent/scoped sources.
Both pauses raise poll flags so allocation-free workers participate. New bounded
root/poll cases pass, and restoring the old missing-root behavior fails the root
regression. The scoped-root matrix now includes concurrent contention.

Heap-bound validated allocation requests eliminate nested descriptor/length
revalidation without removing entry checks. Typed requests establish header
compatibility and reselect the arena after GC. The work queue notifies on work
publication, quiescence or failure; a bounded chain/tree model checks traversal
and termination with 1/2/4/8 workers. Contracts are in [SAFETY.md](SAFETY.md).

Three isolated source/build-identified benchmark sweeps retain raw samples,
commands/logs and source archives. Validated requests reduce TLAB allocation
elapsed time by **2.44–2.64×** locally. Predicate-based notifications improve the
four-worker small-object median about 7% and payload median about 22%; some small
differences regress/noise, and eight-worker small-object copying remains about
2.5× slower than four workers. Complete tables/artifacts: [GC_PERFORMANCE.md](GC_PERFORMANCE.md).
No default cap was introduced to hide that scaling issue.

Final validation:

- `cargo test --workspace`: **653 passed**, zero failures, eight existing ignores.
- `cargo test --release -p gcrust-rt`: **184 unit tests and 8 compile-fail doctests
  passed**, nine existing ignores in that invocation. The skipped detector was
  explicitly run with `GCR_GC_VERIFY=1` and `--ignored`: **1 passed**, covering
  185 optimized runtime unit tests in total.
- Optimized integration: **42 passed** (operand_roots, threads, concurrency_stress,
  concurrency_contract, managed_memory, snapshot_progress, diagnostic_progress,
  heap_cmd, heap_diff, generational).
- ThreadSanitizer: **30 native executions and 23 runtime tests passed**, no reports.
  Includes concurrent root/poll tests, queue traversal, scoped roots and validated
  allocation requests. `git diff --check` passes.
- Logs: `/tmp/gcr-goal-oct16-workspace.log`,
  `/tmp/gcr-goal-oct16-runtime-release.log`, `/tmp/gcr-goal-oct16-detector.log`,
  `/tmp/gcr-goal-oct16-integration-release.log`, `/tmp/gcr-goal-oct16-tsan.log`.

The goal remains active. Concurrent mutation/copy/barrier correctness, the full
access/publication/embedding audits, representative latency/RSS budgets and
release/soak evidence remain open in [GC_READINESS.md](GC_READINESS.md).

## 2026-10-16: forwarding read barriers

Repaired both read-barrier decoders to use the collector's bit-63 forwarding
encoding. The old bit-zero test treated ordinary odd type IDs as forwarding
pointers. Atomic readers now wait for a marker-only copy claim to publish a
destination or restore its original header. Regression coverage includes actual
Compact/Full collections, odd type IDs, deterministic claim publication and
rollback, and a bounded publication test that reads the copied payload before
joining its writer. Restoring the old decoder makes the odd-ID regression fail.

Final verification: 657 workspace tests passed (eight existing ignores); 188
optimized runtime unit tests plus the explicitly enabled detector passed, with
eight compile-fail doctests and 42 optimized integration tests. ThreadSanitizer
passed 30 native executions and 27 runtime tests without reports. Logs are
`/tmp/gcr-readbarrier-final-{workspace,runtime-release,detector,integration-release,tsan}.log`.

A separate valid-custom-header bug remains open: the heap confuses a u16 type-ID
offset with an aligned u64 forwarding-word offset. A safe standalone reproducer
confirmed acceptance and allocation with a type ID at offset two; collection was
not invoked through the misaligned atomic pointer. This needs a distinct header
layout/protocol repair, recorded in the project status pad and GC readiness ledger.
The full production GC goal remains open.

## 2026-10-16: custom-header forwarding layout

Separated Heap's aligned eight-byte forwarding word from its u16 type-ID
location. `ObjHeader::HEAP_FORWARDING_OFFSET` defaults to zero; Full retains
eight and Compact retains zero. Layout validation checks alignment/bounds.
Parallel and concurrent copying decode an overlapping ID from captured native
bytes, or read the separate immutable ID. Parallel copying excludes the claimed
word and restores its complete original metadata in the destination.

Four regressions cover native-order ID decoding and three custom layouts: ID
offsets two/ten and forwarding offsets zero/eight. Bounded subprocesses exercise
1/2/4/8 workers, shared cycles, full metadata preservation, exact live size, major
relocation, minor promotion followed by major relocation, and concurrent
collection without mutator writes. Restoring low-16-bit ID decoding fails the
isolated regression: `/tmp/gcr-custom-header-decoding-negative.log`.

Final verification:

- `cargo test --workspace`: **661 passed**, no failures, eight existing ignores.
- Optimized runtime: **192 unit tests and 8 compile-fail doctests passed**; nine
  existing ignores. Explicit `GCR_GC_VERIFY=1` detector run: **1 passed**, for
  **193 optimized runtime unit tests** in total.
- Selected optimized integrations: **42 passed**.
- ThreadSanitizer: **30 native executions and 31 runtime tests passed**, no reports.
- `git diff --check` passes. Logs:
  `/tmp/gcr-header-final-{workspace,runtime-release,detector,integration-release,tsan}.log`.

A separate collector-failure hang is confirmed: after an injected precise-layout
assertion, MutatorGcLock's destructor spins restoring a mutator while the failed
cycle still has `gc_requested` raised. The captured stack is in
`/tmp/gcr-custom-header-negative-sample.txt`; the process was terminated and
source restored. The project pad and [SAFETY.md](SAFETY.md) record the required
coordinated failure policy. Clearing flags and resuming a partly relocated heap
is not a valid repair. The full production GC goal remains open.

## 2026-10-16: collection panic escape and helper payloads

Collection work now contains escaping panics before mutator-restoration guards
can unwind. Direct Heap, dedicated STW/concurrent, mutator major/minor/scoped
extra and legacy SemiSpace entry points abort on collection panic: relocation
has no rollback protocol. Evacuation jobs also contain panics locally, before
the generic reusable helper pool can retain/drop arbitrary payloads. Normal
caller unwinds and pre-collection validation retain their existing behavior.

Five regressions exercise bounded children across entry points and both
concurrent root passes (including verified actual relocation), 1/4 workers,
helper failure, three simultaneous helper payloads with forbidden destructors,
promotion exhaustion, and payload handling. A positive case collects during an
unrelated unwind and preserves the caller's panic. The detector now verifies
its diagnostic and SIGABRT in a child. Expected-fatal children also reject
ThreadSanitizer reports in captured stderr.

Negative evidence: removing the outer collection boundary reproduces the
restoration hang (`/tmp/gcr-failure-boundary-negative.log`); a coordinator-only
boundary lets the helper pool drop a payload and exit 88 instead of aborting
(`/tmp/gcr-worker-payload-negative.log`). Both defects are repaired.

Final verification:

- Workspace: **666 passed**, no failures, eight existing ignores.
- Optimized runtime: **197 unit tests and 8 compile-fail doctests passed**; nine
  existing ignores. Explicit verified detector: **1 passed**, for **198 optimized
  runtime unit tests** total.
- Optimized integration selection: **42 passed**.
- ThreadSanitizer: **30 native executions and 36 runtime tests passed**, no reports.
- Focused failure tests pass after strengthening child-stderr assertions.
- `git diff --check` passes. Logs: `/tmp/gcr-failure-v2-workspace.log`,
  `/tmp/gcr-failure-v2-current-runtime-release.log`,
  `/tmp/gcr-failure-v2-current-detector.log`,
  `/tmp/gcr-failure-v2-integration-release.log`, `/tmp/gcr-failure-v2-tsan.log`,
  `/tmp/gcr-failure-child-checks.log`.

A subsequent isolated 36-run benchmark sweep completed with exact object-count
checks and retained source/raw evidence. It reveals a current checkpoint
regression relative to the earlier queue snapshot, including about 15%/17%
slower four-worker small/payload copy medians. Allocation remains 2.28–2.46×
faster than the original baseline. Attribution and correction remain open in
[GC_PERFORMANCE.md](GC_PERFORMANCE.md); no performance-preservation claim is made.

Full GC production readiness remains open: concurrent mutation/barrier
integration, the broader access/ownership/failure audit, performance regression
and scaling, platform/CI matrix and long-run evidence still require work.

## 2026-10-16: queue contention and bounded streaming

An eight-worker profile identified dominant mutex contention in shared work
queue paths. Adaptive batches share scarce work and amortize wide frontiers over
up to 128 tasks. Large scans publish while retaining their active-producer count;
small fixed scans use a separate specialization. Claim/discovery/layout/address
buffers stay within 512 entries per worker, including wide variable arrays.
Shared queue/root memory remains graph-dependent. No worker-count cap was added.

New regressions verify mid-scan producer accounting, actual relocation of
65,536-element arrays with 8,192 shared back-linked children, aliases/payloads and
exact live size, major and minor-then-major cycles at 1/2/4/8 workers. Queue models
now include stars as well as chains/trees. Native header extraction is checked
across all aligned ID slots before/after the captured source word is replaced.
The decoder arithmetic experiment was reverted after inconclusive timing.

Final verification:

- Workspace: **669 passed**, no failures, eight existing ignores.
- Optimized runtime: **200 unit tests and 8 compile-fail doctests passed**; nine
  existing ignores. Explicit verified detector: **1 passed**, for **201 optimized
  runtime unit tests** total.
- Optimized integration selection: **42 passed**.
- ThreadSanitizer: **30 native executions and 39 runtime tests passed**, no reports.
- `git diff --check` passes. Logs:
  `/tmp/gcr-batching-final-{workspace,runtime-release,detector,integration-release,tsan}.log`.

Three interleaved fixed/adaptive benchmark pairs completed 72 workload processes
after all checks finished. Only the queue batch-size rule differs between their
sources; final source hashes match the restored adaptive snapshot. Under the
recorded background load, ratios of median small-copy times are 1.29×/1.17×/2.22× for
2/4/8 workers, with improvement in every paired small-copy trial at those counts.
One-worker payload copying is about 4% slower. These are qualified workstation
measurements, not quiet-machine performance guarantees. Earlier long confirmation
was CPU-contended and is retained without claiming a clean comparison. Schema-2
reports now retain host load/process context before/after workloads. Full tables,
raw evidence and limits are in [GC_PERFORMANCE.md](GC_PERFORMANCE.md).

The full goal remains open: concurrent mutation/barrier integration, remaining
ownership/access/failure audits, quiet performance confirmation, eight-worker
scaling, latency/RSS budgets, platform/CI matrix and long-run evidence require work.

## Scoped concurrent access checkpoint, 2026-10-16

Concurrent copying and host accesses now share ordered object stripes. Scoped
access resolves old aliases under exclusion, and competing copiers reserve one
destination. Identity comparison handles published forwarding aliases without
waiting on an unpublished claim. Card marking selects the actual physical arena;
below-base addresses cannot underflow card arithmetic.

The deterministic regression exercises actual concurrent collection, writes of
1/2/4/8 bytes, three header layouts, both pauses with root reloads, and a following
minor collection preserving a nursery edge written through the destination.
Contention and opposite lock-order tests are bounded subprocesses. Deliberately
returning the old address or using raw reference equality makes the regression
fail; those negative implementations were restored before verification.

Final gates: `/tmp/gcr-scoped-serial-gates.json` records sanitizer and serial
workspace exit zero; logs are `/tmp/gcr-scoped-serial-tsan.log` and
`/tmp/gcr-scoped-serial-workspace.log`. Optimized logs are
`/tmp/gcr-scoped-final-runtime-release.log`,
`/tmp/gcr-scoped-final-detector.log` and
`/tmp/gcr-scoped-final-integration-release.log`.
Initial parallel attempts exceeded existing CSV/native deadlines during observed
host load around 120 on 12 CPUs. The targeted CSV and complete serial retries
passed without changing source, deadlines or program worker counts.

Generated managed accesses, reference CAS, reflection/FFI/container paths and
aggregate synchronization still need integration with this protocol. Ordinary
raw-pointer equality remains raw-pointer equality. These results establish the
host primitive, not completed language-wide concurrent collection support.

### Host reference CAS follow-up, 2026-10-16

Added identity-aware scoped SeqCst reference CAS to Heap and MutatorThread,
including pre-replacement SATB and successful nursery-edge card marking. The
actual concurrent-cycle test now uses it. A deterministic alias fixture verifies
success across source/destination bits, failed-slot preservation and exact SATB
entries. A temporary raw-bit comparison fails that fixture
(`/tmp/gcr-reference-cas-negative.log`); the correct source was restored.
Current-source focused debug collector verification: 28 passed. Optimized runtime:
204 passed plus 9 compile-fail doctests; the explicit ignored detector was not
rerun in this follow-up. Log: `/tmp/gcr-reference-cas-release.log`.
The full workspace/sanitizer checkpoint above precedes this additive follow-up.
Generated AtomCas integration remains open.
Current-source collector ThreadSanitizer also passed all 28 tests, with no reports
(`/tmp/gcr-reference-cas-tsan.log`). `git diff --check` passed.

## Generated reference Atom checkpoint, 2026-10-16

Managed-reference AtomLoad and AtomCas now call non-safepointing runtime entry
points that hold canonical object access for the entire operation. CAS compares
managed identity and applies SATB/card barriers; scalar/RawPtr atomics retain
their bit semantics. Native declarations, JIT symbol mappings and relocation
call effects are connected. No concurrent capability was disabled.

Evidence includes an IR routing regression, native/JIT normal/stress identity
and allocating-replacement checks, two contending reference writers in the
managed-memory fixture, and a bounded runtime ABI test across eight actual
concurrent collections. The fixture exposed a quoted LLVM block-label parsing
bug in its structural checker; the checker now handles quoted identifiers and
has a regression retaining its rejection of collecting snapshot paths.

All four serial gates in `/tmp/gcr-atom-integrated-gates.json` exited zero:
full sanitizer, full workspace, optimized runtime, and selected optimized
integration. Logs are `/tmp/gcr-atom-integrated-{tsan,workspace,release,integration-release}.log`.
The explicit optimized corruption detector also passed:
`/tmp/gcr-atom-integrated-detector.log`. Sanitizer native deadlines were unchanged.
The workspace total is 676 passed, zero failures, eight existing ignores;
optimized runtime is 205 regular plus one explicit detector, nine compile-fail
doctests, and 20 selected integration tests. Sanitizer reports 30 native and
44 runtime executions (43 distinct runtime tests), no reports.

General fields/arrays, scalar Atoms, closure/header reads, construction,
reflection and aggregate synchronization still need the scoped protocol.
Current performance is unmeasured; host load was high during verification.

## Scalar Atom checkpoint, 2026-10-16

Scalar Atom loads/CAS now use width-specialized scoped runtime operations for
8/16/32/64 bits. The operations retain matching-width SeqCst atomics, bool byte
storage, floating-point bit comparison and RawPtr address comparison. JIT/native
symbols and nonrelocating-call effects are connected. The untraced scalar path
does not run managed-reference SATB/card barriers. Runtime width dispatch is not
needed. Idle scopes acquire no object stripe; current call overhead remains to
be measured rather than assumed negligible.

The bounded concurrent ABI test now covers each width through eight actual
collection cycles and verifies neighboring bytes after narrow operations and
copying. Native/JIT normal/stress tests cover bool publication, narrow values,
char, NaN/signed zero, reference identity and RawPtr address CAS. The RawPtr Atom
is explicitly constructed inside unsafe according to the existing RawPtr-record
capability rules. An IR regression requires every width's scoped load/CAS path;
the native sanitizer fixture exercises all widths too.

All five gates in `/tmp/gcr-scalar-integrated-gates.json` exited zero: full
sanitizer, full serial workspace, optimized runtime, selected optimized
integration, and the explicit optimized corruption detector. Logs are
`/tmp/gcr-scalar-integrated-{tsan,workspace,release,integration-release,detector}.log`.
Results: 677 workspace tests passed with zero failures/eight existing ignores;
206 optimized runtime unit tests including detector, nine compile-fail doctests,
and 21 selected optimized integration tests; sanitizer 30 native and 44 runtime
executions (43 distinct runtime tests), no reports. Existing native deadlines and
workloads were retained. `git diff --check` passed.

Direct ordinary fields/arrays (including Atom storage fields), closure/header
reads, constructors, reflection and aggregate locking remain open. Layout
semantics must distinguish traced slots from native RawPtr even when both use
pointer storage in the current emitter. Performance budgets remain open.

## Mutable field/array slot checkpoint, 2026-10-16

Generated mutable scalar/reference fields and array slots now carry base+offset
through an atomic phase check. Idle paths retain native SC or proven-private
plain accesses with alias metadata. Active paths use complete matching-width
scoped operations. Reference stores carry explicit tracedness from field/element
layout, use pre-CAS SATB logging/retry and mark the canonical physical card;
RawPtr scalar storage is untraced. Thread appends a stable heap phase pointer at
offset 80, with root/child initialization and compile-time layout assertions.
Both concurrent pauses advance the epoch used to reload generated roots.

Evidence includes all-width/nonzero-offset runtime ABI operations across eight
actual concurrent cycles, sentinel preservation, exact live size, late-card
nursery-edge preservation for CAS and ordinary reference store, phase/fallback/
private-provenance IR checks, private graph tests and native/JIT moving-GC tests.
A suspected requirement to mark only object-start cards was disproved: the
collector's overlap-aware scanner accepts later dirty cards. No card-marking
change was made for that hypothesis. An old private-access unit assertion was
updated to permit only the phase-byte atomic while continuing to reject atomic
private data accesses and require provenance. All eight private/escape unit
tests pass. These are recorded audit corrections, not unresolved runtime bugs.

Final source-stable gate evidence: `/tmp/gcr-slots-verified-gates.json` records
exit zero and matching before/after source hashes for full sanitizer, full serial
workspace, optimized runtime, selected optimized integration and explicit
optimized detector. SHA-256:
`2720fe78176aac332907c43b28c6af9044cc82406e0079802bdd5c513ef39587`.
Logs: `/tmp/gcr-slots-verified-{tsan,workspace,release,integration-release,detector}.log`.
Results: 680 workspace passed/zero failed/eight existing ignores; 208 optimized
runtime unit tests including detector, nine compile-fail doctests, 24 selected
optimized integration tests; sanitizer 30 native and 46 runtime executions
(45 distinct runtime tests), no reports. Existing deadlines and workloads were
retained. `git diff --check` passed.

Aggregate locking, immutable/publication/constructor proofs, runtime/reflection/
FFI and bulk access inventory, full generated concurrent mutation workloads,
embedding/platform/soak evidence and performance budgets remain open. Native
idle fast paths are retained; current overhead has not yet been measured.

## Reflection access checkpoint, 2026-10-16

Reflection count/kind/name now resolve supplied aliases before inspecting object
metadata; field-i64 retains one scope across descriptor selection and its
matching-width SC field load. Bounds and aggregate-kind failures drop access
before diagnostics may park; field-name allocates after inspection has released
the guard. Names/descriptors remain immutable off-heap and published enum tags
remain immutable under the language object contract.

The bounded ABI regression now uses a scheduled identity policy: pointer decode
in the concurrent scan rendezvous with the RUNNING worker after the collector
has released its owner stripe. Every one of eight cycles must include a worker
iteration while copying stays active. The fixture uses the standard Full enum
tag offset, distinct width-correct scalar metadata and an alias-based payload
change checked through reflection. It also retains reference/Atom/slot checks,
nonzero offsets, sentinels and exact live size. This strengthens earlier tiny-heap
coverage, which did not force active-phase mutator work.

Negative variants using the source alias for metadata/payload fail with captured
assertions: zero fields instead of one, or payload 17 instead of 23. Logs are
`/tmp/gcr-reflection-{metadata,payload}-negative.log`. The bounded subprocess
contains the resulting collector wait after an intentionally failing worker;
correct source was restored before all final checks.

`/tmp/gcr-reflection-verified-gates.json` records exit zero and unchanged hashes
for debug runtime, optimized runtime, selected optimized integration, explicit
optimized corruption detector and collector ThreadSanitizer. Changed-file hash
(runtime.rs and runtime/atom_tests.rs):
`ecba4e0b6e891ea1b864155bc96d216180096d0413ddd26e22bcc8bc69a0c3ce`.
Logs: `/tmp/gcr-reflection-verified-{debug-runtime,release-runtime,release-integration,detector,tsan-collector}.log`.
Results: 208 debug unit tests; 208 optimized unit tests including detector; nine
compile-fail doctests; 23 selected optimized integrations; 31 collector sanitizer
tests, no reports. `git diff --check` passed. This is targeted verification, not a
new full-workspace/full-native-sanitizer checkpoint.

Constructor/publication proofs, remaining runtime/container/FFI and rendering
accesses, generated end-to-end concurrent workloads, embedding/platform/soak
validation and measured performance budgets remain open.

## Scalar native buffer checkpoint, 2026-10-16

The scalar copy-in/out ABI now resolves its managed base and retains canonical
access for the whole copy. Managed interior addresses do not escape. It remains
non-safepointing/RUNNING, with private disjoint native storage, naturally aligned
managed elements, matching-width per-element SC accesses and bool normalization.
Reference arrays are excluded. Guards end before native entry/callback/blocking.
No whole-array transaction guarantee is added.

The scheduled eight-cycle ABI regression now retains a buffer source alias in an
untouched traced probe field and a separate global root. Every active iteration
varies input by iteration and width, checks 1/8/16/32/64-bit copy-out/in, zero-byte
null native buffers, canonical data independently, unchanged tail sentinels,
length and exact live bytes. Repeated input initially hid a stale-source read;
the varying pattern corrected the coverage gap. Wrong-source variants for either
direction fail with captured byte-array mismatches:
`/tmp/gcr-buffer-{in,out}-negative.log`. Correct code was restored before gates.

`/tmp/gcr-buffer-verified-gates.json` records exit zero and unchanged changed-file
hashes for debug runtime, optimized runtime, selected optimized integrations,
explicit optimized detector and full native/runtime ThreadSanitizer. Changed-file
hash (runtime.rs, runtime/managed.rs, runtime/atom_tests.rs):
`f4e1cf3cd194faa9faf01f2bfbda10898acdbd4c0225ddfa139dcd420be89287`.
Logs: `/tmp/gcr-buffer-verified-{debug-runtime,release-runtime,release-integration,detector,tsan}.log`.
Results: 208 debug runtime unit tests; 208 optimized runtime unit tests including
detector; nine compile-fail doctests; 40 selected optimized integration tests;
sanitizer 30 native and 46 runtime executions (45 distinct runtime tests), no
reports. Native sanitizer includes the root-moving callback/buffer fixture.
Existing deadlines/workloads were retained. `git diff --check` passed.
The full workspace was not rerun for this targeted runtime follow-up.

The mutable scalar-buffer bridge is repaired; immutable String/publication proofs,
reference/container bulk operations, remaining runtime/FFI access, full generated
concurrent workloads, embedding/platform/soaks and performance budgets remain open.

## Host response publication checkpoint, 2026-10-16

Found a real race in scratch-rooted partial array construction: plain pointer
stores competed with the concurrent collector's atomic slot scan. Entry stores
now use the complete canonical SC reference-store helper with SATB/card handling.
The builder polls pending requests before each entry allocation, where all live
managed objects are scratch-rooted, and reloads the array afterward. New Strings
finish bytes before pointer publication; the final response initializes from
reloaded scratch roots without another safepoint. String/array count rewrites
were removed because typed allocation initializes the immutable counts.

The bounded regression requires a collection to observe a partial scratch-rooted
array and verifies every one of 32,768 UTF-8 entries. The first version used
reservation timing: it passed the 681-test workspace but failed its partial-array
assertion in release when construction outran the collector. A test-only hook
now rendezvous after scratch rooting, before the first entry. It waits for the
request without parking/polling; the production loop must service it. There is
no production delay or relaxed timeout. The poll-negative variant fails partial
observation; the plain-store variant reports a ThreadSanitizer race between
ai_host_call and process_slot_concurrent. Logs:
`/tmp/gcr-io-{poll,store}-negative.log`. Correct code was restored before gates.

Final evidence: `/tmp/gcr-io-final-gates.json` records exit zero and unchanged
changed-file hashes for full sanitizer, debug runtime, optimized runtime,
selected optimized integrations and explicit optimized detector. Changed-file
hash (runtime.rs, runtime/io.rs, runtime/io_publication_tests.rs):
`a7d1be47eb7928832a702c002eb607447ff6d53439bb9af2975c19dbf5f2e401`.
Logs: `/tmp/gcr-io-final-{tsan,debug-runtime,release-runtime,release-integration,detector}.log`.
Results: 209 debug runtime unit tests; 209 optimized runtime unit tests including
detector; nine compile-fail doctests; 37 selected optimized integrations; sanitizer
30 native and 47 runtime executions (46 distinct runtime tests), no reports.
`git diff --check` passed. The earlier 681-test full workspace pass is at
`/tmp/gcr-io-verified-workspace.log`; the full workspace was not repeated after
the test-only scheduling correction.

General constructors, direct header/closure access, runtime/container/bulk paths,
generated full concurrent workloads, embedding/platform/soaks and performance
budgets remain open. Header audit targets are recorded in the project status pad.

## Canonical header reader checkpoint, 2026-10-16

Confirmed old-alias failures in physical type-ID and closure-code inspection.
The new non-safepointing ai_object_type_id resolves and scopes the physical read.
Generated TypeIdOf preserves its direct idle load and nominal compact-enum ID
mapping, with scoped resolution while copying. Runtime/JIT declarations and
nonrelocating effects are connected. Closure-code lookup retains canonical access
through header, capture-count metadata and code-pointer read. Type-name lookup
and channel witness creation/validation/message checks use scoped type inspection
without keeping a guard over allocation, diagnostics or queue blocking.

The eight-cycle scheduled ABI regression now retains old closure/witness aliases
in immutable probe fields. It checks a two-capture closure's code address and
physical ID, existing channel validation, channel creation from an old witness,
and allocated type-name contents during copying. Exact live-byte accounting and
the previous reflection/reference/scalar/buffer checks remain. The generated IR
regression checks phase resolution and nominal enum mapping; compact-enum/native
checks pass. Negative variants return 152 instead of physical ID 7 or index the
closure layout table with 152; logs `/tmp/gcr-header-{type-id,closure}-negative.log`.
Correct code was restored before final verification.

`/tmp/gcr-header-verified-gates.json` records exit zero and unchanged changed-file
hashes for debug compiler/runtime libraries, optimized runtime, selected optimized
integrations, explicit optimized detector and full sanitizer. Changed files are
runtime.rs, runtime/atom_tests.rs, src/codegen.rs and tests/reflect.rs; hash:
`f9487f98d53a3350514700762f45beda035d8f356b291ceb99c63a53d23cecd3`.
Logs `/tmp/gcr-header-verified-{debug-libraries,release-runtime,release-integration,detector,tsan}.log`.
Results: 189 compiler and 209 runtime library tests; 209 optimized runtime unit
tests including detector; nine compile-fail doctests; 46 selected optimized
integrations; sanitizer 30 native/47 runtime executions (46 distinct runtime
tests), no reports. `git diff --check` passed. This is not a new full-workspace
checkpoint. All broader readiness/performance obligations remain open.

## Current baseline and bounded continuation checkpoint, 2026-10-16

Audited normal generated Full-layout enum tags/payloads and immutable value-box
reads: their bytes lie outside the forwarding word and remain immutable between
polls, conditional on completed publication. Mutable/header/identity operations
on returned old aliases still need their scoped protocols. Custom-header/unsafe
host mutation, constructors and code/metadata lifetimes remain separate audits.

Current-source baseline: 36 successful count/live-byte-checked processes at
1/2/4/8 workers, with raw samples, CPU/load observations and source archive in
`target/gc-bench/2026-10-16-current-access-baseline.json`. Eight-worker small copy
remained slower than four (4.002 vs 1.949 ms); eight workers helped payload copy
(0.916 vs 1.309 ms). Background load 3.74–4.49 was recorded. Current profiling
still showed shared queue mutex costs (`/tmp/gcr-current-access-profile.txt`).

A combined finish/take critical section was tested in three source-archived pairs,
produced no repeatable benefit and slowed two-worker payloads in every pair; it
was reverted. An initial half-frontier local split fragmented batches and was
replaced. The retained implementation keeps small published-object continuations
intact up to 128 tasks/32 KiB of copied work, publishes excess, and puts large
copies and BUSY retries into the shared queue. Active counts cover local work;
copy claims finish publication first. Copy storage still uses exact reservations.

The retained candidate passed 209 debug runtime unit tests, 209 optimized runtime
unit tests including detector, nine compile-fail doctests and eight selected
optimized integrations. ThreadSanitizer passed 11 full parallel-module and 32
collector executions (43 executions, 40 distinct names), no reports. The
chain/tree/star model uses the production continuation at 1/2/4/8 workers and
checks exact visitation and final quiescence. Gate evidence:
`/tmp/gcr-budget-continuation-gates.json`, with unchanged parallel.rs hash
`8e0d3a538363c0d808e21a59f335136f27503fd63aeb17705a150d264da2a25e`.
Logs `/tmp/gcr-budget-continuation-*.log`. Full workspace/native sanitizer were
not repeated for this queue-only change. `git diff --check` passed.

Three interleaved pairs completed 72 benchmark processes. Only parallel.rs
changed between each control/candidate archive; all six archive hashes and
workload count/live-byte checks passed. Artifacts
`target/gc-bench/2026-10-16-budget-continuation-pair-{1,2,3}-{control,continuation}.json`;
analysis `target/gc-bench/2026-10-16-budget-continuation-analysis.json`.
Small-copy paired median ratios were 1.298 at four workers and 1.132 at eight,
with improvement in all three pairs at each count. Background load rose to
42.10–74.91 on 12 CPUs; unchanged allocation controls also moved substantially.
One-/two-worker small-copy and payload results were mixed, including slower
payload medians at four/eight. This does not establish a quiet or broadly
regression-free improvement. Source was restored to the validated candidate
and its hash rechecked after experimentation. Performance/readiness stays open.
