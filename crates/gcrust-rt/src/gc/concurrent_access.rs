//! Scoped access and copy exclusion for concurrently relocated objects.
use super::*;
use std::sync::atomic::AtomicU64;
use allocator_api2::boxed::Box as NativeBox;
use crate::runtime::metadata_allocator::MetadataAllocator;

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum CopyStage {
    BeforeClaim,
    BeforePublication,
}

const STRIPES: usize = 256;

pub(super) struct ObjectLocks {
    slots: NativeBox<[Mutex<()>; STRIPES], MetadataAllocator>,
    prepared: std::sync::Once,
}

pub(super) struct ObjectLockGuards<'h> {
    _first: MutexGuard<'h, ()>,
    _second: Option<MutexGuard<'h, ()>>,
}

impl ObjectLocks {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::new_in(&MetadataAllocator::default())
    }

    pub(super) fn new_in(allocator: &MetadataAllocator) -> Self {
        Self {
            slots: NativeBox::new_in(std::array::from_fn(|_| Mutex::new(())), allocator.clone()),
            prepared: std::sync::Once::new(),
        }
    }

    /// Initialize platform mutex backing before publishing the copying phase.
    /// Some targets allocate on first lock. Keep that work off mutator access
    /// and copy critical paths, leaving unused locks cold in ordinary STW heaps.
    pub(super) fn prepare(&self) {
        self.prepared.call_once(|| {
            for index in 0..STRIPES {
                drop(self.lock(index));
            }
        });
    }

    fn index(object: *const u8) -> usize {
        let word = object.addr() >> 3;
        (word ^ (word >> 8) ^ (word >> 16)) & (STRIPES - 1)
    }

    fn lock(&self, index: usize) -> MutexGuard<'_, ()> {
        // These locks contain no poisonable Rust state. The unsafe access
        // contract requires valid headers/traceable slots even on unwind;
        // internal copy failures terminate the collection instead of resuming.
        self.slots[index]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn one(&self, object: *const u8) -> ObjectLockGuards<'_> {
        ObjectLockGuards {
            _first: self.lock(Self::index(object)),
            _second: None,
        }
    }

    pub(super) fn pair(&self, old: *const u8, new: *const u8) -> ObjectLockGuards<'_> {
        let a = Self::index(old);
        let b = Self::index(new);
        let first = self.lock(a.min(b));
        let second = (a != b).then(|| self.lock(a.max(b)));
        ObjectLockGuards {
            _first: first,
            _second: second,
        }
    }

    #[cfg(test)]
    pub(super) fn is_locked(&self, object: *const u8) -> bool {
        matches!(
            self.slots[Self::index(object)].try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }
}

/// Keeps an object's canonical storage stable against concurrent copying.
///
/// Dereference the resolved pointer only while this guard is alive. Do not allocate managed storage, park,
/// enter a safepoint, invoke managed callbacks, or acquire another object-access
/// guard in this scope. This exclusion does not replace the language's atomic
/// field or aggregate synchronization between mutators when no GC is active.
#[must_use = "retain the guard for the entire object access"]
pub struct ObjectAccess<'h> {
    canonical: *mut u8,
    _locks: Option<ObjectLockGuards<'h>>,
}

impl ObjectAccess<'_> {
    /// The resolved object base. Dereference only under this guard. Interior
    /// addresses must not escape; base aliases still follow normal rooting and
    /// reloading rules at subsequent safepoints.
    pub fn as_ptr(&self) -> *mut u8 {
        self.canonical
    }
}

impl Heap {
    /// Compare published managed references while old/new aliases can coexist.
    ///
    /// Marker-only claims do not expose a destination, so a distinct published
    /// reference cannot yet alias that claimed object. Do not wait for its copy:
    /// this operation can be used inside an access scope whose stripe a copier
    /// may need. A published destination's acquire path orders the source's
    /// forwarding publication before this comparison.
    ///
    /// # Safety
    /// Caller is the owning RUNNING mutator or collector coordinator, with no
    /// safepoint in this operation. Arguments are null or live, published GC
    /// object bases in this heap (not raw/interior/function pointers). Normal
    /// rooting and forwarding-word atomic-access rules apply.
    #[inline]
    pub unsafe fn reference_eq(&self, left: *mut u8, right: *mut u8) -> bool {
        if left == right {
            return true;
        }
        if !self.barriers_active() {
            return false;
        }
        let published = |object: *mut u8| {
            if object.is_null() || !self.from_space().contains(object) {
                return object;
            }
            let word = unsafe {
                &*object
                    .add(self.header.forwarding_offset)
                    .cast::<AtomicU64>()
            };
            let bits = word.load(Ordering::Acquire);
            if bits & FORWARDING_BIT != 0 && bits != FORWARDING_BIT {
                (bits & !FORWARDING_BIT) as *mut u8
            } else {
                object
            }
        };
        published(left) == published(right)
    }

    /// Sequentially consistent compare/exchange of an untagged managed-reference field.
    ///
    /// Comparison uses managed identity; the physical compare/exchange retries
    /// when another writer changes the slot. Failed comparisons do not modify
    /// the slot. SATB logging is conservative on physical-CAS retries.
    ///
    /// # Safety
    /// As `object_access`; `state` is the caller's owning RUNNING registration
    /// in this heap. `offset` identifies an aligned, initialized AtomicU64
    /// reference slot within `object`, outside its header. All values are null
    /// or live published untagged object bases, and all writers use compatible
    /// atomics and GC barriers. Expected/new values remain reachable throughout.
    /// No nested access guard may be held. This operation does not safepoint.
    pub unsafe fn compare_exchange_reference(
        &self,
        state: &ThreadState,
        object: *mut u8,
        offset: usize,
        expected: *mut u8,
        new: *mut u8,
    ) -> Result<*mut u8, *mut u8> {
        let access = unsafe { self.object_access(object) };
        let slot = unsafe { &*access.as_ptr().add(offset).cast::<AtomicU64>() };
        let mut observed = slot.load(Ordering::SeqCst);
        loop {
            if !unsafe { self.reference_eq(observed as *mut u8, expected) } {
                return Err(observed as *mut u8);
            }
            if self.barriers_active() {
                unsafe { state.log_satb(self, observed) };
            }
            match slot.compare_exchange(observed, new as u64, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(previous) => {
                    if self.has_nursery() && self.is_nursery(new) {
                        self.mark_card_dirty(slot as *const AtomicU64 as *const u8);
                    }
                    return Ok(previous as *mut u8);
                }
                Err(actual) => observed = actual,
            }
        }
    }

