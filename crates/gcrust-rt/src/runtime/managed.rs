//! SC linearization for mutable inline aggregate slots.
//!
//! Scalar/reference slots use native SC atomics. Aggregate slots are accessed
//! under one address stripe; unrelated objects can proceed independently. An SC
//! RMW at unlock is the aggregate access's linearization event in the same system
//! order as scalar atomics. No safepoint or allocation is allowed while holding
//! a stripe. Waiters publish their roots and park; relocation changes the stripe
//! address, so acquisition revalidates after resumption and retries as necessary.
use super::{Thread, blocking_region};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, TryLockError};

const STRIPES: usize = 64;

struct Stripe {
    held: Mutex<bool>,
    available: Condvar,
}

pub(crate) struct ManagedAccess {
    stripes: [Stripe; STRIPES],
    order: AtomicU64,
    #[cfg(test)]
    completed_waits: AtomicU64,
    #[cfg(test)]
    parked_waits: AtomicU64,
}

impl ManagedAccess {
    pub(crate) fn new() -> Self {
        Self {
            stripes: std::array::from_fn(|_| Stripe {
                held: Mutex::new(false),
                available: Condvar::new(),
            }),
            order: AtomicU64::new(0),
            #[cfg(test)]
            completed_waits: AtomicU64::new(0),
            #[cfg(test)]
            parked_waits: AtomicU64::new(0),
        }
    }

    fn index(object: *const u8) -> usize {
        // Mix allocation alignment bits so adjacent, similarly sized objects
        // spread across stripes; identity is revalidated after any GC pause.
        let address = object as usize >> 4;
        (address ^ (address >> 6)) & (STRIPES - 1)
    }

    fn try_acquire(&self, index: usize) -> bool {
        let mut held = match self.stripes[index].held.try_lock() {
            Ok(held) => held,
            Err(TryLockError::WouldBlock) => return false,
            Err(TryLockError::Poisoned(error)) => panic!("{error}"),
        };
        if *held {
            return false;
        }
        *held = true;
        true
    }

    fn wait_available(&self, index: usize) {
        let stripe = &self.stripes[index];
        let mut held = stripe.held.lock().unwrap();
        while *held {
            #[cfg(test)]
            self.parked_waits.fetch_add(1, Ordering::Release);
            held = stripe.available.wait(held).unwrap();
            #[cfg(test)]
            self.parked_waits.fetch_sub(1, Ordering::Release);
        }
        // Availability is a wakeup hint, not ownership. The blocked caller
        // must resume and reload its receiver before claiming any stripe.
        drop(held);
        #[cfg(test)]
        self.completed_waits.fetch_add(1, Ordering::Release);
    }

    fn release(&self, index: usize, linearize: bool) {
        if linearize {
            self.order.fetch_add(1, Ordering::SeqCst);
        }
        let stripe = &self.stripes[index];
        let mut held = stripe.held.lock().unwrap();
        assert!(*held, "managed aggregate stripe was not held");
        *held = false;
        // Waiters do not take ownership before resuming. Relocation can make
        // their next acquisition target another stripe, so no subsequent
        // release on this stripe is guaranteed to wake a remaining waiter.
        stripe.available.notify_all();
    }
}

/// Acquire an aggregate object's stripe and return its post-relocation address.
///
/// # Safety
/// The caller is the owning running mutator, with a valid rooted object. It must
/// perform only the snapshot load/store and barrier before ai_managed_unlock:
/// no allocation, safepoint, foreign call, suspension or nested acquisition.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_managed_lock(thread: *mut Thread, object: *mut u8) -> *mut u8 {
    unsafe {
        assert!(
            (*thread).managed_lock.is_none(),
            "nested managed aggregate access"
        );
        let heap = &*(*thread).heap;
        // This path cannot park or poll. The owning mutator remains RUNNING,
        // so copying cannot begin until it reaches a later safepoint, even if
        // the collector requests its initial pause just after this phase read.
        // During copying, use the rooted blocking path below: it waits for the
        // phase to end and revalidates the object's relocated stripe.
        let index = ManagedAccess::index(object);
        if !heap.barriers_active() && heap.managed_access.try_acquire(index) {
            (*thread).managed_lock = Some(index);
            return object;
        }
        let dyna = &*(*thread).dyna_thread;
        let roots = dyna.scratch_scope();
        let slot = roots.push(object);
        loop {
            let before = roots.get(slot);
            let index = ManagedAccess::index(before);
            blocking_region(thread, || heap.managed_access.wait_available(index));
            let current = roots.get(slot);
            let current_index = ManagedAccess::index(current);
            if heap.managed_access.try_acquire(current_index) {
                (*thread).managed_lock = Some(current_index);
                return current;
            }
            // Another mutator won the wakeup, or relocation changed the
            // stripe. Park without ownership and retry after the next wakeup.
        }
    }
}

