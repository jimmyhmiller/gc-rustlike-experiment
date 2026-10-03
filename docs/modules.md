# Modules and projects

`src/compile.rs` loads `mod name;` from `name.gcr` or `name/mod.gcr`. Nested file
modules resolve under the module's directory. It records each loaded file in the
source map and injects prelude declarations unless the user declares the same
name. `src/resolve.rs` resolves module paths, visibility, and use aliases.
Seven module integration tests passed in the local assessment.

`gcr.toml` names a project and its entry:

```toml
[package]
name = "myapp"
version = "0.1.0"
entry = "src/main.gcr"
```

The entry defaults to `src/main.gcr`. `[link]` accepts TOML string arrays
for `libs`, `lib-paths`, `frameworks`, and `args`; these become native linker
arguments. The driver discovers manifests from source paths and parent folders,
and accepts project directories. A discovered manifest changes `run` to native
build-and-execute, even for a file argument.

`src/manifest.rs` deserializes TOML into a typed schema. It rejects unknown keys,
duplicate keys, incorrect value types, malformed syntax, empty names/entries, and
package names containing path components. Manifest discovery reports errors; an
invalid parent manifest is not silently ignored. Multiline arrays, escaped quotes,
and `#` inside quoted strings follow TOML syntax.
It does not resolve managed dependencies or maintain a package lockfile. Examples:
`examples/project` and `examples/raylib`.
