//! Queue edges proportional to occupancy, without taking a waiter-held mutex.
use super::native_memory::NativeSlots;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub(super) struct QueueRange {
    head: AtomicUsize,
    count: AtomicUsize,
    young: AtomicBool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
        runtime::*,
    };
    use std::collections::VecDeque;

    #[test]
    fn collector_channel_scan_reference_ring_ranges() {
        for capacity in [1, 3, 64, 8193] {
            let slots = NativeSlots::new(
                (0..capacity)
                    .map(|i| std::sync::atomic::AtomicU64::new(i as u64 + 1))
                    .collect(),
            );
            // Test every wrap/full/empty shape for small rings, boundary heads
            // for large rings. Check the exact FIFO visitation order, including empty
            // and full rings and both segments of wrapped ranges.
            let heads: Vec<_> = if capacity <= 64 {
                (0..capacity).collect()
            } else {
                vec![0, 1, capacity / 2, capacity - 1]
            };
            for head in heads {
                for count in [0, 1, capacity / 2, capacity - 1, capacity] {
                    let range = QueueRange::populated(head, count);
                    let mut indices = Vec::new();
                    unsafe {
                        range.visit(&slots, &mut |p| {
                            let index = p.offset_from(slots[0].as_ptr()) as usize;
                            indices.push(index);
                        })
                    };
                    assert_eq!(
                        indices,
                        (0..count)
                            .map(|i| (head + i) % capacity)
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
    }

    unsafe fn check(heap: &crate::gc::Heap, ctrl: *const ChanCtrl, expected: &VecDeque<u64>) {
        let _pause = heap.pause_world();
        let c = unsafe { &*ctrl };
        // A parked waiter can own this mutex. Scanning must not acquire it.
        let _state = c.state.lock().unwrap();
        let mut actual = Vec::new();
        unsafe {
            c.scan_roots(&mut |slot| {
                let value = slot.read() as *const u8;
                assert!(!value.is_null(), "scanner visited an unoccupied cell");
                actual.push(value.add(Full::SIZE).cast::<u64>().read());
            })
        };
        assert_eq!(actual, expected.iter().copied().collect::<Vec<_>>());
    }

    #[test]
    fn collector_channel_scan_tracks_operations_and_moving_values() {
        if std::env::var_os("GCR_CHANNEL_SCAN_CHILD").is_none() {
            use std::{
                process::{Command, Stdio},
                time::{Duration, Instant},
            };
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime::channel_scan::tests::collector_channel_scan_tracks_operations_and_moving_values", "--nocapture"])
                .env("GCR_CHANNEL_SCAN_CHILD", "1")
                .env("GCR_GC_VERIFY", "1").env("GCRUST_POISON", "1")
                .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
            let until = Instant::now() + Duration::from_secs(30);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= until {
                    child.kill().unwrap();
                    panic!(
                        "channel scan stalled: {:?}",
                        child.wait_with_output().unwrap()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "{out:?}");
            return;
        }
        for generational in [false, true] {
            let types = vec![
                TypeInfo::for_header(Full::SIZE).with_varlen_values(0),
                TypeInfo::for_header(Full::SIZE)
                    .with_type_id(1)
                    .with_raw_bytes(8),
            ];
            let mut rt = if generational {
                RuntimeContext::new_generational(32768, 2 << 20, types)
            } else {
                RuntimeContext::new(2 << 20, types)
            };
            let heap = rt.heap().clone();
            rt.enter(|thread| unsafe {
                let dyna = &*(*thread).dyna_thread;
                let roots = dyna.scratch_scope();
                let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
                for capacity in [1, 3, 64, 8193] {
                    let ctrl = ai_chan_new(thread, capacity as i64, roots.get(witness), 1);
                    let mut expected = VecDeque::new();
                    let mut serial = 0;
                    let send = |serial: &mut u64, expected: &mut VecDeque<u64>| {
                        *serial += 1;
                        let value = ai_gc_alloc_fixed(thread, 1, 0);
                        value.add(Full::SIZE).cast::<u64>().write(*serial);
                        assert_eq!(ai_chan_send(thread, roots.get(witness), ctrl, value), 0);
                        expected.push_back(*serial);
                    };
                    let recv = |expected: &mut VecDeque<u64>| {
                        let value = ai_chan_recv(thread, roots.get(witness), ctrl);
                        assert!(!value.is_null());
                        assert_eq!(
                            value.add(Full::SIZE).cast::<u64>().read(),
                            expected.pop_front().unwrap()
                        );
                    };
                    check(&heap, ctrl, &expected);
                    // Force head wrap even for sparse large queues.
                    for _ in 0..capacity + 5 {
                        send(&mut serial, &mut expected);
                        recv(&mut expected);
                    }
                    for _ in 0..capacity {
                        send(&mut serial, &mut expected);
                    }
                    check(&heap, ctrl, &expected);
                    for _ in 0..capacity / 2 {
                        recv(&mut expected);
                    }
                    check(&heap, ctrl, &expected);
                    for _ in 0..capacity / 2 {
                        send(&mut serial, &mut expected);
                    }
                    // Queue is the only root for these messages.
                    heap.mutator_triggered_gc::<IdentityPtrPolicy>(dyna);
                    check(&heap, ctrl, &expected);
                    ai_chan_close(thread, ctrl);
                    while !expected.is_empty() {
                        recv(&mut expected);
                        if expected.len() % 113 == 0 {
                            check(&heap, ctrl, &expected);
                        }
                    }
                    check(&heap, ctrl, &expected);
                    assert!(ai_chan_recv(thread, roots.get(witness), ctrl).is_null());
                }
            });
        }
    }
}
impl QueueRange {
    pub(super) fn empty() -> Self {
        Self {
            head: AtomicUsize::new(0),
            count: AtomicUsize::new(0),
            young: AtomicBool::new(false),
        }
    }
    #[cfg(test)]
    pub(super) fn populated(head: usize, count: usize) -> Self {
        Self {
            head: AtomicUsize::new(head),
            count: AtomicUsize::new(count),
            // Synthetic populated queues have not passed through the send
            // barrier. Conservatively scan them until a minor completes.
            young: AtomicBool::new(count != 0),
        }
    }
    /// Under the queue-state mutex, after slot/head/count changes, before any
    /// safepoint, blocking transition or mutex release. STW publication of the
    /// mutator's stopped state makes both fields and slots visible together.
    /// These atomics are not an independently consistent concurrent snapshot.
    pub(super) fn publish(&self, head: usize, count: usize) {
        self.head.store(head, Ordering::Release);
        self.count.store(count, Ordering::Release);
    }
    pub(super) fn remember_young(&self) {
        self.young.store(true, Ordering::Release);
    }
    pub(super) fn needs_minor_scan(&self) -> bool {
        self.young.load(Ordering::Acquire)
    }
    /// # Safety
    /// World stopped, after all minor-copy jobs and weak-owner retirement.
    /// Every surviving owned queue's direct references now point to old space.
    pub(super) unsafe fn minor_completed(&self) {
        self.young.store(false, Ordering::Release);
    }
    /// World stopped; slot mutations and publish cannot be in flight. Parallel
    /// collector readers may visit disjoint channels, never mutator state.
    pub(super) unsafe fn visit(&self, slots: &NativeSlots, visit: &mut dyn FnMut(*mut u64)) {
        let head = self.head.load(Ordering::Acquire);
        let count = self.count.load(Ordering::Acquire);
        let first = count.min(slots.len() - head);
        for slot in &slots[head..head + first] {
            visit(slot.as_ptr());
        }
        for slot in &slots[..count - first] {
            visit(slot.as_ptr());
        }
    }
}