    /// Resolve and protect an object for a complete, non-safepointing access.
    ///
    /// Published old aliases resolve with an acquire load and acquire only the
    /// destination stripe, as new aliases do. Before publication, the source
    /// stripe excludes copy claims; marker-only claims retry without a guard.
    /// A copier holds the ordered source/destination pair through publication.
    /// No identity table or extra object-header fields are required.
    ///
    /// # Safety
    /// Caller owns a RUNNING registration in this heap, or is its collector
    /// coordinator. `object` is a non-null valid object base, kept reachable
    /// through the access. No safepoint, managed allocation, managed blocking operation or nested
    /// object access is allowed until the guard is dropped. Type IDs, forwarding
    /// representation and physical varlen counts remain valid/immutable; other
    /// header payload obeys the same guarded access protocol. Forwarding words
    /// have atomic observers: any mutation of their other bits during copying
    /// must be a matching-width AtomicU64 operation within this scope.
    /// Layout/root invariants must hold
    /// even on unwind. Apply the usual pointer-write/card/SATB protocols too.
    #[inline]
    pub unsafe fn object_access(&self, object: *mut u8) -> ObjectAccess<'_> {
        assert!(
            !object.is_null(),
            "object access requires a non-null object"
        );
        if !self.barriers_active() {
            return ObjectAccess {
                canonical: object,
                _locks: None,
            };
        }
        if self.from_space().contains(object) {
            let word = unsafe {
                &*object.add(self.header.forwarding_offset).cast::<AtomicU64>()
            };
            let published = word.load(Ordering::Acquire);
            if published & FORWARDING_BIT != 0 && published != FORWARDING_BIT {
                let canonical = (published & !FORWARDING_BIT) as *mut u8;
                // Publication initializes the destination before this acquire.
                // Forwarding cannot change until the caller can safepoint;
                // the destination stripe alone excludes every payload access.
                return ObjectAccess {
                    canonical,
                    _locks: Some(self.object_locks.one(canonical)),
                };
            }
        }
        loop {
            let locked = self.object_locks.one(object);
            if !self.from_space().contains(object) {
                return ObjectAccess {
                    canonical: object,
                    _locks: Some(locked),
                };
            }
            let word = unsafe {
                &*object
                    .add(self.header.forwarding_offset)
                    .cast::<AtomicU64>()
            };
            let original = word.load(Ordering::Acquire);
            if original == FORWARDING_BIT {
                // Never wait for a copier while retaining a stripe it needs.
                drop(locked);
                std::thread::yield_now();
                continue;
            }
            if original & FORWARDING_BIT == 0 {
                return ObjectAccess {
                    canonical: object,
                    _locks: Some(locked),
                };
            }
            let canonical = (original & !FORWARDING_BIT) as *mut u8;
            drop(locked);
            // The source word is now immutable. Protect only canonical storage,
            // including a copier that published but has not released its pair.
            let locked = self.object_locks.one(canonical);
            debug_assert_eq!(word.load(Ordering::Acquire), original);
            return ObjectAccess {
                canonical,
                _locks: Some(locked),
            };
        }
    }
}

/// Exclusive ownership of one source forwarding word. It survives releasing the
/// source stripe to acquire the ordered source/destination pair. Readers retry
/// marker-only claims without retaining a stripe. Only the owner reserves space.
pub(super) struct CopyClaim<'h> {
    pub(super) word: &'h AtomicU64,
    pub(super) original: u64,
    pub(super) published: bool,
}

