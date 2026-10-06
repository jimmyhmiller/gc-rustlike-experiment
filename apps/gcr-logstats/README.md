# gcr-logstats

A parallel UTF-8 log-file analyzer written in gc-rust. It emits one JSON summary
with file count, UTF-8 byte count, line count, and counts of lines containing the
case-sensitive literals `ERROR` and `WARN`. A line containing both contributes
to both counts; repeated occurrences in a line count once. Empty files have zero
lines, CRLF keeps its CR byte, and a final newline does not create an extra line.
This is a literal log summary, not a structured JSON/syslog parser.

```sh
cargo gcr-build
./target/debug/gcr run apps/gcr-logstats -- 4 server.log worker.log
./target/debug/gcr run apps/gcr-logstats --jit -- 4 server.log worker.log
./target/debug/gcr build apps/gcr-logstats -o /tmp/gcr-logstats
/tmp/gcr-logstats 4 server.log worker.log
```

Workers claim file positions with `AtomicI64.fetch_add`. Each scans its own file
and publishes an immutable `Summary` through `Atom<Summary>.swap`. CAS retry
callbacks only construct a new summary; they never repeat I/O or other effects.
Published summaries and the shared input list are never mutated. This exercises
pointer CAS and allocation under contention without depending on the unfinished
ordinary managed-object race semantics.

Worker counts must be 1–64; at most one worker per input is started. One fixed
set of OS threads processes the entire input list, avoiding thread creation per
file. Inputs are capped at 10,000 paths and 1 MiB per file. Each argument is an
independent input occurrence: repeating a path intentionally counts it again.
Files are read when claimed, so callers requiring a stable snapshot must supply
stable files. Memory includes the argv/path list and up to one current file per
worker, plus GC reserve and delayed reclamation.

All workers are joined before returning. Read errors report a diagnostic with
exit status 1 and no partial JSON summary. Missing arguments return 2; invalid
worker counts return 1. Output errors return 1. Worker creation failure and
unrecoverable allocation failure still follow the runtime's fatal policy.
Cancellation, streaming large files and live monitoring remain future work.

`tests/logstats_app.rs` compares JIT and native output against a serial Rust
model, using 1/4/64 configured workers, normal and every-allocation GC, Unicode
paths/content, empty files, CRLF, final unterminated lines, literal boundaries,
invalid UTF-8 and missing inputs. Child processes have deadlines. Run:

```sh
cargo test --test logstats_app
GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release --test logstats_app
```
