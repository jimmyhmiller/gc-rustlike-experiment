//! Prepare native synchronization before publishing a channel to mutators/GC.
use super::{
    ChanCtrl, ChanState, Condvar, Mutex, RwLock, channel_scan,
    native_memory::{Charge, NativeSlots},
    native_retirement::Retirement,
};
use std::sync::{Arc, atomic::AtomicUsize};

/// Native-only constructor. Caller is unregistered or BLOCKED, capacity is
/// positive and representable, and the charge reserves exactly capacity * 8.
/// No managed address enters this function.
pub(super) fn construct(
    capacity: usize,
    array_type: u16,
    message_type: u16,
    charge: Charge,
    retirement: &Retirement,
) -> Arc<ChanCtrl> {
    let control = Arc::new(ChanCtrl {
        retirement: super::native_retirement::RecordLink::new(retirement),
        owner: AtomicUsize::new(1),
        state: Mutex::new(ChanState {
            head: 0,
            tail: 0,
            count: 0,
            closed: false,
            senders: 1,
            waiting_senders: 0,
            waiting_receivers: 0,
        }),
        scan: channel_scan::QueueRange::empty(),
        not_empty: Condvar::new(),
        not_full: Condvar::new(),
        slots: RwLock::new(Some(NativeSlots::zeroed_reserved(
            capacity, charge, retirement,
        ))),
        cap: capacity,
        array_type,
        message_type,
    });
    prepare(&control);
    control
}

/// Called inside the native BLOCKED constructor. Notification can initialize
/// pthread storage even without a waiter. Prime both lock modes too: register
/// and stopped-world root scans must not initialize the backing lock later.
pub(super) fn prepare(control: &ChanCtrl) {
    drop(control.state.lock().unwrap());
    drop(control.slots.write().unwrap());
    drop(control.slots.read().unwrap());
    control.not_empty.notify_all();
    control.not_full.notify_all();
}

#[cfg(test)]
mod tests {
    use crate::{
        gc::{Full, ObjHeader, TypeInfo},
        runtime::*,
    };
    #[test]
    fn collector_channel_sync_reference_constructor_prepares_all_primitives() {
        for capacity in [1, 4097] {
            let memory = std::sync::Arc::new(native_memory::NativeMemory::default());
            let retirement = native_retirement::Retirement::default();
            // Unregistered native construction; no registry has touched either
            // lock or notification object before the measured first access.
            let control =
                super::construct(capacity, 0, 1, memory.charge(capacity * 8), &retirement);
            allocation_probe::start();
            drop(control.state.lock().unwrap());
            drop(control.slots.read().unwrap());
            drop(control.slots.write().unwrap());
            control.not_empty.notify_one();
            control.not_empty.notify_all();
            control.not_full.notify_one();
            control.not_full.notify_all();
            let counts = allocation_probe::finish();
            assert_eq!(counts, (0, 0));
            drop(control);
            retirement.shutdown();
            assert_eq!(memory.bytes(), 0);
        }
    }
    #[test]
    fn collector_channel_sync_reference_operations_do_not_allocate() {
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
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
                let message = roots.push(ai_gc_alloc_fixed(thread, 1, 0));
                roots.get(message).add(Full::SIZE).cast::<u64>().write(42);
                for capacity in [1, 4097] {
                    let control = ai_chan_new(thread, capacity, roots.get(witness), 1);
                    let before = heap.collections();
                    allocation_probe::start();
                    ai_chan_bind_owner(thread, control, roots.get(message));
                    let send =
                        ai_chan_send(thread, roots.get(witness), control, roots.get(message));
                    let received = ai_chan_recv(thread, roots.get(witness), control);
                    ai_chan_sender_clone(thread, control);
                    ai_chan_sender_drop(thread, control);
                    let senders = ai_chan_waiting_senders(thread, control);
                    let receivers = ai_chan_waiting_receivers(thread, control);
                    let counts = allocation_probe::finish();
                    assert_eq!(counts, (0, 0));
                    assert_eq!(send, 0);
                    assert_eq!(received, roots.get(message));
                    assert_eq!((senders, receivers), (0, 0));
                    assert_eq!(heap.collections(), before);
                    allocation_probe::start();
                    ai_chan_close(thread, control);
                    let (allocations, frees) = allocation_probe::finish();
                    assert_eq!(allocations, 0);
                    assert!(frees > 0);
                    assert_eq!(heap.native_buffer_bytes(), 0);
                    ai_chan_close(thread, control);
                }
            });
        }
    }
}

// SAFETY: the controller has one private link, and its registry removes it once.
// Native destruction touches no managed objects; native observers never use links.
unsafe impl super::native_retirement::Record for ChanCtrl {
    fn retirement(&self) -> &super::native_retirement::RecordLink<Self> { &self.retirement }
    fn mailbox(state: &mut super::native_retirement::State) -> &mut super::native_retirement::RecordQueue<Self> { &mut state.channels }
}
