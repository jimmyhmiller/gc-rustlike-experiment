use super::*;
use crate::gc::Full;

fn check_retired_profile_backing(generational: bool) {
        let heap = if generational {
            Heap::new_generational::<Full>(4096, 16384, vec![])
        } else {
            Heap::new::<Full>(16384, vec![])
        };
        let counter = heap.native_memory.registry.clone();
        let (state, _) = heap.register_thread();
        unsafe {
            state.record_alloc(4095, 7);
            heap.safe_deregister_thread(&state);
        }
        drop(state);
        let requested = {
            let retired = heap.retired_alloc_counters.lock().unwrap();
            assert_eq!(retired[4095].count, 1);
            assert_eq!(retired[4095].bytes, 7);
            retired.capacity() * std::mem::size_of::<SiteCounter>()
        };
        let retained = counter.bytes();
        let heap_backing = heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes();
        let report = heap.alloc_site_profile();
        let row = report.iter().find(|row| row.site_id == 4095).unwrap();
        assert_eq!(row.count, 1);
        assert_eq!(row.bytes, 7);
        drop(heap);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(row.count, 1, "standalone report must survive its source heap");
        assert_eq!(retained, requested + heap_backing, "heap-retired profile backing is outside the ledger");
}

#[test]
fn collector_retired_profile_backing_semispace() { check_retired_profile_backing(false); }
#[test]
fn collector_retired_profile_backing_generational() { check_retired_profile_backing(true); }
