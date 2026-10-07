# Private object graphs and bounded field bodies

The compiler now follows references through reference arrays and object fields,
solving memory-flow edges together with origin propagation. Escaping containers
transitively escape their contents, including cycles. Unknown owners, captured
references, native calls and unsupported aggregates remain conservative. Private
scalar/reference accesses use plain operations with origin-scoped alias metadata;
private pointer stores still execute the generational barrier. Mutable aggregate
snapshot accesses retain their locks and safepoints.

Fixed inline field accesses also participate in bounded-body effect analysis.
A body proven nonparking after its entry poll can omit interior traced aliases,
while incoming roots remain available on the entry slow path. The previous
all-function root-slot-selection optimization remains discarded.

Validation: **559 workspace tests passed** (8 ignored), **26 instrumented native
executions** and **7 runtime sanitizer tests**, plus release compilation. The new
private_heap fixture checks aliased private graphs, private old-to-young pointer
writes and shared reference-array children across three mutators in all 12
JIT/native/debug × normal/stress × one/four-worker configurations. Compiler tests
cover cyclic escapes, late stores, opaque option payloads and unknown owners.
The managed-memory conformance audit now admits only atomic or explicitly
private-proven accesses; full-debug fields still must be atomic.

The complete comparison retains unchanged benchmark source hashes, ordered full
output checking, Rust O3 and Java G1, three forks, 20 warmups and 10 samples.

| Case | GCR ms | Rust ms | Java ms | GCR / Java |
|---|---:|---:|---:|---:|
| nbody | 42.1655 | 27.2994 | 44.315 | 0.951 |
| spectralnorm | 36.136 | 35.5689 | 63.0873 | 0.573 |
| fannkuchredux | 19.167 | 13.1391 | 16.4597 | 1.164 |
| binarytrees | 13.058 | 72.6176 | 10.0207 | 1.303 |
| scalar | 39.543 | 39.555 | 44.5865 | 0.887 |
| array | 4.91 | 4.0433 | 6.4207 | 0.765 |
| nbody_objects | 37.164 | 27.0066 | 44.1432 | 0.842 |

Object-based nbody now meets the measured Java median. Fannkuch and trees remain
slower, so the overall JVM-parity target is still open. The raw result and
validation files are private-graph-results.json and private-graph-validation.json.
