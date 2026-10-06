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
for fixture in managed_memory channel_contract; do
  GCR_CODEGEN_TSAN=1 \
  GCRUST_RUNTIME_LIB="$PWD/target/tsan-runtime/aarch64-apple-darwin/release/libgcrust_rt.a" \
    ./target/tsan-compiler/debug/gcr build "tests/fixtures/$fixture.gcr" \
    -o "target/tsan-runtime/$fixture"
done
python3 - <<'PYTSAN'
import os, subprocess
for fixture in ('managed_memory', 'channel_contract'):
    for stress in ('0', '1'):
        env = os.environ.copy()
        env.update(GCR_GC_STRESS=stress, GCR_GC_VERIFY='1', GCR_GC_WORKERS='4',
                   GCR_NURSERY_MB='1', GCR_TENURED_MB='8', TSAN_OPTIONS='halt_on_error=1')
        print('Instrumented native ' + fixture + '; GC stress=' + stress, flush=True)
        subprocess.run(['target/tsan-runtime/' + fixture], env=env, check=True, timeout=90)
PYTSAN
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
