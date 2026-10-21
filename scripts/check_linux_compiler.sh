#!/bin/bash
# Full compiler/JIT/native integration on Jimmy's existing Linux host.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
  echo 'Linux compiler validation requires native x86-64 Linux.' >&2
  exit 1
fi
export LLVM_SYS_211_PREFIX=/usr/lib/llvm-21
export CC=/usr/bin/clang-21
rustc +1.96.0 -Vv
"$CC" --version
uname -a
for profile in debug release; do
  profile_args=()
  if [ "$profile" = release ]; then profile_args=(--release); fi
  echo "Linux workspace: profile=$profile"
  timeout 600s cargo +1.96.0 test --locked --workspace "${profile_args[@]}"
  echo "Linux JIT concurrent: profile=$profile"
  timeout 120s cargo +1.96.0 test --locked -p gc-rust "${profile_args[@]}" --lib \
    codegen::tests::generated_aggregate_access_survives_active_concurrent_cycles \
    -- --exact --nocapture
done
echo 'Linux workspace detector: profile=release'
GCR_GC_VERIFY=1 timeout 60s cargo +1.96.0 test --locked -p gcrust-rt --release --lib \
  gc::tests::semi_space_detector_panics_on_bad_header_in_traced_slot \
  -- --include-ignored --exact