/// Read one immutable snapshot of a mutable inline aggregate.
///
/// # Safety
/// As `ai_managed_lock`. The byte range identifies one initialized aggregate
/// payload outside the header, and `destination` is disjoint writable native
/// storage of `bytes` bytes. No managed/interior pointer to the source escapes.
/// May park on stripe contention; caller must publish roots and reload derived
/// pointers afterward. Destination references need rooting only after return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_managed_read(
    thread: *mut Thread,
    object: *mut u8,
    offset: usize,
    destination: *mut u8,
    bytes: usize,
) {
    unsafe {
        assert!((*thread).managed_lock.is_none(), "nested managed aggregate access");
        let heap = &*(*thread).heap;
        let index = ManagedAccess::index(object);
        let object = if heap.managed_access.try_acquire(index) {
            (*thread).managed_lock = Some(index);
            object
        } else {
            ai_managed_lock(thread, object)
        };
        // The aggregate stripe serializes mutators. Canonical access also
        // excludes copying; source/destination aliases share its copy locks.
        // Acquire in this order everywhere, never park with ObjectAccess held.
        {
            let access = heap.object_access(object);
            std::ptr::copy_nonoverlapping(access.as_ptr().add(offset), destination, bytes);
            // Different old/new aggregate stripes still share canonical access.
            // Linearize before releasing that common exclusion.
            ai_managed_unlock(thread);
        }
    }
}

/// Try a complete inline aggregate replacement without a safepoint.
///
/// # Safety
/// Owning RUNNING mutator; object is a live rooted base. The range is one valid
/// aggregate payload outside the header. Source is disjoint initialized native
/// storage. References lists every traced slot as sorted, unique relative u16
/// byte offsets, aligned in the managed object and fully within the range.
/// Source references are valid published bases or null. No nested access/stripe.
/// Returns 0 without writes if contended, or 1 after the complete replacement.
/// Caller must use a rooted/reloading fallback after a failed attempt.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_managed_try_write(
    thread: *mut Thread, object: *mut u8, offset: usize, source: *const u8,
    bytes: usize, references: *const u16, reference_count: usize,
) -> u8 {
    unsafe {
        assert!((*thread).managed_lock.is_none(), "nested managed aggregate access");
        let heap = &*(*thread).heap;
        let index = ManagedAccess::index(object);
        if !heap.managed_access.try_acquire(index) { return 0; }
        (*thread).managed_lock = Some(index);
        let access = heap.object_access(object);
        let destination = access.as_ptr().add(offset);
        let mut cursor = 0;
        for index in 0..reference_count {
            let at = references.add(index).read() as usize;
            assert!(at >= cursor && at.checked_add(8).is_some_and(|end| end <= bytes), "invalid aggregate reference offsets");
            let slot = &*destination.add(at).cast::<AtomicU64>();
            if heap.barriers_active() {
                (&*(*thread).dyna_thread).log_satb(heap, slot.load(Ordering::SeqCst));
            }
            // Keep traced words atomic, copying only untraced gaps as bytes.
            if at != cursor {
                std::ptr::copy_nonoverlapping(source.add(cursor), destination.add(cursor), at - cursor);
            }
            let bits = source.add(at).cast::<u64>().read_unaligned();
            slot.store(bits, Ordering::SeqCst);
            if heap.has_nursery() && heap.is_nursery(bits as *const u8) {
                heap.mark_card_dirty(destination.add(at));
            }
            cursor = at + 8;
        }
        if bytes != cursor {
            std::ptr::copy_nonoverlapping(source.add(cursor), destination.add(cursor), bytes - cursor);
        }
        // Keep canonical exclusion through the SC event, including when aliases
        // selected different aggregate stripes before resolution.
        ai_managed_unlock(thread);
        drop(access);
        1
    }
}

/// Linearize and release the current aggregate snapshot access.
///
/// # Safety
/// Must be paired on the same mutator with ai_managed_lock. All accesses to the
/// protected slot (including its generational barrier) must already be complete.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_managed_unlock(thread: *mut Thread) {
    unsafe {
        let index = (*thread)
            .managed_lock
            .take()
            .expect("unpaired managed aggregate unlock");
        (&*(*thread).heap).managed_access.release(index, true);
    }
}

