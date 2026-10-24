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

- `Arc<T>` is gc-rust's shared box (no reference count; the collector frees
  it) and shared mutable slots (`Lock`) are atoms.
- The atom registry holds atoms strongly (gc-rust has no weak references), so
  `capture_atoms` also sees unreachable atoms.
- The deadline ticker thread is a `std::thread` daemon; with no destructors
  it runs until the program ends rather than until the engine is dropped.
- Host values (`Value::Native`) are `dyn Any` boxes; `with_mut` gives the
  shared value itself.

## Timing (Apple Silicon M2 Max, whole process, median)

| Benchmark | Rust funct | port | port / Rust |
|---|---|---|---|
| `fib(30)` | 331 ms | 346 ms | 1.05× |
| `loops` (2000×2000) | 372 ms | 398 ms | 1.07× |
| `collatz` (1..100k) | 1.61 s | 1.70 s | 1.06× |
| `list` (10k map/filter/fold) | 18.6 ms | 14.8 ms | 0.79× |
| startup (`println(1)`) | 2.31 ms | 2.48 ms | 1.07× |
| `examples/demo.ft` | 2.44 ms | 2.66 ms | 1.09× |
| `funct test examples` | 28.8 ms | 29.3 ms | 1.02× |
| lex `chess.ft` | 0.78 ms | 0.80 ms | 1.03× |
| lex + parse `chess.ft` | 1.74 ms | 1.40 ms | 0.81× |

The VM rows were measured with hyperfine. Startup and `demo.ft` are within
the noise of process launch, so they come from runs interleaved in random
order. The lex rows are per pass (`lexbench`/`parsebench`). In retired
instructions the VM is 1.07× (fib), 1.08× (loops) and 1.08× (collatz).
