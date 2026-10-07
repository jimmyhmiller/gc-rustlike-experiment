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
for fixture in managed_memory channel_contract app_payloads csv_limits inline_allocation root_mirror_callback private_arrays; do
  GCR_CODEGEN_TSAN=1 \
  GCRUST_RUNTIME_LIB="$PWD/target/tsan-runtime/aarch64-apple-darwin/release/libgcrust_rt.a" \
    ./target/tsan-compiler/debug/gcr build "tests/fixtures/$fixture.gcr" \
    -o "target/tsan-runtime/$fixture"
done
for app in gcr-csvreport gcr-buildplan gcr-routes; do
  GCR_CODEGEN_TSAN=1 \
  GCRUST_RUNTIME_LIB="$PWD/target/tsan-runtime/aarch64-apple-darwin/release/libgcrust_rt.a" \
    ./target/tsan-compiler/debug/gcr build "apps/$app" -o "target/tsan-runtime/$app"
done
python3 - <<'PYTSAN'
import json, os, subprocess
for fixture in ('managed_memory', 'channel_contract', 'app_payloads', 'csv_limits', 'inline_allocation', 'root_mirror_callback', 'private_arrays'):
    for stress in ('0', '1'):
        env = os.environ.copy()
        env.update(GCR_GC_STRESS=stress, GCR_GC_VERIFY='1', GCR_GC_WORKERS='4',
                   GCR_NURSERY_MB='1', GCR_TENURED_MB='64',
                   GCR_STRESS_HEAP_MB='64' if fixture == 'csv_limits' else '8',
                   TSAN_OPTIONS='halt_on_error=1')
        print('Instrumented native ' + fixture + '; GC stress=' + stress, flush=True)
        subprocess.run(['target/tsan-runtime/' + fixture], env=env, check=True, timeout=90)
cases = [
        ('gcr-csvreport', ['4', 'category', 'cents', '-',
            'apps/gcr-csvreport/examples/purchases.csv',
            'apps/gcr-csvreport/examples/purchases.csv'],
            {'files': 2, 'rows': 6, 'total': 3000}),
        ('gcr-buildplan', ['apps/gcr-buildplan/examples/release.csv', '2', '-'],
            {'workers': 2, 'makespan': 9, 'critical_path': 9}),
        ('gcr-routes', ['apps/gcr-routes/examples/commute.csv', 'home', 'work', '-'],
            {'distance': 7, 'path': ['home', 'park', 'cafe', 'work']}),
    ]
for app, args, expected in cases:
    for stress in ('0', '1'):
        env = os.environ.copy()
        env.update(GCR_GC_STRESS=stress, GCR_GC_VERIFY='1', GCR_GC_WORKERS='4',
                   GCR_NURSERY_MB='1', GCR_TENURED_MB='16', TSAN_OPTIONS='halt_on_error=1')
        print('Instrumented native ' + app + '; GC stress=' + stress, flush=True)
        output = subprocess.run(['target/tsan-runtime/' + app, *args], env=env,
                                check=True, capture_output=True, text=True, timeout=90)
        result = json.loads(output.stdout)
        assert all(result[key] == value for key, value in expected.items()), result
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
