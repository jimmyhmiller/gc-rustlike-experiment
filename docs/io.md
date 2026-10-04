# Checked I/O and application support

`src/stdlib/io.gcr` is embedded alongside `prelude.gcr` and supplies automatically
imported APIs. `crates/gcrust-rt/src/runtime/io.rs` performs host operations while
the mutator is BLOCKED; it copies managed inputs into owned buffers before that
transition. Native handles close before managed result construction. Result
objects and directory entries use scratch roots and write barriers while being
built, including under collection on every allocation.

| API | Result / behavior |
| --- | --- |
| `process_args()` | `Result<Array<String>, IoError>`; native argv includes executable at index 0 |
| `fs_read_text(path, max_bytes)` | Checked, bounded UTF-8 read of a regular file |
| `fs_read_dir(path)` | Sorted `Array<String>` entry names, capped at 100,000 |
| `fs_kind(path)` | 1 file, 2 directory, 3 symlink, 4 other; does not follow symlinks |
| `path_absolute(path)` | Absolute host path; preserves `..` semantics around symlinks |
| `path_join(parent, name)` | POSIX string path join; absolute name replaces parent |
| `fs_write_text_atomic(path, text)` | Synced temporary plus rename; preserves existing regular-file permissions |
| `stdout_line(text)` | Checked line write/flush; returns `Result<i64, IoError>` |
| `stderr_line(text)` | Line write; returns 0 or an I/O error category |

These checked APIs return `IoError` with operation, path, category, and message.
Categories: 1 not found, 2 permission denied, 3 invalid data/input, 4 other I/O.
`io_error_message` renders the diagnostic. Invalid UTF-8 paths/arguments and paths
containing NUL are errors. A parent-directory sync failure after an atomic rename
is reported even though the new file has already been published.

Native `process_args` reads that executable's argv. JIT execution owns an
immutable argument context inherited by spawned threads. The CLI supplies the
entry path as argv[0] and forwards arguments after `--`; embedding code can use
`jit_run_i64_with_args`. Older embedding helpers default to host argv.
Legacy `arg_count`/`arg_str` now read real arguments, but retain their empty/zero
fallbacks. Legacy `read_file` retains its empty-on-error behavior; use
`fs_read_text` when errors must be preserved.

`str_join` now uses linear native assembly and one managed allocation rather
than repeatedly copying its growing output. `str_find_from` performs literal
byte search without per-candidate substring allocations. `str_find_range`
restricts search to a byte interval; see [parallel.md](parallel.md). `json_quote` encodes
UTF-8 strings with escaped control bytes. `vec_at` and `opt_expect` provide
explicitly checked extraction; failure calls `panic`, which diagnoses and aborts.

The internal `host_io` intrinsic has a fixed response ABI. Lowering rejects a
shadowed `IoResponse` with incompatible fields rather than emitting unsafe
layout accesses. Applications should use the typed wrappers.

The first consumer is [gcr-search](../apps/gcr-search/README.md). These APIs and
that app are locally verified on macOS ARM64; POSIX path helpers do not establish
Windows support. File-watching, streaming public handles, process spawning, and
networking are not part of this milestone.
