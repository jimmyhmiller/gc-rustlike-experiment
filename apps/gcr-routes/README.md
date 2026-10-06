# gcr-routes

Find a cheapest directed route through an exported edge list. The JSON result
contains source, destination, reachability, distance, path, node count, and edge
count. Costs can represent travel time, integer fees, or another additive unit.

```sh
cargo gcr-build
./target/debug/gcr run apps/gcr-routes -- apps/gcr-routes/examples/commute.csv home work -
./target/debug/gcr run apps/gcr-routes --jit -- apps/gcr-routes/examples/commute.csv home work -
./target/debug/gcr build apps/gcr-routes -o /tmp/gcr-routes
/tmp/gcr-routes apps/gcr-routes/examples/commute.csv home work /tmp/route.json
```

CLI: `<edges.csv> <from> <to> <output.json|->`. The sample chooses
`home -> park -> cafe -> work`, with cost 7.

The exact CSV header is `from,to,cost`. Endpoints preserve their exact Unicode
text, including whitespace, and must be nonempty. Costs are trimmed nonnegative
i64 integers. Zero costs, self loops, and parallel edges are supported. The graph
is directed: reverse edges must be supplied explicitly. Nodes exist when they
appear in an edge, so a self loop can declare an otherwise isolated node.
Unknown endpoints, invalid numbers, negative costs, or malformed CSV are errors.

Dijkstra's algorithm uses a generic minimum heap and skips stale queue entries.
Equal-cost candidates sort by node id (first appearance in CSV); equal-distance
updates retain the first predecessor. The resulting path is deterministic for a
fixed input but is not promised to be the lexicographically first cheapest path.
A route to the source itself has cost 0 and a one-node path.

Cost additions are checked. Overflowing candidates are skipped so a valid
alternative route can still win. If the target is graph-reachable but no path
has a representable i64 cost, the app reports an overflow error. A disconnected
target is a successful result with `reachable:false`, `distance:null`, and an
empty path. An exact `9223372036854775807` distance is supported.

Limits: 1 MiB input, 10,000 edges, 5,000 nodes. Maps, arrays, and heaps are ordinary
unsynchronized collections owned by the main thread. The graph is loaded into
memory. File output uses atomic replacement after a complete result; a dash
writes one JSON line. Status 0 means success, 1 a data/I/O error, and 2 usage
error. Output errors follow [checked I/O semantics](../../docs/io.md).

`tests/real_apps.rs` compares generated graphs with an independent Bellman-Ford
model and verifies every returned path's cost. Tests cover native/JIT, moving-GC
stress, zero-cost edges, numeric boundaries, disconnected graphs, and output
preservation. Run `cargo test --test real_apps --test app_primitives`.
