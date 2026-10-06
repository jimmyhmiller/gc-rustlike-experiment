# CSV, minimum heaps, and integer parsing

`src/stdlib/csv.gcr` and `src/stdlib/heap.gcr` are automatically imported alongside
the prelude. They are exercised by the CSV reporter, build planner, and route
planner. They require no host CSV library or native priority-queue handle.

## CSV

`csv_parse(text, max_records, max_fields)` returns
`Result<Vec<Vec<String>>, CsvError>`. Limits must be positive. A record may have
one or more fields; an empty field is a valid string. Empty input (or only an
initial BOM) has no records. A final record terminator does not create another
record. A trailing comma creates a final empty field. Blank lines are one-field
records. The parser accepts LF, CRLF, and CR terminators and strips one initial
UTF-8 BOM. Embedded line endings inside quoted fields are preserved verbatim.

Quotes are allowed only at the beginning of a field. A doubled quote inside a
quoted field decodes to one quote. After a closing quote, only a delimiter or EOF
is valid; whitespace there is an error. Commas/newlines inside quotes do not
split fields. The parser preserves all field text and does not trim values or
validate row width; callers validate headers and schemas.

Input is capped at 16 MiB and a decoded field at 1 MiB. Caller limits bound
records and fields per record. `CsvError` carries a 1-based logical record and
1-based UTF-8 byte position plus a message. EOF diagnostics use input length + 1;
records count quoted multiline fields once. `csv_error_message` renders these.

`csv_encode(rows)` returns `Result<String, CsvError>`. It emits CRLF after every
record, escaping commas, quotes, and line endings. A record with one empty field
encodes as CRLF; zero-field records are rejected because CSV cannot distinguish
them from one empty field. An empty row list encodes as an empty string. Fields
are capped at 1 MiB and output at 16 MiB, including escaping and terminators.
`csv_escape(text)` is the unbounded individual-field escaping helper. Parsed
strings and encoder pieces are ordinary managed collections.

## Minimum heap

`heap_new<T>() -> MinHeap<T>` creates an empty heap; an expected type annotation
supplies `T`. `heap_len` reports its size, and `heap_peek` returns `Option<T>` for
the smallest item without removing it. `heap_push` mutates the heap and returns
its new size. `heap_pop` mutates it and returns the minimum or `None` when empty.
Push/pop require `T: Ord` and take O(log n) comparisons. No ordering of equal
items is promised. Reference elements remain references; inline elements retain
value-copy semantics. Comparison methods must define a consistent ordering.

`MinHeap`, Vec, MapStr, and HashMap have no implicit synchronization. Own a heap
in one thread or protect shared mutation externally; mutation through aliases
has the same requirement. Do not change an inserted item's comparison key while
it is in the heap. Concurrent collection variants are separate future APIs.

`str_compare(left,right)` and String's `Ord` implementation compare unsigned
UTF-8 bytes lexicographically. This is deterministic ordering, not locale-aware
collation or Unicode normalization.

## Checked integer parsing

`parse_int(text) -> Option<i64>` accepts ASCII decimal digits with an optional
leading minus. Leading zeroes and negative zero are allowed. Empty strings,
lone minus, plus, whitespace, non-ASCII digits, and out-of-range values return
`None`. Both i64 endpoints are supported. Trim explicitly when the input schema
permits surrounding whitespace. Plain integer arithmetic retains the language's
wrapping semantics; checked sum/time operations must use `checked_add_i64` and
other checked arithmetic helpers.

Regression tests cover CSV round trips and errors, signed integer boundaries,
heap growth/reuse with inline/reference elements, contextual match inference,
mutable pattern payloads, and nested enum payload tracing. App tests independently
model grouping, scheduling, and shortest paths across JIT/native and GC stress.
