#!/usr/bin/env bash
# Run every example program and assert it produces its known-good result. A
# local regression check (no CI required). Pass `--gc-stress` to additionally
# collect at every allocation, proving the GC stays correct under relocation.
#
#   ./scripts/run_examples.sh            # normal run
#   ./scripts/run_examples.sh --gc-stress
set -uo pipefail
cd "$(dirname "$0")/.."

STRESS=""
if [ "${1:-}" = "--gc-stress" ]; then
  STRESS="--gc-stress"
fi

# Parallel indexed arrays work with the Bash 3.2 bundled with macOS.
NAMES=(atom channel binary_trees ffi ffi_struct ffi_bytes ffi_buffer ffi_callback
       fib mandelbrot match mutability nbody nbody_vec3 prelude_demo shapes stdlib
       threads strings types vec vec_prelude)
EXPECTED=(20000 109900 5242840 1066 1 42 14 1050 2178309 86906 47 33 921463
          1457652585 42 47 414 37492500 35 206 386 285)

# Build once so the per-example runs don't each recompile.
cargo build --quiet --bin gcr || { echo "build failed"; exit 1; }
GCR="./target/debug/gcr"

fail=0
for ((index=0; index<${#NAMES[@]}; index++)); do
  name="${NAMES[$index]}"
  want="${EXPECTED[$index]}"
  if output=$("$GCR" run "examples/$name.gcr" $STRESS); then
    got=$(printf '%s\n' "$output" | tail -1)
  else
    printf "  FAIL %-16s compiler/program failed\n" "$name"
    fail=1
    continue
  fi
  if [ "$got" = "$want" ]; then
    printf "  ok   %-16s = %s\n" "$name" "$got"
  else
    printf "  FAIL %-16s expected %s, got %s\n" "$name" "$want" "$got"
    fail=1
  fi
done

# The multi-file project example (driven by its directory entry).
output=$("$GCR" run examples/modproj/main.gcr $STRESS)
status=$?
got=$(printf '%s\n' "$output" | tail -1)
if [ "$status" = "0" ] && [ "$got" = "32" ]; then
  printf "  ok   %-16s = %s\n" "modproj" "$got"
else
  printf "  FAIL %-16s expected 32, got %s\n" "modproj" "$got"
  fail=1
fi

if [ "$fail" = "0" ]; then
  echo "all examples passed${STRESS:+ (under --gc-stress)}"
else
  echo "some examples FAILED"
fi
exit $fail
