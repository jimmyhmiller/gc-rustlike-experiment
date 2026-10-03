# Debugging and heap inspection

Five LLDB and three DWARF tests passed on the assessed macOS host. A full-debug
native build disables optimization and emits source locals:

```sh
./target/debug/gcr build examples/shapes.gcr --debug -o /tmp/gcr-shapes
lldb /tmp/gcr-shapes
```

Inside LLDB, import `tools/gcr_lldb.py` with its absolute path. Its summaries decode
the baked `gcrust_type_meta` blob; `frame variable` displays managed values, and
`gcrv <expr>` supplies a reflection-based view. Default native builds emit line
tables, while `--debug` requests full local-variable information.

`gcr emit reflect <file>` emits JSON metadata. Allocation profiling records
source file/line/column sites; tests distinguish sites within a function and
verify module/prelude attribution. It is not merely a function/type histogram.

| Setting/tool | Inspected behavior |
| --- | --- |
| GCR_ALLOC_PROFILE=1 | Dump allocation-site counters |
| GCR_GC_STATS=1 | Dump collection/pause statistics |
| GCR_GC_LOG=path | Write JSONL collection events |
| GCR_HEAP_DUMP=json | Dump a JSON heap snapshot at program end |
| GCR_HEAP_DUMP_FILE=path | Write the program-end snapshot to a file |
| GCR_HEAP_SNAPSHOT_DIR=dir | Write numbered snapshots from heap_snapshot() |
| gcr heap file --out path | Write a selected snapshot, preferring an in-program snapshot |
| gcr heap file --series path | Bundle in-program snapshots; fall back to the end snapshot |
| gcr heap-diff before.json after.json | Compare per-type count/byte growth |

`heap_snapshot()` exists as an in-language builtin. Dump code pauses mutators
before reading the shared heap. Mid-execution snapshots can include live frame
roots; a program-end snapshot has a different root set. Reachability and retained
size analysis therefore depend on when the snapshot was taken.

Widgets in `widgets/` consume source, IR, heap, and benchmark data. Their files
exist, but this assessment did not visually validate their rendering or certify
an interactive debugger comparable to a mature managed-language toolchain.
