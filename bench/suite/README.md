# Comparative benchmark sources

Directories contain gc-rust, Rust, Go, and Java sources for nbody, spectralnorm,
fannkuchredux, and binarytrees. `bench/run_suite.py` compiles them, compares numeric
output, and invokes hyperfine; `bench/gen_report.py` renders its saved results.

```sh
python3 bench/run_suite.py
python3 bench/gen_report.py
```

The harness needs the relevant native/compiler tools plus hyperfine. This
assessment did not rerun it. Read [measurement requirements](../RESULTS.md) before
publishing a ratio. Inputs for gc-rust are hardcoded; other versions receive the
harness's configured arguments.

## Attribution

Retain the attribution/license headers in the competitor sources. Existing
credits identify Programming-Language-Benchmarks
(https://github.com/hanabi1224/Programming-Language-Benchmarks, MIT) and the
Computer Language Benchmarks Game
(https://salsa.debian.org/benchmarksgame-team/benchmarksgame/, BSD-3-Clause).
Individual source headers are the reference for authorship and terms. This
assessment does not claim that each file remains an unmodified upstream copy.
