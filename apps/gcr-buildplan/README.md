# gcr-buildplan

Plan a worker-limited build or batch pipeline from a dependency CSV. The app
reports each task's start, finish, and assigned worker, the schedule's makespan,
and the dependency critical path. It models durations; it does not execute
commands or claim to compute an optimal schedule.

```sh
cargo gcr-build
./target/debug/gcr run apps/gcr-buildplan -- apps/gcr-buildplan/examples/release.csv 2 -
./target/debug/gcr run apps/gcr-buildplan --jit -- apps/gcr-buildplan/examples/release.csv 2 -
./target/debug/gcr build apps/gcr-buildplan -o /tmp/gcr-buildplan
/tmp/gcr-buildplan apps/gcr-buildplan/examples/release.csv 2 /tmp/plan.json
```

CLI: `<tasks.csv> <workers:1..64> <output.json|->`. The sample's makespan and
critical path are both 9. A header-only file produces an empty schedule.

The exact header is `task,duration,depends`. Task names and dependency names are
trimmed, nonempty, and case-sensitive. Names may contain Unicode or quoted commas
but cannot contain semicolons. Durations are nonnegative signed-64-bit integers;
zero-duration tasks are supported. Dependencies are separated by semicolons;
an empty dependency cell means no dependencies. Forward references are allowed.
Duplicate task names, duplicate dependencies, self-dependencies, unknown names,
cycles, malformed CSV, and overflowing schedule times are errors. A cycle error
lists every blocked task, including descendants of the cycle.

At each event time, all tasks finishing then complete before dispatch. Ready
jobs sort by UTF-8 task-name bytes and use the lowest free worker number (starting
at 1). Zero-duration completions create another dispatch round at the same time.
Output tasks sort by name, so input order does not change the schedule. The
critical path assumes unlimited workers and can be smaller than the makespan.
The scheduler uses ordinary owner-local maps, arrays, and minimum heaps; the
worker count models capacity without creating OS threads.

Limits: 1 MiB input, 2,000 tasks, 10,000 dependency edges. The app loads the whole
graph and stores both dependencies and child lists. File output atomically
replaces the destination after successful validation and scheduling; `-` emits
one JSON line. Status 0 means success, 1 a data/I/O error, and 2 invalid usage.
Output errors follow [checked I/O semantics](../../docs/io.md).

`tests/real_apps.rs` compares deterministic generated DAG schedules with an
independent scan-based scheduler, including reverse input order, simultaneous
finishes, zero durations, multiple capacities, native/JIT and GC stress. Error
cases verify cycles and preserved output. Run
`cargo test --test real_apps --test app_primitives`.
