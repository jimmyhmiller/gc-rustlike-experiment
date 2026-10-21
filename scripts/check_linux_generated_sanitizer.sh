#!/bin/bash
# Instrument LLVM-generated programs and Rust runtime with one Clang sanitizer.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
  echo 'Generated Linux sanitizer validation requires native x86-64 Linux.' >&2
  exit 1
fi
export LLVM_SYS_211_PREFIX=/usr/lib/llvm-21
export CC=/usr/bin/clang-21
"$CC" --version
rustc +1.96.0 -Vv
rustc +nightly-2026-06-24 -Vv
uname -a
timeout 600s cargo +1.96.0 build --locked -p gc-rust --bin gcr --target-dir target/linux-compiler
RUSTFLAGS='-Zsanitizer=thread -Zexternal-clangrt -Cforce-frame-pointers=yes' \
  timeout 300s cargo +nightly-2026-06-24 build --locked -Zbuild-std --release \
  -p gcrust-rt --target x86_64-unknown-linux-gnu --target-dir target/linux-aot-tsan
python3 scripts/check_generated_sanitizer.py \
  --compiler target/linux-compiler/debug/gcr \
  --runtime target/linux-aot-tsan/x86_64-unknown-linux-gnu/release/libgcrust_rt.a \
  --output target/linux-aot-tsan