/// Copy the FFI buffer one indivisible SC array element at a time. The native
/// side is private/unshared and may be unaligned. The managed side has natural
/// element alignment. Whole arrays are not an indivisible snapshot.
///
/// # Safety
/// Both buffers span byte_len initialized bytes; managed has natural alignment
/// for bits (1/8/16/32/64). Caller retains canonical object access for the whole
/// copy and remains RUNNING without safepoints or managed blocking. Normal
/// per-element atomic synchronization is required among mutators.
pub(super) unsafe fn copy_scalar_buffer(
    managed: *mut u8,
    native: *mut u8,
    byte_len: i64,
    bits: i64,
    copy_out: bool,
) {
    let stride = match bits {
        1 | 8 => 1,
        16 => 2,
        32 => 4,
        64 => 8,
        _ => panic!("invalid scalar buffer width"),
    };
    assert!(
        byte_len >= 0 && byte_len % stride == 0,
        "invalid scalar buffer size"
    );
    macro_rules! copy {
        ($integer:ty, $atomic:ty) => {{
            for offset in (0..byte_len as usize).step_by(stride as usize) {
                unsafe {
                    let cell = &*managed.add(offset).cast::<$atomic>();
                    let raw = native.add(offset).cast::<$integer>();
                    if copy_out {
                        let mut value = core::ptr::read_unaligned(raw);
                        if bits == 1 {
                            value &= 1;
                        }
                        cell.store(value, Ordering::SeqCst);
                    } else {
                        let mut value = cell.load(Ordering::SeqCst);
                        if bits == 1 {
                            value &= 1;
                        }
                        core::ptr::write_unaligned(raw, value);
                    }
                }
            }
        }};
    }
    match bits {
        1 | 8 => copy!(u8, std::sync::atomic::AtomicU8),
        16 => copy!(u16, std::sync::atomic::AtomicU16),
        32 => copy!(u32, std::sync::atomic::AtomicU32),
        64 => copy!(u64, AtomicU64),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{Full, ObjHeader, TypeInfo};
    use crate::runtime::RuntimeContext;
    use std::hint::black_box;
    use std::time::Instant;

    #[test]
    fn collector_aggregate_availability_wakes_all_waiters_without_ownership() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        let access = Arc::new(ManagedAccess::new());
        let index = 0;
        assert!(access.try_acquire(index));
        let (sent, received) = mpsc::channel();
        let workers: Vec<_> = (0..4).map(|_| {
            let access = access.clone();
            let sent = sent.clone();
            std::thread::spawn(move || {
                access.wait_available(index);
                sent.send(()).unwrap();
            })
        }).collect();
        let deadline = Instant::now() + Duration::from_secs(5);
        while access.parked_waits.load(Ordering::Acquire) != 4 {
            assert!(Instant::now() < deadline, "waiters did not park");
            std::thread::yield_now();
        }
        access.release(index, false);
        let mut awakened = 0;
        while awakened < 4 && received.recv_timeout(Duration::from_secs(2)).is_ok() {
            awakened += 1;
        }
        // Unstick every worker before asserting a negative-control failure.
        access.stripes[index].available.notify_all();
        for worker in workers { worker.join().unwrap(); }
        assert_eq!(awakened, 4, "availability stranded waiters after release");
        assert!(access.try_acquire(index), "availability waiter retained ownership");
        access.release(index, false);
    }

    #[test]
    fn collector_aggregate_write_contention_has_no_payload_or_order_side_effects() {
        let info = TypeInfo::for_header_with_interior_ptrs(Full::SIZE, &[24]).with_raw_bytes(24);
        let mut runtime = RuntimeContext::new(65536, vec![info.clone()]);
        let heap = runtime.heap().clone();
        runtime.enter(|thread| unsafe {
            let object = heap.alloc_obj::<Full>(&info, 0);
            let original = [11u64, 0, 22];
            std::ptr::copy_nonoverlapping(original.as_ptr().cast::<u8>(), object.add(Full::SIZE), 24);
            let root = heap.globals.add(object as u64);
            let index = ManagedAccess::index(object);
            assert!(heap.managed_access.try_acquire(index));
            let order = heap.managed_access.order.load(Ordering::SeqCst);
            let input = [33u64, 0, 44];
            let references = [8u16];
            crate::runtime::allocation_probe::start();
            let result = ai_managed_try_write(thread, object, Full::SIZE, input.as_ptr().cast(), 24, references.as_ptr(), 1);
            let traffic = crate::runtime::allocation_probe::finish();
            let observed = object.add(Full::SIZE).cast::<[u64; 3]>().read();
            let after = heap.managed_access.order.load(Ordering::SeqCst);
            heap.managed_access.release(index, false);
            assert_eq!(result, 0);
            assert_eq!(observed, original);
            assert_eq!(order, after);
            assert!((*thread).managed_lock.is_none());
            assert_eq!(traffic, (0, 0));
            heap.globals.set(root, 0);
        });
    }

    #[test]
    fn collector_aggregate_write_contention_reloads_moved_base_and_indirect_rhs() {
        const NAME: &str = "runtime::managed::tests::collector_aggregate_write_contention_reloads_moved_base_and_indirect_rhs";
        if std::env::var_os("GCR_AGGREGATE_RELOAD_CHILD").is_none() {
            use std::process::{Command, Stdio};
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME]).env("GCR_AGGREGATE_RELOAD_CHILD", "1")
                .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + std::time::Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!("aggregate relocation wait hung: {:?}", child.wait_with_output().unwrap());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        use crate::gc::{Heap, IdentityPtrPolicy, ThreadState};
        use crate::runtime::{Frame, FrameOrigin};
        use std::{cell::UnsafeCell, sync::mpsc, time::Duration};
        let leaf_info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let holder_info = TypeInfo::for_header_with_interior_ptrs(Full::SIZE, &[16, 24])
            .with_type_id(1).with_raw_bytes(24);
        let heap = Heap::new::<Full>(8192, vec![leaf_info.clone(), holder_info.clone()]);
        let holder = unsafe { heap.alloc_obj::<Full>(&holder_info, 0) };
        let left = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
        let right = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
        unsafe {
            holder.add(16).cast::<[u64; 3]>().write([0, 0, 1]);
            left.add(16).cast::<u64>().write(42);
            right.add(16).cast::<u64>().write(43);
        }
        let initial_rhs = [left as u64, right as u64, 99];
        let root = unsafe { heap.globals.add(holder as u64) };
        let held_index = ManagedAccess::index(holder);
        // Model native stripe ownership while the contender waits. The fixture
        // touches no managed payload through this ownership during collection.
        assert!(heap.managed_access.try_acquire(held_index));
        let (ready, waiting) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut runtime = RuntimeContext::from_heap(heap);
            let heap = runtime.heap().clone();
            runtime.enter(|thread| unsafe {
                let rhs = UnsafeCell::new(initial_rhs);
                let origin = FrameOrigin { num_roots: 0, num_indirect: 2, name: std::ptr::null() };
                #[repr(C)]
                struct ValueFrame { header: Frame, indirect: [*mut u8; 2] }
                let mut frame = ValueFrame {
                    header: Frame { parent: (*thread).top_frame, origin: &origin },
                    indirect: [rhs.get().cast::<u8>(), rhs.get().cast::<u64>().add(1).cast()],
                };
                struct Unlink(*mut Thread, *mut Frame);
                impl Drop for Unlink {
                    fn drop(&mut self) { unsafe { (*self.0).top_frame = self.1; } }
                }
                let _unlink = Unlink(thread, frame.header.parent);
                // Preserve provenance for the header and trailing indirect cells.
                (*thread).top_frame = (&raw mut frame).cast::<Frame>();
                let original = heap.globals.get(root) as *mut u8;
                let references = [0u16, 8];
                assert_eq!(ai_managed_try_write(thread, original, 16, rhs.get().cast(), 24, references.as_ptr(), 2), 0);
                // Only an address crosses threads. Its owner remains alive
                // until the join; the controller observes the atomic state only.
                ready.send((heap.clone(), (*thread).dyna_thread as usize)).unwrap();
                let relocated = ai_managed_lock(thread, original);
                assert!(heap.from_space_contains(relocated));
                assert_ne!(relocated, original);
                let refreshed = rhs.get().read();
                assert_ne!(refreshed[0], initial_rhs[0]);
                assert_ne!(refreshed[1], initial_rhs[1]);
                // Same fallback sequence as generated code: read the rooted
                // local after acquisition, never a pre-wait staging copy.
                std::ptr::copy_nonoverlapping(rhs.get().cast::<u8>(), relocated.add(16), 24);
                ai_managed_unlock(thread);
                let mut observed = [0u64; 3];
                ai_managed_read(thread, relocated, 16, observed.as_mut_ptr().cast(), 24);
                assert_eq!(observed, refreshed);
                for (bits, expected) in [(observed[0], 42), (observed[1], 43)] {
                    let pointer = bits as *mut u8;
                    assert!(heap.from_space_contains(pointer));
                    assert_eq!(pointer.add(16).cast::<u64>().read(), expected);
                }
                assert_eq!(observed[2], 99);
                done.send(()).unwrap();
            });
        });
        let (heap, state_address) = waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let blocked = loop {
            if unsafe { (&*(state_address as *const ThreadState)).is_blocked() } { break true; }
            if Instant::now() >= deadline { break false; }
            std::thread::yield_now();
        };
        if blocked {
            // A waiter may wake while a collection pause is still open. It
            // must not monopolize the stripe while exit_blocked waits for GC.
            let pause = heap.pause_world();
            let before = heap.managed_access.completed_waits.load(Ordering::Acquire);
            heap.managed_access.release(held_index, false);
            let deadline = Instant::now() + Duration::from_secs(5);
            while heap.managed_access.completed_waits.load(Ordering::Acquire) == before {
                assert!(Instant::now() < deadline, "stripe waiter did not wake");
                std::thread::yield_now();
            }
            assert!(unsafe { (&*(state_address as *const ThreadState)).is_blocked() });
            assert!(heap.managed_access.try_acquire(held_index),
                "blocked waiter held a stripe while waiting for the collection pause");
            drop(pause);
            unsafe { heap.stw_collect::<IdentityPtrPolicy>(); }
        }
        heap.managed_access.release(held_index, false);
        worker.join().unwrap();
        assert!(blocked, "contender never published its frame and blocked");
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    /// Single RUNNING mutator, idle collector, one two-word aggregate. Calls
    /// cross black-box function pointers to model the generated runtime ABI.
    #[test]
    #[ignore = "throughput benchmark; run explicitly with --nocapture"]
    fn aggregate_idle_access_throughput() {
        const OPERATIONS: u64 = 250_000;
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(16);
        let mut runtime = RuntimeContext::new(65536, vec![info.clone()]);
        let heap = runtime.heap().clone();
        let thread = runtime.thread_ptr();
        let execute = || {
        let object = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        assert!(!object.is_null());
        unsafe {
            object.add(Full::SIZE).cast::<u64>().write(0);
            object.add(Full::SIZE + 8).cast::<u64>().write(!0);
        }
        let root = unsafe { heap.globals.add(object as u64) };
        let lock =
            black_box(ai_managed_lock as unsafe extern "C" fn(*mut Thread, *mut u8) -> *mut u8);
        let unlock = black_box(ai_managed_unlock as unsafe extern "C" fn(*mut Thread));
        let mut samples = Vec::new();
        for sample in 0..9 {
            let object = heap.globals.get(root) as *mut u8;
            let start = Instant::now();
            for _ in 0..OPERATIONS {
                unsafe {
                    let object = lock(thread, object);
                    let first = object.add(Full::SIZE).cast::<u64>();
                    let second = object.add(Full::SIZE + 8).cast::<u64>();
                    let value = first.read();
                    // Keep both snapshot words observable without placing a
                    // conditional panic or callback inside the held region.
                    let complement = black_box(second.read());
                    first.write(value + 1);
                    second.write(complement - 1);
                    unlock(thread);
                }
            }
            samples.push(u64::try_from(start.elapsed().as_nanos()).unwrap());
            let object = heap.globals.get(root) as *mut u8;
            let expected = (sample + 1) * OPERATIONS;
            assert_eq!(
                unsafe { object.add(Full::SIZE).cast::<u64>().read() },
                expected
            );
            assert_eq!(
                unsafe { object.add(Full::SIZE + 8).cast::<u64>().read() },
                !expected
            );
            assert!(unsafe { (*thread).managed_lock.is_none() });
            assert_eq!(heap.from_used(), info.allocation_size(0));
            assert_eq!(heap.collections(), 0);
        }
        let mut measured = samples[2..].to_vec();
        measured.sort_unstable();
        println!(
            "aggregate benchmark: workers=1 operations={OPERATIONS} median_ns={} raw_ns={samples:?}",
            measured[measured.len() / 2]
        );
        };
        unsafe { crate::runtime::with_current_thread(thread, execute); }
    }
}
