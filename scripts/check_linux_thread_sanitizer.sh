#!/bin/bash
# Instrument the complete Rust runtime and standard library on native Linux.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
  echo 'Linux ThreadSanitizer gate requires native x86-64 Linux.' >&2
  exit 1
fi
toolchain=nightly-2026-06-24
rustc +"$toolchain" -Vv
uname -a
export RUSTFLAGS='-Zsanitizer=thread -Cforce-frame-pointers=yes'
export TSAN_OPTIONS=halt_on_error=1
for workers in 1 4; do
  echo "Linux TSAN runtime: workers=$workers"
  GCR_GC_WORKERS="$workers" GCR_GC_VERIFY=1 GCRUST_POISON=1 \
    timeout 600s cargo +"$toolchain" test --locked -Zbuild-std --release \
    -p gcrust-rt --target x86_64-unknown-linux-gnu --target-dir target/tsan-linux \
    --lib -- --test-threads=1
  echo "Linux TSAN detector: workers=$workers"
  GCR_GC_WORKERS="$workers" GCR_GC_VERIFY=1 GCRUST_POISON=1 \
    timeout 60s cargo +"$toolchain" test --locked -Zbuild-std --release \
    -p gcrust-rt --target x86_64-unknown-linux-gnu --target-dir target/tsan-linux \
    --lib gc::tests::semi_space_detector_panics_on_bad_header_in_traced_slot \
    -- --include-ignored --exact
done