impl Drop for CopyClaim<'_> {
    fn drop(&mut self) {
        if !self.published {
            self.word.store(self.original, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{Compact, Full, IdentityPtrPolicy};
    use std::sync::atomic::AtomicU64;

    #[test]
    fn collector_access_stripes_reference_published_forwarding_skips_source_lock() {
        if bounded("collector_access_stripes_reference_published_forwarding_skips_source_lock") {
            return;
        }
        fn check<H: ObjHeader>() {
            let info = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
            let heap = Arc::new(Heap::new::<H>(65536, vec![info.clone()]));
            let old = unsafe { heap.alloc_obj::<H>(&info, 0) };
            unsafe {
                old.add(H::SIZE).cast::<u64>().write(42);
            }
            let new = unsafe { heap.copy_or_forward_atomic::<IdentityPtrPolicy>(old) };
            let before = heap.to_space().used();
            let (ready, waiting) = std::sync::mpsc::channel();
            let (release, resume) = std::sync::mpsc::channel();
            let other = heap.clone();
            let address = old as usize;
            let holder = std::thread::spawn(move || {
                let _guard = other.object_locks.one(address as *const u8);
                ready.send(()).unwrap();
                resume.recv().unwrap();
            });
            waiting.recv().unwrap();
            crate::runtime::allocation_probe::start();
            let forwarded = unsafe { heap.copy_or_forward_atomic::<IdentityPtrPolicy>(old) };
            let traffic = crate::runtime::allocation_probe::finish_bytes();
            release.send(()).unwrap();
            holder.join().unwrap();
            assert_eq!(forwarded, new);
            assert_eq!(traffic, (0, 0, 0, 0));
            assert_eq!(heap.to_space().used(), before);
            assert_eq!(unsafe { forwarded.add(H::SIZE).cast::<u64>().read() }, 42);
        }
        check::<Compact>();
        check::<Full>();
        check::<OffsetHeader>();
    }

    #[test]
    fn collector_access_stripes_reference_published_access_locks_only_destination() {
        if bounded("collector_access_stripes_reference_published_access_locks_only_destination") {
            return;
        }
        fn check<H: ObjHeader>() {
            let info = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
            let heap = Arc::new(Heap::new::<H>(65536, vec![info.clone()]));
            // Pick a genuinely distinct stripe: otherwise a held source stripe
            // would also legitimately exclude destination access.
            let (old, new) = (0..128).find_map(|_| {
                let old = unsafe { heap.alloc_obj::<H>(&info, 0) };
                unsafe { old.add(H::SIZE).cast::<u64>().write(42) };
                let new = unsafe { heap.copy_or_forward_atomic::<IdentityPtrPolicy>(old) };
                if ObjectLocks::index(old) != ObjectLocks::index(new) {
                    Some((old, new))
                } else {
                    // Shift destination placement if the two arenas happen to
                    // hash corresponding addresses onto identical stripes.
                    assert!(!unsafe { crate::gc::alloc::alloc_obj::<H>(heap.to_space(), &info, 0) }.is_null());
                    None
                }
            }).expect("fixture requires distinct source/destination stripes");
            heap.object_locks.prepare();
            heap.gc_phase.store(GcPhase::Copying as u8, Ordering::Release);
            let (ready, waiting) = std::sync::mpsc::channel();
            let (release, resume) = std::sync::mpsc::channel();
            let other = heap.clone();
            let address = old as usize;
            let holder = std::thread::spawn(move || {
                let _guard = other.object_locks.one(address as *const u8);
                ready.send(()).unwrap();
                resume.recv().unwrap();
            });
            waiting.recv().unwrap();
            crate::runtime::allocation_probe::start();
            let access = unsafe { heap.object_access(old) };
            let traffic = crate::runtime::allocation_probe::finish_bytes();
            assert_eq!(access.as_ptr(), new);
            assert!(heap.object_locks.is_locked(new), "canonical payload is unprotected");
            assert_eq!(unsafe { access.as_ptr().add(H::SIZE).cast::<u64>().read() }, 42);
            unsafe { access.as_ptr().add(H::SIZE).cast::<u64>().write(43) };
            drop(access);
            assert!(!heap.object_locks.is_locked(new));
            assert_eq!(traffic, (0, 0, 0, 0));
            release.send(()).unwrap();
            holder.join().unwrap();
            assert_eq!(unsafe { old.add(H::SIZE).cast::<u64>().read() }, 42);
            assert_eq!(unsafe { new.add(H::SIZE).cast::<u64>().read() }, 43);
            heap.gc_phase.store(GcPhase::Idle as u8, Ordering::Release);
        }
        check::<Compact>();
        check::<Full>();
        check::<OffsetHeader>();
    }

    #[test]
    fn collector_access_stripes_reference_satb_flush_reuses_mutator_buffer() {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let heap = Heap::new::<Full>(65536, vec![info.clone()]);
        let old = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        let (state, _) = heap.register_thread();
        // Isolate an active bounded queue, with no collection/safepoint allowed
        // during the native allocation measurement. The real pressure fixture
        // separately checks tracing and final reclamation under overwrites.
        heap.satb_queue.begin_cycle::<IdentityPtrPolicy>(old, info.allocation_size(0));
        heap.gc_phase.store(GcPhase::Copying as u8, Ordering::Release);
        let mut capacity = 0;
        while !unsafe { state.satb_buffer() }.should_flush() {
            unsafe { state.satb_buffer() }.log(old as u64);
            capacity += 1;
        }
        assert!(capacity > 0, "fixture must exercise nonempty full buffers");
        heap.satb_queue.flush_buffer(unsafe { state.satb_buffer() });
        // Warm the first unique publication and queue storage before measuring
        // duplicate-only chunks, including duplicates after a collector drain.
        for _ in 0..capacity {
            unsafe { state.log_satb(&heap, old as u64) };
        }
        assert_eq!(heap.satb_queue.drain_all(), [old as u64]);
        crate::runtime::allocation_probe::start();
        for _ in 0..64 {
            for _ in 0..capacity {
                unsafe { state.log_satb(&heap, old as u64) };
            }
        }
        let traffic = crate::runtime::allocation_probe::finish_bytes();
        assert_eq!(traffic, (0, 0, 0, 0), "duplicate SATB flush replaced local storage");
        assert!(unsafe { state.satb_buffer() }.is_empty());
        assert!(heap.satb_queue.is_empty());
        assert_eq!(heap.satb_queue.stats().unique_values, 1);
        heap.satb_queue.finish_cycle();
        heap.gc_phase.store(GcPhase::Idle as u8, Ordering::Release);
        unsafe { heap.safe_deregister_thread(&state) };
    }

    #[test]
    fn collector_access_stripes_reference_marker_waits_for_publication() {
        if bounded("collector_access_stripes_reference_marker_waits_for_publication") {
            return;
        }
        use std::sync::mpsc;
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut heap = Arc::new(Heap::new::<Full>(65536, vec![info.clone()]));
        let old = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        unsafe {
            old.add(Full::SIZE).cast::<u64>().write(42);
        }
        let address = old as usize;
        let (ready, publication_paused) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let resume = Mutex::new(resume);
        let (entered, waiter_entered) = mpsc::channel();
        let attempts = AtomicUsize::new(0);
        Arc::get_mut(&mut heap).unwrap().concurrent_copy_observer =
            Some(Arc::new(move |stage, source, destination| {
                assert_eq!(source as usize, address);
                match stage {
                    CopyStage::BeforeClaim => {
                        if attempts.fetch_add(1, Ordering::Relaxed) == 1 {
                            entered.send(()).unwrap();
                        }
                    }
                    CopyStage::BeforePublication => {
                        ready.send(destination as usize).unwrap();
                        resume.lock().unwrap().recv().unwrap();
                    }
                }
            }));
        let other = heap.clone();
        let copier = std::thread::spawn(move || unsafe {
            other.copy_or_forward_atomic::<IdentityPtrPolicy>(address as *mut u8) as usize
        });
        let destination = publication_paused.recv().unwrap();
        assert_eq!(
            unsafe {
                (&*old.add(Full::HEAP_FORWARDING_OFFSET).cast::<AtomicU64>()).load(Ordering::Acquire)
            },
            FORWARDING_BIT
        );
        let (done, result) = mpsc::channel();
        let other = heap.clone();
        let waiter = std::thread::spawn(move || {
            let result = unsafe {
                other.copy_or_forward_atomic::<IdentityPtrPolicy>(address as *mut u8) as usize
            };
            done.send(result).unwrap();
        });
        waiter_entered.recv().unwrap();
        assert!(
            matches!(
                result.recv_timeout(std::time::Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "a marker-only claim is not a published destination"
        );
        release.send(()).unwrap();
        assert_eq!(result.recv().unwrap(), destination);
        assert_eq!(copier.join().unwrap(), destination);
        waiter.join().unwrap();
        assert_eq!(heap.to_space().used(), info.allocation_size(0));
        assert_eq!(
            unsafe {
                (destination as *const u8)
                    .add(Full::SIZE)
                    .cast::<u64>()
                    .read()
            },
            42
        );
    }

    struct ScanTagged;
    impl PtrPolicy for ScanTagged {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            (bits & 7 == 3).then_some((bits & !7) as *mut u8)
        }
        fn encode_ptr(pointer: *mut u8) -> u64 {
            pointer as u64 | 3
        }
    }

    fn scan_boundaries<H: ObjHeader, P: PtrPolicy>() {
        for fixed in [0u16, 1, 31, 32, 33] {
            for interiors in [0u16, 2] {
                for length in [0usize, 1, 31, 32, 33, 65] {
                    let owner_info = TypeInfo::for_header(H::SIZE)
                        .with_raw_bytes((interiors + 1) * 8)
                        .with_varlen_values(fixed)
                        .with_owned_interior_ptrs(
                            (0..interiors)
                                .map(|index| (H::SIZE + fixed as usize * 8 + index as usize * 8) as u16)
                                .collect(),
                        );
                    let leaf_info = TypeInfo::for_header(H::SIZE)
                        .with_type_id(1)
                        .with_raw_bytes(8);
                    let heap = Heap::new::<H>(65536, vec![owner_info.clone(), leaf_info.clone()]);
                    let owner = unsafe { heap.alloc_obj::<H>(&owner_info, length) };
                    let leaves: Vec<_> = (0..7)
                        .map(|index| unsafe {
                            let leaf = heap.alloc_obj::<H>(&leaf_info, 0);
                            leaf.add(H::SIZE).cast::<u64>().write(42 + index);
                            leaf
                        })
                        .collect();
                    let mut used = [false; 7];
                    let mut index = 0;
                    unsafe {
                        scan_object(owner, &owner_info, |slot| {
                            if index % 5 == 0 {
                                slot.write(0);
                            } else {
                                used[index % 7] = true;
                                slot.write(P::encode_ptr(leaves[index % 7]));
                            }
                            index += 1;
                        });
                    }
                    assert_eq!(index, fixed as usize + interiors as usize + length);
                    let sentinel = H::SIZE + fixed as usize * 8 + interiors as usize * 8;
                    unsafe {
                        owner.add(sentinel).cast::<u64>().write(0xfeed_beef);
                    }
                    let root = unsafe { heap.globals.add(P::encode_ptr(owner)) };
                    for _ in 0..2 {
                        unsafe {
                            heap.concurrent_collect::<P>();
                        }
                        let owner = P::try_decode_ptr(heap.globals.get(root)).unwrap();
                        assert_eq!(
                            heap.from_used(),
                            owner_info.allocation_size(length)
                                + used.iter().filter(|&&value| value).count()
                                    * leaf_info.allocation_size(0)
                        );
                        assert_eq!(
                            unsafe { owner.add(sentinel).cast::<u64>().read() },
                            0xfeed_beef
                        );
                        let mut index = 0;
                        unsafe {
                            scan_object(owner, &owner_info, |slot| {
                                let bits = slot.read();
                                if index % 5 == 0 {
                                    assert_eq!(bits, 0);
                                } else {
                                    let leaf = P::try_decode_ptr(bits).unwrap();
                                    assert!(heap.from_space().contains(leaf));
                                    assert_eq!(
                                        leaf.add(H::SIZE).cast::<u64>().read(),
                                        42 + (index % 7) as u64
                                    );
                                }
                                index += 1;
                            });
                        }
                        assert_eq!(index, fixed as usize + interiors as usize + length);
                    }
                }
            }
        }
    }

    #[test]
    fn collector_access_stripes_reference_batched_scan_preserves_all_slot_shapes() {
        if bounded("collector_access_stripes_reference_batched_scan_preserves_all_slot_shapes") {
            return;
        }
        scan_boundaries::<Compact, IdentityPtrPolicy>();
        scan_boundaries::<Full, IdentityPtrPolicy>();
        scan_boundaries::<Compact, ScanTagged>();
        scan_boundaries::<Full, ScanTagged>();
    }

    #[test]
    fn collector_access_stripes_reference_batched_scan_releases_owner_and_uses_no_native_scratch() {
        if bounded(
            "collector_access_stripes_reference_batched_scan_releases_owner_and_uses_no_native_scratch",
        ) {
            return;
        }
        let owner_info = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
        let leaf_info = TypeInfo::for_header(Full::SIZE)
            .with_type_id(1)
            .with_raw_bytes(8);
        let padding_info = TypeInfo::for_header(Full::SIZE)
            .with_type_id(2)
            .with_varlen_bytes(0);
        let heap = Heap::new::<Full>(
            1 << 20,
            vec![owner_info.clone(), leaf_info.clone(), padding_info.clone()],
        );
        let owner = unsafe { heap.alloc_obj::<Full>(&owner_info, 65) };
        heap.object_locks.prepare();
        let copied = unsafe { heap.copy_or_forward_atomic::<IdentityPtrPolicy>(owner) };
        // Choose padding in eight-byte increments rather than hoping a run of
        // 24-byte leaves visits the needed hash bucket. A 4096-byte interval
        // contains a complete aligned 2048-byte block: its 256 word addresses
        // cover every stripe while the hash's higher input bits stay fixed.
        let start = unsafe { heap.from_space().base().add(heap.from_used()) };
        let minimum_padding = padding_info.allocation_size(0);
        let padding = (minimum_padding..=minimum_padding + 4096)
            .step_by(8)
            .find(|&bytes| {
                ObjectLocks::index(unsafe { start.add(bytes) }) == ObjectLocks::index(copied)
            })
            .expect("complete aligned stripe block must contain a collision");
        let padding_object = unsafe {
            heap.alloc_obj::<Full>(&padding_info, padding - minimum_padding)
        };
        assert_eq!(padding_object, start);
        let leaf = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
        assert_eq!(leaf, unsafe { start.add(padding) });
        assert_eq!(ObjectLocks::index(leaf), ObjectLocks::index(copied));
        unsafe { leaf.add(Full::SIZE).cast::<u64>().write(42) };
        // The last partial batch is the only path to this distinct child.
        let tail = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
        unsafe {
            tail.add(Full::SIZE).cast::<u64>().write(43);
        }
        for index in 0..65 {
            unsafe {
                copied
                    .add(owner_info.varlen_count_offset() + 8 + index * 8)
                    .cast::<u64>()
                    .write(if index == 64 {
                        tail as u64
                    } else {
                        leaf as u64
                    });
            }
        }
        crate::runtime::allocation_probe::start();
        unsafe {
            heap.scan_object_concurrent::<IdentityPtrPolicy>(copied, &owner_info);
        }
        assert_eq!(
            crate::runtime::allocation_probe::finish_bytes(),
            (0, 0, 0, 0)
        );
        assert_eq!(
            heap.to_space().used(),
            owner_info.allocation_size(65) + 2 * leaf_info.allocation_size(0)
        );
        let moved = unsafe { heap.check_forwarded_atomic(leaf) }.unwrap();
        assert_eq!(unsafe { moved.add(Full::SIZE).cast::<u64>().read() }, 42);
        let moved_tail = unsafe { heap.check_forwarded_atomic(tail) }.unwrap();
        assert_eq!(
            unsafe { moved_tail.add(Full::SIZE).cast::<u64>().read() },
            43
        );
        for index in 0..65 {
            assert_eq!(
                unsafe {
                    copied
                        .add(owner_info.varlen_count_offset() + 8 + index * 8)
                        .cast::<u64>()
                        .read()
                },
                if index == 64 {
                    tail as u64
                } else {
                    leaf as u64
                },
                "concurrent discovery must not heal parent slots"
            );
        }
    }

    #[test]
    fn collector_access_stripes_reference_cycle_prepares_all_native_locks() {
        let heap = Heap::new::<Full>(65536, vec![]);
        assert!(!heap.object_locks.prepared.is_completed());
        // No copied object touches a stripe: preparation must cover even the
        // first later object mapped to an otherwise unused stripe.
        unsafe { heap.concurrent_collect::<IdentityPtrPolicy>() };
        assert!(heap.object_locks.prepared.is_completed());
        crate::runtime::allocation_probe::start();
        heap.object_locks.prepare();
        for index in 0..STRIPES {
            drop(heap.object_locks.lock(index));
        }
        let traffic = crate::runtime::allocation_probe::finish();
        assert_eq!(traffic, (0, 0), "native lock initialization escaped into access");
    }

    #[test]
    fn collector_aggregate_read_completes_while_concurrent_copy_is_paused() {
        if bounded("collector_aggregate_read_completes_while_concurrent_copy_is_paused") { return; }
        use crate::runtime::{RuntimeContext, ai_managed_read, ai_slot_bits64_load};
        use std::sync::mpsc;
        use std::time::Duration;
        let leaf_info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let aggregate_info = TypeInfo::for_header(Full::SIZE).with_type_id(1).with_fields(1).with_raw_bytes(16);
        let owner_info = TypeInfo::for_header(Full::SIZE).with_type_id(2).with_fields(1);
        let mut heap = Heap::new::<Full>(65536, vec![leaf_info.clone(), aggregate_info.clone(), owner_info.clone()]);
        let leaf = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
        let aggregate = unsafe { heap.alloc_obj::<Full>(&aggregate_info, 0) };
        let owner = unsafe { heap.alloc_obj::<Full>(&owner_info, 0) };
        unsafe {
            leaf.add(Full::SIZE).cast::<u64>().write(777);
            aggregate.add(Full::SIZE).cast::<u64>().write(leaf as u64);
            aggregate.add(Full::SIZE + 8).cast::<u64>().write(11);
            aggregate.add(Full::SIZE + 16).cast::<u64>().write(22);
            owner.add(Full::SIZE).cast::<u64>().write(aggregate as u64);
        }
        let root = unsafe { heap.globals.add(owner as u64) };
        let leaf_address = leaf as usize;
        let (paused, pause_seen) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let resume = Mutex::new(resume);
        heap.concurrent_copy_observer = Some(Arc::new(move |stage, source, _| {
            if source as usize == leaf_address && matches!(stage, CopyStage::BeforeClaim) {
                paused.send(()).unwrap();
                resume.lock().unwrap().recv().unwrap();
            }
        }));
        let (ready, ready_heap) = mpsc::channel();
        let (read, start_read) = mpsc::channel();
        let (done, read_done) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut runtime = RuntimeContext::from_heap(heap);
            let heap = runtime.heap().clone();
            runtime.enter(|thread| unsafe {
                let state = &*(*thread).dyna_thread;
                ready.send(heap.clone()).unwrap();
                loop {
                    if heap.gc_requested() { state.enter_safepoint(); }
                    if start_read.try_recv().is_ok() { break; }
                    std::thread::yield_now();
                }
                // Reload the root after pause one. Its unhealed field supplies
                // a valid old alias without retaining pointers across a pause.
                {
                    let owner = heap.globals.get(root) as *mut u8;
                    let old = ai_slot_bits64_load(thread, owner, Full::SIZE) as *mut u8;
                    let canonical = heap.read_barrier(old);
                    assert_ne!(old, canonical);
                    {
                        let access = heap.object_access(canonical);
                        access.as_ptr().add(Full::SIZE + 8).cast::<u64>().write(33);
                        access.as_ptr().add(Full::SIZE + 16).cast::<u64>().write(44);
                    }
                    for alias in [old, canonical] {
                        let mut snapshot = [0u64; 3];
                        ai_managed_read(thread, alias, Full::SIZE, snapshot.as_mut_ptr().cast(), 24);
                        assert_eq!(snapshot, [leaf_address as u64, 33, 44]);
                    }
                }
                done.send(()).unwrap();
                // All derived aliases and snapshot references are dead before
                // the final pause. Continue participating in both handshakes.
                while !worker_stop.load(Ordering::Acquire) {
                    if heap.gc_requested() { state.enter_safepoint(); }
                    std::thread::yield_now();
                }
            });
        });
        let heap = ready_heap.recv_timeout(Duration::from_secs(5)).unwrap();
        let collecting = heap.clone();
        let collector = std::thread::spawn(move || unsafe { collecting.concurrent_collect::<IdentityPtrPolicy>() });
        pause_seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(heap.barriers_active());
        read.send(()).unwrap();
        let completed = read_done.recv_timeout(Duration::from_secs(5));
        // Always release the collector before asserting, including fault runs.
        release.send(()).unwrap();
        collector.join().unwrap();
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        assert!(completed.is_ok(), "aggregate read waited for the entire copying phase");
    }

    #[test]
    fn collector_aggregate_write_preserves_satb_and_nursery_card_during_copy() {
        if bounded("collector_aggregate_write_preserves_satb_and_nursery_card_during_copy") { return; }
        use crate::runtime::{RuntimeContext, ai_managed_read, ai_managed_try_write, ai_slot_bits64_load};
        use std::sync::mpsc;
        use std::time::Duration;
        let leaf_info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let parent_info = TypeInfo::for_header_with_interior_ptrs(Full::SIZE, &[32, 56])
            .with_type_id(1).with_fields(1).with_raw_bytes(40);
        let owner_info = TypeInfo::for_header(Full::SIZE).with_type_id(2).with_fields(1);
        let mut heap = Heap::new_generational::<Full>(8192, 65536, vec![leaf_info.clone(), parent_info.clone(), owner_info.clone()]);
        let anchor = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
        let displaced = unsafe { heap.alloc_obj::<Full>(&leaf_info, 0) };
        let parent = unsafe { heap.alloc_obj::<Full>(&parent_info, 0) };
        let owner = unsafe { heap.alloc_obj::<Full>(&owner_info, 0) };
        let young = unsafe { heap.alloc_nursery_obj::<Full>(&leaf_info, 0) };
        unsafe {
            anchor.add(16).cast::<u64>().write(111);
            displaced.add(16).cast::<u64>().write(42);
            young.add(16).cast::<u64>().write(999);
            parent.add(16).cast::<u64>().write(anchor as u64);
            parent.add(24).cast::<u64>().write(11);
            parent.add(32).cast::<u64>().write(displaced as u64);
            parent.add(40).cast::<u64>().write(22);
            parent.add(48).cast::<u64>().write(0xfeed);
            parent.add(56).cast::<u64>().write(anchor as u64);
            owner.add(16).cast::<u64>().write(parent as u64);
        }
        let root = unsafe { heap.globals.add(owner as u64) };
        let young_root = unsafe { heap.globals.add(young as u64) };
        let anchor_address = anchor as usize;
        let (paused, pause_seen) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let resume = Mutex::new(resume);
        let pause_once = AtomicBool::new(false);
        heap.concurrent_copy_observer = Some(Arc::new(move |stage, source, _| {
            if source as usize == anchor_address && matches!(stage, CopyStage::BeforeClaim)
                && !pause_once.swap(true, Ordering::AcqRel) {
                paused.send(()).unwrap();
                resume.lock().unwrap().recv().unwrap();
            }
        }));
        let (ready, ready_heap) = mpsc::channel();
        let (write, start_write) = mpsc::channel();
        let (done, write_done) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut runtime = RuntimeContext::from_heap(heap);
            let heap = runtime.heap().clone();
            runtime.enter(|thread| unsafe {
                let state = &*(*thread).dyna_thread;
                ready.send(heap.clone()).unwrap();
                loop {
                    if heap.gc_requested() { state.enter_safepoint(); }
                    if start_write.try_recv().is_ok() { break; }
                    std::thread::yield_now();
                }
                {
                    let owner = heap.globals.get(root) as *mut u8;
                    let old = ai_slot_bits64_load(thread, owner, 16) as *mut u8;
                    let canonical = heap.read_barrier(old);
                    assert_ne!(old, canonical);
                    let young = heap.globals.get(young_root);
                    let references = [8u16];
                    let input = [33u64, young, 44];
                    let destination = 1 - heap.from_idx.load(Ordering::Acquire);
                    let table = &heap.nursery_state.as_ref().unwrap().card_tables[destination];
                    let card = (canonical.add(32) as usize - heap.to_space().base() as usize) / table.card_size();
                    assert!(!table.is_dirty(card));
                    assert_eq!(ai_managed_try_write(thread, old, 24, input.as_ptr().cast(), 24, references.as_ptr(), 1), 1);
                    assert!(table.is_dirty(card), "write missed its canonical destination card before final rescan");
                    for alias in [old, canonical] {
                        let mut snapshot = [0u64; 3];
                        ai_managed_read(thread, alias, 24, snapshot.as_mut_ptr().cast(), 24);
                        assert_eq!(snapshot, input);
                    }
                    let access = heap.object_access(canonical);
                    assert_eq!(access.as_ptr().add(48).cast::<u64>().read(), 0xfeed);
                    assert_eq!(access.as_ptr().add(56).cast::<u64>().read(), anchor_address as u64);
                    drop(access);
                    heap.globals.set(young_root, 0);
                }
                done.send(()).unwrap();
                while !worker_stop.load(Ordering::Acquire) {
                    if heap.gc_requested() { state.enter_safepoint(); }
                    std::thread::yield_now();
                }
            });
        });
        let heap = ready_heap.recv_timeout(Duration::from_secs(5)).unwrap();
        let collecting = heap.clone();
        let collector = std::thread::spawn(move || unsafe { collecting.concurrent_collect::<IdentityPtrPolicy>() });
        pause_seen.recv_timeout(Duration::from_secs(5)).unwrap();
        write.send(()).unwrap();
        let completed = write_done.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        collector.join().unwrap();
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        assert!(completed.is_ok(), "aggregate write waited for the copying phase");
        // The overwritten child had not been discovered when we replaced it.
        // Only the SATB log can retain its snapshot reachability this cycle.
        let mut displaced_survived = false;
        unsafe { heap.from_space().walk(&heap.type_table, &mut |object, info| {
            if info.type_id == 0 && object.add(16).cast::<u64>().read() == 42 { displaced_survived = true; }
        }); }
        assert!(displaced_survived, "overwritten snapshot edge was not retained");
        let (state, _) = heap.register_thread();
        unsafe {
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state);
            let owner = heap.globals.get(root) as *mut u8;
            let parent = owner.add(16).cast::<u64>().read() as *mut u8;
            let promoted = parent.add(32).cast::<u64>().read() as *mut u8;
            assert!(heap.from_space_contains(promoted), "new edge missed the destination card");
            assert_eq!(promoted.add(16).cast::<u64>().read(), 999);
            assert_eq!(parent.add(48).cast::<u64>().read(), 0xfeed);
            heap.safe_deregister_thread(&state);
        }
    }

    fn bounded(test: &str) -> bool {
        if std::env::var_os("GCR_OBJECT_ACCESS_CHILD").is_some() {
            return false;
        }
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("gc::heap::concurrent_access::tests::{test}"),
            ])
            .env("GCR_OBJECT_ACCESS_CHILD", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("object-access protocol hung: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        true
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct OffsetHeader {
        pad: u16,
        id: u16,
        extra: u32,
    }
    // Initialized eight-byte header, ID at offset two, clear forwarding bit.
    unsafe impl ObjHeader for OffsetHeader {
        const SIZE: usize = 8;
        const TYPE_ID_OFFSET: usize = 2;
        fn new(id: u16) -> Self {
            Self {
                pad: 0x1111,
                id,
                extra: 0x2222,
            }
        }
        fn type_id(&self) -> u16 {
            self.id
        }
    }

    unsafe fn write_payload(pointer: *mut u8, width: usize, value: u64) {
        unsafe {
            match width {
                1 => pointer.cast::<u8>().write(value as u8),
                2 => pointer.cast::<u16>().write(value as u16),
                4 => pointer.cast::<u32>().write(value as u32),
                8 => pointer.cast::<u64>().write(value),
                _ => unreachable!(),
            }
        }
    }
    unsafe fn read_payload(pointer: *mut u8, width: usize) -> u64 {
        unsafe {
            match width {
                1 => pointer.cast::<u8>().read() as u64,
                2 => pointer.cast::<u16>().read() as u64,
                4 => pointer.cast::<u32>().read() as u64,
                8 => pointer.cast::<u64>().read(),
                _ => unreachable!(),
            }
        }
    }

    fn actual_cycle<H: ObjHeader>(width: usize) {
        let leaf = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
        let parent_info = TypeInfo::for_header(H::SIZE).with_type_id(1).with_fields(2);
        let mut heap = Heap::new_generational::<H>(1 << 18, 1 << 18, vec![leaf.clone(), parent_info.clone()]);
        let old = unsafe { heap.alloc_obj::<H>(&leaf, 0) };
        let parent = unsafe { heap.alloc_obj::<H>(&parent_info, 0) };
        assert!(!old.is_null() && !parent.is_null());
        unsafe {
            parent
                .add(parent_info.value_field_offset(0))
                .cast::<u64>()
                .write(old as u64);
        }
        let young = unsafe { heap.alloc_nursery_obj::<H>(&leaf, 0) };
        assert!(!young.is_null());
        unsafe {
            young.add(H::SIZE).cast::<u64>().write(777);
        }
        let young_address = young as usize;
        let root = unsafe { heap.globals.add(parent as u64) };
        let young_root = unsafe { heap.globals.add(young as u64) };
        let old_address = old as usize;
        let (before, before_copy) = std::sync::mpsc::channel();
        let (allow, allow_copy) = std::sync::mpsc::channel();
        let allow_copy = Mutex::new(allow_copy);
        let (copied, copy_ready) = std::sync::mpsc::channel();
        let (release, release_copy) = std::sync::mpsc::channel();
        let release_copy = Mutex::new(release_copy);
        heap.concurrent_copy_observer = Some(Arc::new(move |stage, source, destination| {
            if source as usize == old_address {
                match stage {
                    CopyStage::BeforeClaim => {
                        before.send(()).unwrap();
                        allow_copy.lock().unwrap().recv().unwrap();
                    }
                    CopyStage::BeforePublication => {
                        copied.send(destination as usize).unwrap();
                        release_copy.lock().unwrap().recv().unwrap();
                    }
                }
            }
        }));
        let heap = Arc::new(heap);
        let worker_heap = heap.clone();
        let (ready, worker_ready) = std::sync::mpsc::channel();
        let (prepare, prepare_access) = std::sync::mpsc::channel();
        let (prepared, access_prepared) = std::sync::mpsc::channel();
        let (go, start_access) = std::sync::mpsc::channel();
        let (attempted, access_attempted) = std::sync::mpsc::channel();
        let (done, access_done) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_parent_info = parent_info.clone();
        let worker = std::thread::spawn(move || {
            let (state, _) = worker_heap.register_thread();
            ready.send(()).unwrap();
            loop {
                if worker_heap.gc_requested() {
                    unsafe {
                        state.enter_safepoint();
                    }
                }
                if prepare_access.try_recv().is_ok() {
                    break;
                }
                std::thread::yield_now();
            }
            // Reload rooted values after the first pause. The parent field
            // still contains a legitimate old-space alias; keep it only in
            // this non-safepointing interval, never across the final pause.
            let parent_address = worker_heap.globals.get(root) as usize;
            let young_address = worker_heap.globals.get(young_root) as usize;
            let old_address = {
                let access = unsafe { worker_heap.object_access(parent_address as *mut u8) };
                unsafe {
                    (&*access
                        .as_ptr()
                        .add(worker_parent_info.value_field_offset(0))
                        .cast::<AtomicU64>())
                        .load(Ordering::SeqCst) as usize
                }
            };
            prepared.send(old_address).unwrap();
            start_access.recv().unwrap();
            // A claim must not make identity comparison wait while its
            // collector is intentionally paused before publication.
            assert!(!unsafe {
                worker_heap.reference_eq(old_address as *mut u8, parent_address as *mut u8)
            });
            attempted.send(()).unwrap();
            // This access begins after the collector took its payload snapshot,
            // before forwarding publication. Its write must reach the canonical
            // destination; narrow payload operations must not overlap copying.
            let canonical = {
                let access = unsafe { worker_heap.object_access(old_address as *mut u8) };
                let canonical = access.as_ptr();
                assert!(worker_heap.object_locks.is_locked(canonical));
                unsafe {
                    write_payload(canonical.add(H::SIZE), width, 42);
                }
                canonical
            };
            assert!(unsafe { worker_heap.reference_eq(old_address as *mut u8, canonical) });
            assert!(unsafe { worker_heap.reference_eq(canonical, old_address as *mut u8) });
            assert!(unsafe {
                worker_heap.reference_eq(std::ptr::null_mut(), std::ptr::null_mut())
            });
            assert!(!unsafe {
                worker_heap.reference_eq(old_address as *mut u8, std::ptr::null_mut())
            });
            for alias in [old_address as *mut u8, canonical] {
                let access = unsafe { worker_heap.object_access(alias) };
                assert_eq!(
                    unsafe { read_payload(access.as_ptr().add(H::SIZE), width) },
                    42
                );
            }
            // The expected reference is the destination alias while the slot
            // may still hold the source alias (or have been healed by scanning).
            assert!(
                unsafe {
                    worker_heap.compare_exchange_reference(
                        &state,
                        parent_address as *mut u8,
                        worker_parent_info.value_field_offset(0),
                        canonical,
                        old_address as *mut u8,
                    )
                }
                .is_ok()
            );
            assert!(
                unsafe {
                    worker_heap.compare_exchange_reference(
                        &state,
                        parent_address as *mut u8,
                        worker_parent_info.value_field_offset(0),
                        parent_address as *mut u8,
                        young_address as *mut u8,
                    )
                }
                .is_err()
            );
            assert_eq!(
                unsafe {
                    worker_heap.compare_exchange_reference(
                        &state,
                        parent_address as *mut u8,
                        worker_parent_info.value_field_offset(1),
                        std::ptr::null_mut(),
                        young_address as *mut u8,
                    )
                },
                Ok(std::ptr::null_mut())
            );
            // The sole tenured root is copied at destination offset zero.
            let destination = 1 - worker_heap.from_idx.load(Ordering::Acquire);
            assert!(
                worker_heap.nursery_state.as_ref().unwrap().card_tables[destination].is_dirty(0)
            );
            done.send(canonical as usize).unwrap();
            // Both aliases are dead before entering the next safepoint.
            while !worker_stop.load(Ordering::Acquire) {
                if worker_heap.gc_requested() {
                    unsafe {
                        state.enter_safepoint();
                    }
                } else {
                    std::thread::yield_now();
                }
            }
            unsafe {
                worker_heap.safe_deregister_thread(&state);
            }
        });
        worker_ready.recv().unwrap();
        let collector_heap = heap.clone();
        let collector = std::thread::spawn(move || unsafe {
            collector_heap.concurrent_collect::<IdentityPtrPolicy>();
        });
        before_copy.recv().unwrap();
        prepare.send(()).unwrap();
        assert_eq!(access_prepared.recv().unwrap(), old as usize);
        allow.send(()).unwrap();
        let destination = copy_ready.recv().unwrap() as *mut u8;
        assert!(heap.object_locks.is_locked(old));
        assert!(heap.object_locks.is_locked(destination));
        assert_eq!(
            unsafe {
                (&*old.add(H::HEAP_FORWARDING_OFFSET).cast::<AtomicU64>()).load(Ordering::Acquire)
            },
            FORWARDING_BIT
        );
        go.send(()).unwrap();
        access_attempted.recv().unwrap();
        release.send(()).unwrap();
        assert_eq!(access_done.recv().unwrap(), destination as usize);
        collector.join().unwrap();
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        let parent = heap.globals.get(root) as *mut u8;
        let moved = unsafe {
            parent
                .add(parent_info.value_field_offset(0))
                .cast::<*mut u8>()
                .read()
        };
        assert_eq!(moved, destination);
        assert_ne!(moved, old);
        assert_eq!(unsafe { read_payload(moved.add(H::SIZE), width) }, 42);
        let expected = H::new(0);
        assert_eq!(
            unsafe { std::slice::from_raw_parts(moved, H::SIZE) },
            unsafe { std::slice::from_raw_parts((&expected as *const H).cast::<u8>(), H::SIZE) }
        );
        assert_eq!(
            heap.from_used(),
            leaf.allocation_size(0) + parent_info.allocation_size(0)
        );
        assert_eq!(heap.collections(), 1);
        unsafe {
            heap.globals.set(young_root, 0);
        }
        let (state, _) = heap.register_thread();
        unsafe {
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state);
        }
        let parent = heap.globals.get(root) as *mut u8;
        let promoted = unsafe {
            parent
                .add(parent_info.value_field_offset(1))
                .cast::<*mut u8>()
                .read()
        };
        assert_ne!(promoted as usize, young_address);
        assert!(heap.from_space().contains(promoted));
        assert_eq!(unsafe { promoted.add(H::SIZE).cast::<u64>().read() }, 777);
        assert_eq!(
            heap.from_used(),
            2 * leaf.allocation_size(0) + parent_info.allocation_size(0)
        );
        assert_eq!(heap.minor_collections(), 1);
        unsafe {
            heap.safe_deregister_thread(&state);
        }
    }

    #[test]
    fn collector_scoped_access_preserves_writes_between_copy_and_publication() {
        if bounded("collector_scoped_access_preserves_writes_between_copy_and_publication") {
            return;
        }
        for width in [1, 2, 4, 8] {
            actual_cycle::<Compact>(width);
            actual_cycle::<Full>(width);
            actual_cycle::<OffsetHeader>(width);
        }
    }

    #[test]
    fn collector_reference_cas_compares_aliases_and_preserves_failed_slot() {
        if bounded("collector_reference_cas_compares_aliases_and_preserves_failed_slot") {
            return;
        }
        let info = TypeInfo::for_header(Full::SIZE).with_fields(1);
        let heap = Heap::new::<Full>(1 << 18, vec![info.clone()]);
        let child = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        let offset = info.value_field_offset(0);
        unsafe { owner.add(offset).cast::<u64>().write(child as u64) };
        let (state, _) = heap.register_thread();
        // Isolate the post-publication interval without a scanner healing the
        // old slot first. The real-cycle test separately exercises both pauses.
        let moved_child = unsafe { heap.copy_or_forward_atomic::<IdentityPtrPolicy>(child) };
        let moved_owner = unsafe { heap.copy_or_forward_atomic::<IdentityPtrPolicy>(owner) };
        heap.gc_phase
            .store(GcPhase::Copying as u8, Ordering::Release);
        let slot = unsafe { &*moved_owner.add(offset).cast::<AtomicU64>() };
        assert_eq!(slot.load(Ordering::SeqCst), child as u64);
        assert_eq!(
            unsafe {
                heap.compare_exchange_reference(&state, owner, offset, moved_child, moved_owner)
            },
            Ok(child)
        );
        assert_eq!(slot.load(Ordering::SeqCst), moved_owner as u64);
        assert_eq!(
            unsafe {
                heap.compare_exchange_reference(
                    &state,
                    moved_owner,
                    offset,
                    child,
                    std::ptr::null_mut(),
                )
            },
            Err(moved_owner)
        );
        assert_eq!(slot.load(Ordering::SeqCst), moved_owner as u64);
        assert_eq!(
            unsafe {
                heap.compare_exchange_reference(
                    &state,
                    moved_owner,
                    offset,
                    owner,
                    std::ptr::null_mut(),
                )
            },
            Ok(moved_owner)
        );
        assert_eq!(slot.load(Ordering::SeqCst), 0);
        let logged = unsafe { state.satb_buffer().drain() };
        assert_eq!(logged, vec![child as u64, moved_owner as u64]);
        heap.gc_phase.store(GcPhase::Idle as u8, Ordering::Release);
        unsafe { heap.safe_deregister_thread(&state) };
    }

    #[test]
    fn collector_concurrent_claim_reserves_one_destination_under_contention() {
        if bounded("collector_concurrent_claim_reserves_one_destination_under_contention") {
            return;
        }
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let heap = Heap::new::<Full>(1 << 18, vec![info.clone()]);
        let old = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        unsafe {
            old.add(Full::SIZE).cast::<u64>().write(42);
        }
        let old_address = old as usize;
        let start = std::sync::Barrier::new(8);
        let results = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        unsafe {
                            heap.copy_or_forward_atomic::<IdentityPtrPolicy>(old_address as *mut u8)
                                as usize
                        }
                    })
                })
                .collect();
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(results.iter().all(|&value| value == results[0]));
        assert_eq!(heap.to_space().used(), info.allocation_size(0));
        assert_eq!(
            unsafe { (results[0] as *mut u8).add(Full::SIZE).cast::<u64>().read() },
            42
        );
    }

    #[test]
    fn collector_access_stripes_order_opposite_pairs_and_deduplicate_collisions() {
        if bounded("collector_access_stripes_order_opposite_pairs_and_deduplicate_collisions") {
            return;
        }
        let locks = ObjectLocks::new();
        let left = 0x1000usize as *const u8;
        let right = 0x2000usize as *const u8;
        assert_ne!(ObjectLocks::index(left), ObjectLocks::index(right));
        let count = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for reverse in [false, true] {
                let locks = &locks;
                let count = &count;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        let (a, b) = if reverse {
                            (0x2000, 0x1000)
                        } else {
                            (0x1000, 0x2000)
                        };
                        let _guard = locks.pair(a as *const u8, b as *const u8);
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(count.load(Ordering::Relaxed), 2000);
        let _collision = locks.pair(left, left);
    }
}

#[cfg(test)]
#[path = "object_lock_storage_tests.rs"]
mod storage_tests;

#[cfg(test)]
impl Heap {
    pub(crate) fn collector_object_lock_backing_bytes(&self) -> usize {
        std::mem::size_of_val(&*self.object_locks.slots)
    }
}
