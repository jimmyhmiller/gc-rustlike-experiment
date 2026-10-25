#!/bin/bash
# Execute built-in header/collector regressions on real big-endian s390x code.
# Requires Docker. No privileged container or global binfmt registration needed.
# Child-mode flags run selected test bodies directly under QEMU, avoiding native
# re-execution of foreign-architecture binaries by subprocess wrappers.
set -euo pipefail
cd "$(dirname "$0")/.."
repo_root="$PWD"
image_name="${GCR_BIG_ENDIAN_IMAGE:-gcrust-big-endian-runtime:1.96.1}"
docker build -t "$image_name" - < scripts/big_endian_runtime.Dockerfile
mkdir -p target/big-endian-runtime
python3 scripts/prepare_runtime_vendor.py
docker run --rm --entrypoint /bin/sh \
  -v "$repo_root/crates/gcrust-rt:/source:ro" \
  -v "$repo_root/target/big-endian-runtime:/target" \
  -v "$repo_root/target/big-endian-runtime/vendor:/vendor:ro" \
  -v "$repo_root/crates/vendor/hashbrown:/hashbrown:ro" \
  "$image_name" -ec '
    mkdir -p /work/runtime
    cp -a /source/. /work/runtime/
    cd /work/runtime
    mkdir -p .cargo
    cp /target/vendor-config.toml .cargo/config.toml
    rustc -Vv
    s390x-linux-gnu-gcc --version
    qemu-s390x --version
    export CARGO_TARGET_S390X_UNKNOWN_LINUX_GNU_LINKER=s390x-linux-gnu-gcc
    export CARGO_TARGET_S390X_UNKNOWN_LINUX_GNU_RUNNER="qemu-s390x -L /usr/s390x-linux-gnu"
    for profile in debug release; do
      for workers in 1 4; do
        echo "Big-endian runtime: profile=$profile workers=$workers"
        for filter in collector_satb_buffer_flush_preserves_values_and_local_storage gc::graph_model_tests:: collector_default_ collector_varlen_ collector_validated_geometry collector_canonical_requests collector_string_range_ collector_string_copy_bounds collector_string_scan_matches_reference collector_string_owned_range_and_error collector_float_parse_reference collector_ffi_copy_reference collector_ffi_buffer_reference collector_channel_buffer_reference collector_channel_scan_reference collector_native_retirement_reference collector_registry_admission_reference collector_registry_compaction_reference collector_metadata_allocator_reference collector_native_metadata_reference collector_native_pressure_registry_ collector_native_pressure_spawn_ collector_native_pressure_combines_ collector_native_pressure_execution_ collector_native_pressure_live_ collector_native_pressure_long_ collector_access_stripes_reference_ collector_aggregate_read_ collector_aggregate_write_ collector_aggregate_availability_ collector_lock_safe_ collector_runtime_thread_storage_ collector_abi_storage_ collector_exported_window_ collector_scoped_registry_reference collector_registry_teardown_reference collector_permanent_registry_reference collector_native_activity_reference collector_atomic_retirement_reference collector_completion_retirement_reference collector_channel_retirement_reference collector_channel_sync_reference collector_channel_registry_reference collector_atomic_registry_reference_ collector_completion_joined_owner_reclaims_while_native_observer_holds_state collector_completion_registry_reference_ collector_reaper_batch_reference collector_reaper_catalogue_reference collector_handoff_root_reference collector_handoff_storage_reference collector_caller_parking_reference collector_channel_groups_reference collector_channel_remembered_reference collector_root_slots_ collector_resolved_retries_ collector_worker_scratch_ collector_pending_copies_ collector_parallel_worker_policy_; do
        if [ "$profile" = release ]; then
          GCR_LOCK_SAFE_SCAN_CHILD=1 GCR_LOCK_SAFE_SIBLING_CHILD=1 GCR_GRAPH_ORACLE_CHILD=1 GCR_CHANNEL_REMEMBERED_CHILD=1 GCR_OBJECT_ACCESS_CHILD=1 GCR_AGGREGATE_RELOAD_CHILD=1 GCR_GC_WORKERS="$workers" GCR_GC_VERIFY=1 GCRUST_POISON=1 \
            timeout 180s cargo test --offline --release --target s390x-unknown-linux-gnu \
            --target-dir /target --lib "$filter" -- --test-threads=1
        else
          GCR_LOCK_SAFE_SCAN_CHILD=1 GCR_LOCK_SAFE_SIBLING_CHILD=1 GCR_GRAPH_ORACLE_CHILD=1 GCR_CHANNEL_REMEMBERED_CHILD=1 GCR_OBJECT_ACCESS_CHILD=1 GCR_AGGREGATE_RELOAD_CHILD=1 GCR_GC_WORKERS="$workers" GCR_GC_VERIFY=1 GCRUST_POISON=1 \
            timeout 180s cargo test --offline --target s390x-unknown-linux-gnu \
            --target-dir /target --lib "$filter" -- --test-threads=1
        fi
        done
      done
    done
  '
