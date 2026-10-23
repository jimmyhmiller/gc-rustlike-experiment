# funct, ported to gc-rust

A port of [funct](https://github.com/jimmyhmiller/funct) (`~/Documents/Code/funct`),
a small embeddable functional language with a reified bytecode VM, from Rust
to gc-rust. Each `src/*.gcr` file is the matching `src/*.rs` file written as
gc-rust; differences forced by the language are marked `// PORT:`.

- `src/` — all of funct's library and CLI: lexer, parser, AST, bytecode,
  compiler, VM, natives (`prelude.gcr`), host interop, JSON, snapshots, the
  `.ft` test runner, and `main.gcr` (funct's CLI plus `lex`/`parse`
  comparison modes).
- Stand-ins for the crates funct uses: `imbl.gcr` (persistent `Vector` and
  `OrdMap`), `serde.gcr` and `serde_json.gcr` (the derives come from gc-rust's
  `#[derive(Serialize, Deserialize)]`).
- `tests/` — funct's Rust integration tests, ported (all but
  `chess_widget.rs`, which drives a chess engine through `std::process`).
- `examples/` — funct's `.ft` examples, which the tests read.
- `reference/` — a Rust driver over the real funct with the same `lex`/`parse`
  modes, for comparisons.

```sh
gcr build ports/funct -o funct      # the funct CLI: funct [run] file.ft | funct test dir
gcr test ports/funct                # funct's test suite (264 tests)
```

## Differences from the Rust original

- `Arc<T>` is `T` and shared mutable slots (`Lock`) are atoms, so `Value`
  holds its parts directly.
- The atom registry holds atoms strongly (gc-rust has no weak references), so
  `capture_atoms` also sees unreachable atoms.
- The deadline ticker thread is a `std::thread` daemon; with no destructors
  it runs until the program ends rather than until the engine is dropped.
- Host values (`Value::Native`) are `dyn Any` boxes; `with_mut` gives the
  shared value itself.

## Timing (Apple Silicon, hyperfine, whole process)

| Benchmark | Rust funct | port | port / Rust |
|---|---|---|---|
| `fib(30)` | 331 ms | 7.74 s | 23.4× |
| `loops` (2000×2000) | 370 ms | 13.2 s | 35.7× |
| `collatz` (1..100k) | 1.62 s | 58.7 s | 36.2× |
| `list` (10k map/filter/fold) | 18.9 ms | 191 ms | 10.1× |
| startup (`println(1)`) | 2.3 ms | 4.2 ms | 1.8× |
| `examples/demo.ft` | 2.6 ms | 4.7 ms | 1.8× |
| `funct test examples` | 29.4 ms | 109 ms | 3.7× |
| lex `chess.ft` | 0.78 ms | 0.87 ms | 1.11× |
| lex + parse `chess.ft` | 1.79 ms | 1.86 ms | 1.04× |

The front end is at parity; the VM is not. Each interpreted instruction
calls `step_inner`, a single large `match`, and gc-rust's codegen gives that
function a 19 KB frame whose 833 GC root slots are zeroed on every call
(6.6 KB of `bzero` per instruction). Sharing root slots between mutually
exclusive match arms is the next compiler improvement to make.
