#!/bin/bash
# Full runtime validation on a native Linux host.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "$(uname -s)" != Linux ]; then
  echo 'Use check_linux_runtime.py to validate from a non-Linux host.' >&2
  exit 1
fi
rustc +1.96.0 -Vv
uname -a
for profile in debug release; do
  profile_args=()
  if [ "$profile" = release ]; then profile_args=(--release); fi
  for workers in 1 4; do
    echo "Linux runtime: profile=$profile workers=$workers"
    GCR_GC_WORKERS="$workers" GCR_GC_VERIFY=1 GCRUST_POISON=1 \
      timeout 300s cargo +1.96.0 test --locked -p gcrust-rt "${profile_args[@]}" \
      --lib -- --test-threads=1
    echo "Linux detector: profile=$profile workers=$workers"
    GCR_GC_WORKERS="$workers" GCR_GC_VERIFY=1 GCRUST_POISON=1 \
      timeout 60s cargo +1.96.0 test --locked -p gcrust-rt "${profile_args[@]}" \
      --lib gc::tests::semi_space_detector_panics_on_bad_header_in_traced_slot \
      -- --include-ignored --exact
  done
  echo "Linux doctests: profile=$profile"
  timeout 120s cargo +1.96.0 test --locked -p gcrust-rt "${profile_args[@]}" --doc
done
