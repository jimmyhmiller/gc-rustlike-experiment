# Recursive branch and root-management continuation

The first working conditional outlining pilot separated an allocating recursive
branch into a noinline managed helper, leaving the original small base case
available for ordinary LLVM inlining. `recursive-branch-pilot.json` measured
12.9335 ms for trees versus Java 9.8776 ms (one fork, 20 warmups, 10 samples).

The following pilot added a cold branch hint for post-call root refresh:
`root-branch-weights-pilot.json` measured 12.2225 ms versus Java 9.3362 ms.
The hint changes optimization choices, not the relocation check or root protocol.
These are exploratory measurements, not JVM parity claims.

Both runs validate the original full output and retain all 96,665,730 tree
allocations and 2,314,735,680 allocated bytes across each 30-iteration process.
Benchmark sources are unchanged; raw files contain source and implementation
fingerprints. Later combined validation and repeat measurements will determine
whether the changes are retained.

The first combined three-fork run was stopped because unrelated Coil workloads
used several CPU cores. Its completed cases are retained as
`recursive-branch-contaminated.json`, explicitly marked incomplete and excluded
from parity claims. Within-fork scheduling outliers reached hundreds of
milliseconds. A quiet-machine repeat is required.

The combined outlining, owner-local epoch and root-selection checkpoint passed
552 workspace tests (8 ignored), 22 instrumented native executions and seven
runtime sanitizer tests. Further call-effect precision for inline array/field
operations is now being validated; immutable value-box reads are noncollecting,
while boxing writes and mutable aggregate snapshot locks remain effectful.

The counted-poll and root-slot-selection checkpoint passed 557 workspace tests
(8 ignored), 24 instrumented native executions and seven runtime sanitizer tests,
but `counted-tail-results.json` exposed performance regressions. Ordinary polls
restored fannkuch performance (`ordinary-polls-pilot.json`); root-slot selection
still made the array kernel about 30 ms. Against the same runtime and unchanged
source, retaining all root slots restored approximately 4.86 ms. Broad noinline
and removing root-refresh branch weights did not fix the regression.

Both operation-budgeted polling and root-slot selection were discarded. The
remaining changes retain ordinary loop checkpoints, owner-local relocation
epochs, recursive branch outlining, inline call-effect precision and GC-aware
direct self tail lowering. Revalidation and a full comparison are required for
this retained checkpoint. The exact CPU mechanism of the root-slot slowdown is
not established; extra dummy stores are not introduced as a workaround.

The retained checkpoint passed 554 workspace tests (8 ignored), 24 instrumented
native executions, seven runtime sanitizer tests and a release build. The full
three-fork/20-warmup/10-sample comparison is `retained-results.json`; all source
hashes and outputs match the original cases. Trees are 12.9585 ms versus Java
9.2674785 ms; array is back to 4.973 ms versus Java 6.527646 ms. Fannkuch remains
slower at 19.435 ms versus Java 16.3861245 ms, and object-based nbody remains
124.898 ms versus Java 44.5817295 ms. This checkpoint does **not** reach parity.

The recursive fixture was subsequently renamed from poll_budget to
recursive_poll, and the startup compatibility comment was clarified. Those are
text/name changes; the raw run retains the measured implementation hashes.
