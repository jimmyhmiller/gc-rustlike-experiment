# Ordered parallel mapping

`vec_parallel_map<T: Sync, U: Sync>(items, workers, callback)` returns
`Result<Vec<U>, String>`. Worker counts must be 1–64; invalid counts return an
error even for empty inputs. Results preserve input order. Each batch is fully
joined before the next starts, so at most `workers` OS threads run at a time and
all workers finish before return. This implementation creates threads per batch;
it is not a persistent worker pool.

```rust
let mut values: Vec<i64> = vec_new();
values = vec_push(values, 2);
values = vec_push(values, 3);
let doubled = vec_parallel_map(values, 2, |n: i64| n * 2);
```

Inputs and outputs currently must satisfy structural `Sync`. Direct spawn
capture checks can be bypassed through function aliases and higher-order calls;
current codegen does not guarantee safe arbitrary managed races. The application
uses immutable document inputs and a string capture. The target contract permits
shared mutable objects once managed access and GC coordination are race-safe;
see [concurrency.md](concurrency.md). These bounds are transitional.

Callbacks that abort abort the process; worker creation failure follows the
runtime's existing fatal-error behavior. Cancellation and recoverable worker
creation errors are not supported. The returned vector retains all results;
callers processing large inputs should invoke this helper on bounded batches.

The search app processes four documents at a time. Workers produce match offsets;
the main thread renders and writes matching lines in order. It avoids retaining
all rendered output. Searches use `str_find_range(text, needle, from, end)`, which
restricts literal byte search to `[from, end)` and returns an absolute byte offset.
Negative bounds clamp to zero, end clamps to string length, and reversed ranges
or starts beyond the string return -1. Empty needles match at a valid start.

Value arrays now store each written value in a GC-traced box. Embedded references
are traced, copying an element preserves value semantics, and unset reference/inline-value elements fail rather than fabricating values. This costs an allocation per value-element store.
Nested value-enum references use shared leading pointer slots, with 8-aligned raw
payloads; closure environments trace embedded captured-value references too.

Tests run the mapping and application through JIT and native AOT with GC stress,
including ordered output, worker limits, nested value/enum payloads, Unicode,
captured references, and named generic callbacks. Platform evidence remains
macOS ARM64.
