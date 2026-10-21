#!/bin/bash
# Instrument Rust runtime AND generated native code. Requires a nightly
# toolchain with rust-src, Python 3, LLVM 21 and a Clang ThreadSanitizer runtime.
set -euo pipefail
cd "$(dirname "$0")/.."
toolchain="${GCR_TSAN_TOOLCHAIN:-nightly}"
if [[ "$(uname -s)" != Darwin || "$(uname -m)" != arm64 ]]; then
  echo 'This sanitizer gate currently supports macOS ARM64 only.' >&2
  exit 1
fi
tsan_runtime_dir="$(xcrun clang -print-resource-dir)/lib/darwin"
tsan_runtime="$tsan_runtime_dir/libclang_rt.tsan_osx_dynamic.dylib"
test -f "$tsan_runtime"
cargo build -p gc-rust --bin gcr --target-dir target/tsan-compiler
# Each build uses its own target directory; ordinary AOT artifacts stay intact.
RUSTFLAGS='-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes' \
  cargo "+$toolchain" build -Zbuild-std --release -p gcrust-rt \
  --target aarch64-apple-darwin --target-dir target/tsan-runtime
python3 scripts/check_generated_sanitizer.py \
  --compiler target/tsan-compiler/debug/gcr \
  --runtime target/tsan-runtime/aarch64-apple-darwin/release/libgcrust_rt.a \
  --output target/tsan-runtime
# Native Rust regression: registered BLOCKED workers race pause release and
# retirement of their poll storage. Rebuild std too, matching the sanitizer ABI.
# Rust supplies -nodefaultlibs, so link the external runtime explicitly. Use the
# same Apple runtime as the generated executable, including instrumented std.
RUSTFLAGS="-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes -Clink-arg=$tsan_runtime -Clink-arg=-Wl,-rpath,$tsan_runtime_dir" TSAN_OPTIONS=halt_on_error=1 \
  cargo "+$toolchain" test -Zbuild-std --release -p gcrust-rt \
  --target aarch64-apple-darwin --target-dir target/tsan-tests-external poll_ -- --test-threads=1

RUSTFLAGS="-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes -Clink-arg=$tsan_runtime -Clink-arg=-Wl,-rpath,$tsan_runtime_dir" TSAN_OPTIONS=halt_on_error=1 \
  cargo "+$toolchain" test -Zbuild-std --release -p gcrust-rt \
  --target aarch64-apple-darwin --target-dir target/tsan-tests-external channel_ -- --test-threads=1

# Collector visitation must also exclude Rust observers that are not managed
# mutators. These tests exercise global root storage under the same TSAN build.
RUSTFLAGS="-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes -Clink-arg=$tsan_runtime -Clink-arg=-Wl,-rpath,$tsan_runtime_dir" TSAN_OPTIONS=halt_on_error=1 \
  cargo "+$toolchain" test -Zbuild-std --release -p gcrust-rt \
  --target aarch64-apple-darwin --target-dir target/tsan-tests-external collector_ -- --test-threads=1

# Nested Rust execution must publish the suspended outer mutator's roots and
# allow its heap to collect while the inner context owns this OS thread.
RUSTFLAGS="-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes -Clink-arg=$tsan_runtime -Clink-arg=-Wl,-rpath,$tsan_runtime_dir" TSAN_OPTIONS=halt_on_error=1 \
  cargo "+$toolchain" test -Zbuild-std --release -p gcrust-rt \
  --target aarch64-apple-darwin --target-dir target/tsan-tests-external current_thread_binding -- --test-threads=1

# Inspection must also suspend registered callers before waiting on GC, preserve
# nested registration state and report incomplete transient-root snapshots.
RUSTFLAGS="-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes -Clink-arg=$tsan_runtime -Clink-arg=-Wl,-rpath,$tsan_runtime_dir" TSAN_OPTIONS=halt_on_error=1 \
  cargo "+$toolchain" test -Zbuild-std --release -p gcrust-rt \
  --target aarch64-apple-darwin --target-dir target/tsan-tests-external snapshot_ -- --test-threads=1

# Terminal diagnostic I/O must abort without Rust unwinding even if stderr fails.
RUSTFLAGS="-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes -Clink-arg=$tsan_runtime -Clink-arg=-Wl,-rpath,$tsan_runtime_dir" TSAN_OPTIONS=halt_on_error=1 \
  cargo "+$toolchain" test -Zbuild-std --release -p gcrust-rt \
  --target aarch64-apple-darwin --target-dir target/tsan-tests-external diagnostic_ -- --test-threads=1

# Heap-bound allocation requests retain validation and select fresh arenas after GC.
RUSTFLAGS="-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes -Clink-arg=$tsan_runtime -Clink-arg=-Wl,-rpath,$tsan_runtime_dir" TSAN_OPTIONS=halt_on_error=1 \
  cargo "+$toolchain" test -Zbuild-std --release -p gcrust-rt \
  --target aarch64-apple-darwin --target-dir target/tsan-tests-external allocation_requests -- --test-threads=1

# Allocation profiling reads owner-local counters only at quiescent snapshots,
# including resizing workers, retirement merges and saturated report totals.
RUSTFLAGS="-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes -Clink-arg=$tsan_runtime -Clink-arg=-Wl,-rpath,$tsan_runtime_dir" TSAN_OPTIONS=halt_on_error=1 \
  cargo "+$toolchain" test -Zbuild-std --release -p gcrust-rt \
  --target aarch64-apple-darwin --target-dir target/tsan-tests-external alloc_site_profile_ -- --test-threads=1
