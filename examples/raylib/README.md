# Raylib example

`dodge.gcr` contains a falling-blocks game using raylib C declarations.
`shot.gcr` contains a screenshot harness. `gcr.toml` names the game entry and
links raylib plus macOS frameworks from the configured library directory.

Build the compiler/runtime from the repository root:

```sh
cargo gcr-build
./target/debug/gcr build examples/raylib -o /tmp/gcr-dodge
```

This requires raylib and the native libraries named by the manifest. Adjust its
library paths to the local installation. The checked-in manifest targets a
Homebrew/macOS layout. The assessment did not build or launch this graphical
example, so it supplies no current gameplay or continuous-GC verification.

The source exercises value-struct C arguments and string-buffer conversion. See
[FFI limits](../../docs/ffi.md) and [current test evidence](../../docs/STATUS.md).
