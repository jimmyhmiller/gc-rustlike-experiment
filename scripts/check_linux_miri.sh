#!/bin/bash
# Focused unsafe-runtime checks on Jimmy's native Linux host (or Linux CI).
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
  echo 'Linux Miri validation requires native x86-64 Linux.' >&2
  exit 1
fi
unset MIRIFLAGS MIRI_SYSROOT RUSTFLAGS RUSTDOCFLAGS CARGO_TARGET_DIR
rustc +nightly-2026-06-24 -Vv
cargo +nightly-2026-06-24 miri --version
timeout -k 10s 300s cargo +nightly-2026-06-24 miri setup
for seed in 0 1 17; do
  for case in storage-moves allocator-window aggregate-reload poison; do
    flags="-Zmiri-seed=$seed"
    case "$case" in
      storage-moves)
        test_name=runtime::abi_storage_tests::collector_runtime_thread_storage_survives_owner_moves_and_polling ;;
      allocator-window)
        test_name=gc::heap::abi_window_tests::collector_exported_window_survives_heap_moves_and_allocation ;;
      aggregate-reload)
        test_name=runtime::managed::tests::collector_aggregate_write_contention_reloads_moved_base_and_indirect_rhs
        flags="$flags -Zmiri-env-set=GCR_AGGREGATE_RELOAD_CHILD=1" ;;
      poison)
        test_name=gc::lock_safe_tests::collector_lock_safe_poison_preserves_running_state ;;
    esac
    echo "Linux Miri: case=$case seed=$seed"
    echo "Miri flags: $flags"
    MIRIFLAGS="$flags" timeout -k 10s 90s cargo +nightly-2026-06-24 miri test \
      --locked -p gcrust-rt --lib "$test_name" -- --exact --test-threads=1
  done
done
echo 'Linux Miri probes: 12 passed'
