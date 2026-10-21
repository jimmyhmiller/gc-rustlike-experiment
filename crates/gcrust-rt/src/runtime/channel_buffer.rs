//! Mutator-side channel backing retirement outside queue/root locks.
use super::{Thread, blocking_region, native_memory::NativeSlots, string_copy};
pub(super) unsafe fn retire(thread: *mut Thread, slots: Option<NativeSlots>) {
    let Some(slots) = slots else {
        return;
    };
    if slots.len() * 8 <= string_copy::COPY_BYTES {
        slots.release();
        return;
    }
    #[cfg(test)]
    let heap_id = unsafe { (*thread).heap } as usize;
    let release = move || {
        #[cfg(test)]
        tests::checkpoint(heap_id, true);
        slots.release();
    };
    unsafe { blocking_region(thread, release) }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
        runtime::*,
    };
    use std::{
        process::{Command, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc::Sender,
        },
        time::{Duration, Instant},
    };
    struct Hook {
        heap: usize,
        retiring: bool,
        ready: Sender<()>,
        completed: Arc<AtomicBool>,
    }
    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
    pub(crate) fn checkpoint(heap: usize, retiring: bool) {
        let hook = {
            let mut slot = HOOK.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|h| h.heap == heap && h.retiring == retiring)
            {
                slot.take()
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            hook.ready.send(()).unwrap();
            while !hook.completed.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }
    pub(in crate::runtime) fn child(name: &str) -> bool {
        if std::env::var_os("GCR_CHANNEL_BUFFER_CHILD").is_some() {
            return true;
        }
        for mode in ["0", "1"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env("GCR_CHANNEL_BUFFER_CHILD", mode)
                .env("GCR_GC_VERIFY", "1")
                .env("GCRUST_POISON", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let until = Instant::now() + Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= until {
                    child.kill().unwrap();
                    panic!(
                        "channel native phase stalled GC: {:?}",
                        child.wait_with_output().unwrap()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        false
    }
    fn runtime_mode(generational: bool) -> RuntimeContext {
        let types = vec![
            TypeInfo::for_header(Full::SIZE).with_varlen_values(0),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_raw_bytes(8),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(2)
                .with_raw_bytes(8),
        ];
        if generational {
            RuntimeContext::new_generational(32768, 2 << 20, types)
        } else {
            RuntimeContext::new(2 << 20, types)
        }
    }
    fn runtime() -> RuntimeContext {
        runtime_mode(std::env::var("GCR_CHANNEL_BUFFER_CHILD").as_deref() == Ok("1"))
    }
    #[test]
    fn collector_channel_buffer_reference_zeroing_and_retirement() {
        for generational in [false, true] {
            let mut rt = runtime_mode(generational);
            let heap = rt.heap().clone();
            rt.enter(|thread| unsafe {
                {
                    let roots = (&*(*thread).dyna_thread).scratch_scope();
                    let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
                    for cap in [1, 4096, 4097, 8193, 131072] {
                        let control = ai_chan_new(thread, cap, roots.get(witness), 1);
                        assert_eq!(heap.native_buffer_bytes(), cap as usize * 8);
                        {
                            let slots = (*control).slots.read().unwrap();
                            let slots = slots.as_ref().unwrap();
                            assert_eq!(slots.len(), cap as usize);
                            assert!(slots.iter().all(|slot| slot.load(Ordering::SeqCst) == 0));
                        }
                        ai_chan_close(thread, control);
                        assert_eq!(heap.native_buffer_bytes(), 0);
                        ai_chan_close(thread, control);
                        assert_eq!(heap.native_buffer_bytes(), 0);
                    }
                }
                assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
            });
        }
    }
    #[test]
    fn collector_channel_buffer_construction_allows_collection_and_charges_reservation() {
        if !child(
            "runtime::channel_buffer::tests::collector_channel_buffer_construction_allows_collection_and_charges_reservation",
        ) {
            return;
        }
        construction_case(8193);
    }
    #[test]
    fn collector_channel_sync_small_construction_allows_collection() {
        if child(
            "runtime::channel_buffer::tests::collector_channel_sync_small_construction_allows_collection",
        ) {
            construction_case(1);
        }
    }
    fn construction_case(capacity: usize) {
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let witness = ai_gc_alloc_varlen(thread, 0, 0, 0);
            let (ready, waiting) = std::sync::mpsc::channel();
            let completed = Arc::new(AtomicBool::new(false));
            *HOOK.lock().unwrap() = Some(Hook {
                heap: Arc::as_ptr(&heap) as usize,
                retiring: false,
                ready,
                completed: completed.clone(),
            });
            let other = heap.clone();
            let done = completed.clone();
            let collector = std::thread::spawn(move || {
                waiting.recv().unwrap();
                other.stw_collect::<IdentityPtrPolicy>();
                let charged = other.native_buffer_bytes();
                let unpublished = other.channels.lock().unwrap().records.is_empty();
                done.store(true, Ordering::Release);
                assert_eq!(charged, capacity * 8);
                assert!(unpublished, "partially initialized queue was published");
            });
            let ctrl = ai_chan_new(thread, capacity as i64, witness, 1);
            assert!(completed.load(Ordering::Acquire));
            blocking_region(thread, || collector.join().unwrap());
            assert_eq!((*ctrl).array_type, 0);
            assert!(
                (*ctrl)
                    .slots
                    .read()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .iter()
                    .all(|cell| cell.load(Ordering::SeqCst) == 0)
            );
            assert_eq!(
                heap.native_buffer_bytes(),
                capacity * 8,
                "registration charged twice"
            );
            ai_chan_close(thread, ctrl);
            assert_eq!(heap.native_buffer_bytes(), 0);
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
    fn retire_case(kind: u8) {
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
            let ctrl = ai_chan_new(thread, 8193, roots.get(witness), 1);
            let owner = ai_gc_alloc_fixed(thread, 2, 0);
            ai_chan_bind_owner(thread, ctrl, owner);
            if kind == 2 {
                let message = ai_gc_alloc_fixed(thread, 1, 0);
                message.add(Full::SIZE).cast::<u64>().write(42);
                assert_eq!(ai_chan_send(thread, roots.get(witness), ctrl, message), 0);
                ai_chan_close(thread, ctrl);
            }
            // No caller root for the owner: with_channel must pin it at entry.
            let (ready, waiting) = std::sync::mpsc::channel();
            let completed = Arc::new(AtomicBool::new(false));
            *HOOK.lock().unwrap() = Some(Hook {
                heap: Arc::as_ptr(&heap) as usize,
                retiring: true,
                ready,
                completed: completed.clone(),
            });
            let other = heap.clone();
            let done = completed.clone();
            let key = ctrl as usize;
            let collector = std::thread::spawn(move || {
                waiting.recv().unwrap();
                let record = other.channels.lock().unwrap().records[&key].0.clone();
                let state_unlocked = record.state.try_lock().is_ok();
                let slots_unlocked = record.slots.try_read().is_ok();
                other.stw_collect::<IdentityPtrPolicy>();
                let charged = other.native_buffer_bytes();
                let owner_kept = other.channels.lock().unwrap().records.contains_key(&key);
                done.store(true, Ordering::Release);
                assert!(state_unlocked && slots_unlocked, "parked with a queue lock");
                assert!(owner_kept);
                assert_eq!(charged, 8193 * 8);
            });
            match kind {
                0 => ai_chan_close(thread, ctrl),
                1 => ai_chan_sender_drop(thread, ctrl),
                _ => {
                    let message = ai_chan_recv(thread, roots.get(witness), ctrl);
                    assert_eq!(
                        message.add(Full::SIZE).cast::<u64>().read(),
                        42,
                        "popped value lost during native cleanup"
                    );
                }
            }
            assert!(completed.load(Ordering::Acquire));
            blocking_region(thread, || collector.join().unwrap());
            assert_eq!(heap.native_buffer_bytes(), 0);
        });
    }
    #[test]
    fn collector_channel_buffer_close_allows_collection() {
        if child("runtime::channel_buffer::tests::collector_channel_buffer_close_allows_collection")
        {
            retire_case(0);
        }
    }
    #[test]
    fn collector_channel_buffer_sender_drop_allows_collection() {
        if child(
            "runtime::channel_buffer::tests::collector_channel_buffer_sender_drop_allows_collection",
        ) {
            retire_case(1);
        }
    }
    #[test]
    fn collector_channel_buffer_last_receive_roots_value_during_cleanup() {
        if child(
            "runtime::channel_buffer::tests::collector_channel_buffer_last_receive_roots_value_during_cleanup",
        ) {
            retire_case(2);
        }
    }
}
