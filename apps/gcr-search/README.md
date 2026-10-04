# gcr-search

A native gc-rust CLI that saves a directory's text snapshot and searches it.
Scanning, framing, decoding, and query logic are written in gc-rust. The runtime
supplies checked host I/O; files and directory handles stay within those calls.

This is the first application milestone. Queries currently scan stored text;
queries use bounded parallel batches of four documents, while there is no
persistent worker pool, inverted index, watch daemon, or HTTP/UI yet.

## Run

From the repository root:

```sh
cargo gcr-build
./target/debug/gcr build apps/gcr-search
./apps/gcr-search/target/gcr-search index . /tmp/gcr-search.index
./apps/gcr-search/target/gcr-search query 'gc_every_alloc' /tmp/gcr-search.index
```

The optional final argument selects the index file. The default is
`.gcr-search.index` in the current directory. The native binary runs independently
of Cargo and LLVM once built. Project execution forwards arguments after `--`:

```sh
./target/debug/gcr run apps/gcr-search -- index . /tmp/gcr-search.index
./target/debug/gcr run apps/gcr-search -- query 'gc_every_alloc' /tmp/gcr-search.index
```

Explicit JIT execution runs the same application and argument interface:

```sh
./target/debug/gcr run apps/gcr-search --jit -- index . /tmp/gcr-search.index
./target/debug/gcr run apps/gcr-search --jit --gc-stress -- query 'gc_every_alloc' /tmp/gcr-search.index
```

Manifest JIT runs return the program's exit status and preserve its stdout.
Projects with native link configuration require native execution.

## Behavior

- Case-sensitive, literal search, restricted to a single line. Empty queries and
  queries containing CR/LF are rejected.
- One JSON object per matching line: `path`, 1-based `line`, 1-based
  `byte_column` of its first match, and complete `text`. Columns count UTF-8
  bytes. CR bytes in CRLF lines remain in the returned text.
- Directory entries are sorted and traversed depth-first, giving deterministic
  snapshot bytes and result order. Empty text files are included.
- `.git`, `target`, `.worktrees`, and `node_modules` are excluded. The default
  index filename and the selected output path are also excluded.
- Symlinks and special files are skipped. Invalid UTF-8, NUL-containing files,
  and files larger than 1 MiB are skipped. Non-UTF-8 paths/arguments produce an
  error. Other filesystem errors fail indexing and preserve the old snapshot.
- Limits: 10,000 documents, 16 MiB total document contents, 128 directory levels,
  100,000 entries per directory, 100,000 visited paths, and 10,000 queued paths.
  Query loads are capped at 64 MiB.
- Index replacement uses a synced sibling temporary, rename, and parent-directory
  sync. Existing regular-file permissions are preserved. Failures before rename
  preserve the previous file; a durability failure after rename is reported.
- A snapshot remains unchanged when source files change. Run `index` again to
  replace it. Concurrent source edits do not provide a filesystem-wide snapshot
  guarantee; a file disappearing during the scan produces an error.
- Exit status: 0 success (including no matches), 1 operation/query error, 2 usage
  error. Search data goes to stdout; summaries and diagnostics go to stderr.

The versioned `GCRSEARCH1` format uses checked decimal byte lengths for each
path and document, so newline-containing names and text do not break framing.
The decoder rejects unknown versions, overflowing lengths, truncation, invalid
counts, and trailing data. It does not execute or open paths from an index.

## Verification

```sh
cargo test --test search_app --test stdlib_io --test stdlib_parallel --test modules
GCR_GC_VERIFY=1 cargo test --release --test search_app --test stdlib_io --test stdlib_parallel --test modules --test concurrency_stress
```

JIT and native tests compare results with a simple serial reference, reload snapshots,
check deterministic replacement, test every truncated prefix of a small valid
snapshot, and exercise Unicode, CRLF, escaped output, empty trees, exclusions,
and symlink cycles. Index/query tests run with collection on every allocation.
Subprocess tests have deadlines and kill/wait on timed-out children.

Local execution is verified on macOS ARM64. The full repository has been indexed
and queried in normal mode. Stress coverage uses small fixtures; it does not
establish a sustained-service or cross-platform guarantee.

## Next milestone

Add a persistent worker pool for indexing and incremental watch mode with concurrent queries,
cancellation, and shutdown. Measure retained heap, allocation churn, GC pauses,
and handle counts before expanding limits. A persistent term/posting index and
local HTTP/UI are later application work.
