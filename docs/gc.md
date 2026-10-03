# Garbage collector and runtime

The runtime lives in `crates/gcrust-rt`. `gc/heap.rs`, `gc/semi_space.rs`,
`gc/scan.rs`, `gc/roots.rs`, `gc/thread.rs`, and `runtime.rs` implement the
collector, root protocol, mutator coordination, and generated-code entry points.

Normal execution uses a nursery plus copying tenured space. Minor collections
promote survivors. Major collection copies tenured objects. A card-table write
barrier records old-to-young stores, including references inside flattened value
fields. Objects carry a type ID; TypeInfo describes the traced locations.

Generated functions pass a runtime Thread pointer. Reference locals use shadow-
stack root slots; value locals containing references use indirect roots. ANF
normalization evaluates eager operands into locals, including scalar expressions
that can allocate, and snapshots local operands before later side effects.
The enclosing operation reloads the resulting rooted heap addresses. The collector rewrites
roots when it moves objects. Registered mutators cooperate through safepoints;
blocking/native transitions publish frames for collection.

`configured_heap_sizes()` in `runtime.rs` supplies both JIT and AOT settings:

| Variable | Default | Interpretation |
| --- | --- | --- |
| GCR_NURSERY_MB | 16 | Nursery setting in MiB |
| GCR_TENURED_MB | 256 | Tenured setting in MiB |

Positive integer values override defaults; invalid/zero/overflowing inputs fall back to the
default. These are startup settings, not adaptive heap sizing or a total RSS cap.
Copying space requires reserve memory. Allocation failure currently aborts.

`gcr run --gc-stress` uses collect-on-allocation semi-space execution in both
bare-file JIT and project-native paths. Standalone native executables select the
same mode with `GCR_GC_STRESS=1`. The precise-layout
verifier is available with `GCR_GC_VERIFY=1`; it checks traced pointers against
object headers. A detector and passing unit tests do not establish soundness.

109 runtime tests pass, including a heap-setting overflow regression.
The full 50-iteration comprehensive concurrency gate passes in debug and release. Concurrent collection machinery exists in the heap implementation,
but the assessment does not establish it as a supported, verified collector mode.
See [STATUS.md](STATUS.md) and [PRODUCTION.md](PRODUCTION.md).
