# Existing examples

Build with `cargo gcr-build`, then use `./target/debug/gcr check <file>` or
`run <file>`. The example runner verifies 23 expected results in normal mode
on macOS ARM64. The raylib graphics example has not been launched.

| Source | Topic |
| --- | --- |
| examples/fib.gcr | Functions and recursion |
| examples/types.gcr | Types and generic/value examples |
| examples/match.gcr | Matching examples |
| examples/strings.gcr | String helpers |
| examples/stdlib.gcr | Prelude collections |
| examples/mutability.gcr | Explicit mutation |
| examples/threads.gcr | Concurrent allocation |
| examples/atom.gcr, examples/channel.gcr | Shared-state primitives |
| examples/ffi*.gcr | C calls, structs, buffers, callbacks |
| examples/project | Manifest and multiple files |
| examples/raylib | Native graphics example |

`--gc-stress` selects collect-on-allocation execution for both JIT and project
runs. `scripts/run_examples.sh` supports the Bash 3.2 bundled with macOS and
reports compiler/program failures. All 22 examples other than `binary_trees`
passed separate bounded stress runs. The full binary-tree every-allocation run
was stopped after several minutes without a result; it remains unverified in
that mode. Its normal JIT and AOT runs pass. See STATUS.md for coverage.
