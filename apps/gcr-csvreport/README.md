# gcr-csvreport

Group signed integer values from real CSV files and emit a JSON report. This is
useful for purchase totals, event counters, and exported metrics with integer
units. Each group has a count, sum, minimum, and maximum; the report also records
file count, data-row count, input byte count, and total sum.

```sh
cargo gcr-build
./target/debug/gcr run apps/gcr-csvreport -- 4 category cents - apps/gcr-csvreport/examples/purchases.csv
./target/debug/gcr run apps/gcr-csvreport --jit -- 4 category cents - apps/gcr-csvreport/examples/purchases.csv
./target/debug/gcr build apps/gcr-csvreport -o /tmp/gcr-csvreport
/tmp/gcr-csvreport 4 category cents /tmp/report.json apps/gcr-csvreport/examples/purchases.csv
```

CLI: `<workers:1..8> <group-column> <value-column> <output.json|-> <file> [file ...]`.
A dash writes one JSON line to stdout. File output uses checked atomic replacement
only after all input processing succeeds. The sample has three rows and a total
of 1500 cents. Groups sort by UTF-8 bytes. Repeated input paths count repeatedly.

Headers must be nonempty and unique, and every row must have the header's width.
Column names match exactly. Group keys preserve whitespace and may be empty.
Values accept ASCII decimal digits with an optional minus; surrounding ASCII
whitespace is trimmed. Decimal fractions, leading plus, and values outside i64
are errors. Every intermediate file/group/merged sum is checked; input file and
row order define accumulation, so an overflowing intermediate sum is an error
also when later rows would bring the mathematical total back into range.

CSV supports commas, quoted fields, doubled quotes, embedded line endings,
Unicode, LF/CRLF/CR record endings, and an initial UTF-8 BOM. Malformed quotes
produce a record and byte diagnostic. See [the CSV API](../../docs/csv-heap.md).

One fixed set of at most eight OS threads claims files with an atomic counter.
Each worker owns its ordinary unsynchronized maps and publishes a completed
report through a bounded `Channel`. The driver receives every completion, joins
all workers, then merges in input order. Completion timing never changes output
or the selected input error. Input paths are safely published and never mutated.
These are ordinary collections with explicit ownership; they have no implicit
collection locks.

Limits: 32 input occurrences, 512 KiB per file, 20,000 data rows per file, 256
columns, 10,000 groups per file and globally, and 200,000 total rows. Files are
read when claimed; supply stable input files for a stable snapshot. This is a
bounded in-memory processor; streaming and cancellation are future work.

Exit status is 0 on success, 1 on invalid data or I/O failure, and 2 on usage
errors. Processing failures emit no partial JSON and preserve existing output.
An output I/O failure can occur after stdout bytes or an atomic rename have
already been published; see [checked I/O semantics](../../docs/io.md).

`tests/real_apps.rs` compares native/JIT output with serial Rust grouping across
12 files, Unicode, multiline/quoted/empty keys, several worker counts, and moving
GC stress. It checks malformed CSV, numeric limits, checked sums, and output
preservation. Run `cargo test --test real_apps --test app_primitives`.
