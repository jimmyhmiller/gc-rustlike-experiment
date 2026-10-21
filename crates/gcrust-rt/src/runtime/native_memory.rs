//! Native buffer and registry pressure, independent of managed owner lifetime.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

pub(crate) const MIN_THRESHOLD: usize = 1 << 20;

pub(crate) struct NativeMemory {
    pub(crate) registry: Arc<super::allocation_counter::AllocationCounter>,
    registry_allocator: super::metadata_allocator::MetadataAllocator,
    live: AtomicUsize,
    peak: AtomicUsize,
    threshold: AtomicUsize,
    collections: AtomicUsize,
    pub(crate) pressure_service: Mutex<()>,
}
impl Default for NativeMemory {
    fn default() -> Self {
        let registry = Arc::default();
        let registry_allocator = super::metadata_allocator::MetadataAllocator::new(&registry);
        Self {
            registry,
            registry_allocator,
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            threshold: AtomicUsize::new(MIN_THRESHOLD),
            collections: AtomicUsize::new(0),
            pressure_service: Mutex::new(()),
        }
    }
}
impl NativeMemory {
    /// Cycle-local buffers borrow this owner; persistent buffers retain their own allocator.
    pub(crate) fn registry_allocator(&self) -> &super::metadata_allocator::MetadataAllocator {
        &self.registry_allocator
    }
    pub(crate) fn bytes(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }
    pub(crate) fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }
    pub(crate) fn collections(&self) -> usize {
        self.collections.load(Ordering::Acquire)
    }
    /// Separate ledgers retain their metric scopes. Saturation makes combined
    /// demand conservative when independent allocations approach address limits.
    pub(crate) fn pressure_bytes(&self) -> usize {
        self.bytes().saturating_add(self.registry.bytes())
    }
    pub(crate) fn needs_collection(&self, additional: usize) -> bool {
        let live = self.pressure_bytes();
        let demand = live.saturating_add(additional);
        let threshold = self.threshold.load(Ordering::Acquire);
        // A released spike must not set the budget for the remainder of a long
        // execution. Include this pending request so repeated large requests
        // retain their serviced headroom. This can only lower policy: demand
        // already over budget still requires collection. Concurrent checks may
        // conservatively remove headroom, never grant unserviced growth.
        let retained_threshold = demand.saturating_mul(2).max(MIN_THRESHOLD);
        if retained_threshold < threshold {
            self.threshold.fetch_min(retained_threshold, Ordering::AcqRel);
        }
        live != 0 && demand > threshold
    }
    /// Called under collector exclusion after a complete pressure collection.
    /// Include the pending request, so large live native allocations do not collect on
    /// every following small allocation. This is a growth trigger, not a cap.
    pub(crate) fn collected(&self, additional: usize) {
        self.threshold.store(self.threshold_for(additional), Ordering::Release);
        self.collections
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            })
            .unwrap();
    }
    /// Root execution has joined its children and drained native retirement.
    /// Retained native observers still count as live demand. This adjusts policy
    /// for the next entry without inventing a collection or resetting metrics.
    /// Caller must be BLOCKED and hold no collector/registry lock.
    pub(crate) fn finish_execution(&self) {
        // The floor cannot be lowered. Ordinary executions need no service lock.
        if self.threshold.load(Ordering::Acquire) == MIN_THRESHOLD { return; }
        let _pressure = self.pressure_service.lock().unwrap();
        // Remove obsolete headroom, but do not forgive unserviced retained
        // demand by raising the trigger without an actual pressure collection.
        self.threshold.fetch_min(self.threshold_for(0), Ordering::AcqRel);
    }
    fn threshold_for(&self, additional: usize) -> usize {
        self.pressure_bytes()
            .saturating_add(additional)
            .saturating_mul(2)
            .max(MIN_THRESHOLD)
    }
    pub(crate) fn charge(self: &Arc<Self>, bytes: usize) -> Charge {
        let old = self
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                live.checked_add(bytes)
            })
            .expect("native buffer accounting overflow");
        // The peak is monotone. Avoid a contended RMW for steady-state
        // allocations that cannot increase it.
        if old + bytes > self.peak.load(Ordering::Relaxed) {
            self.peak.fetch_max(old + bytes, Ordering::Relaxed);
        }
        Charge {
            memory: self.clone(),
            bytes,
        }
    }
}
pub(crate) struct Charge {
    memory: Arc<NativeMemory>,
    bytes: usize,
}
impl Charge {
    pub(super) fn metadata_allocator(&self) -> super::native_metadata::MetadataAllocator {
        super::native_metadata::MetadataAllocator::new(&self.memory.registry)
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        self.memory
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_sub(self.bytes)
            })
            .expect("native buffer accounting underflow");
    }
}

struct InlineSlots {
    values: Box<[std::sync::atomic::AtomicU64]>,
    charge: Option<Charge>,
}
enum Backing {
    Inline(InlineSlots),
    Deferred(super::native_retirement::Reservation),
}
pub(super) struct NativeSlots {
    backing: Option<Backing>,
}
impl NativeSlots {
    #[cfg(test)]
    pub(super) fn new(values: Box<[std::sync::atomic::AtomicU64]>) -> Self {
        Self { backing: Some(Backing::Inline(InlineSlots { values, charge: None })) }
    }
    pub(super) fn zeroed_reserved(capacity: usize, charge: Charge, retirement: &super::native_retirement::Retirement) -> Self {
        assert_eq!(capacity.checked_mul(std::mem::size_of::<std::sync::atomic::AtomicU64>()), Some(charge.bytes));
        // Every all-zero AtomicU64 is valid. Backing stays private until the
        // complete control enters the registry; no atomic observer exists yet.
        let values = unsafe { Box::<[std::sync::atomic::AtomicU64]>::new_zeroed_slice(capacity).assume_init() };
        let backing = if capacity * 8 > super::string_copy::COPY_BYTES {
            Backing::Deferred(retirement.reserve(values, Some(charge)))
        } else { Backing::Inline(InlineSlots { values, charge: Some(charge) }) };
        Self { backing: Some(backing) }
    }
    /// Synchronous explicit release, inside the caller's native BLOCKED body.
    pub(super) fn release(mut self) {
        match self.backing.take() {
            Some(Backing::Deferred(reservation)) => reservation.release(),
            inline => drop(inline),
        }
    }
    pub(super) fn retire_into(mut self, batch: &mut super::native_retirement::Batch) {
        match self.backing.take() {
            Some(Backing::Deferred(reservation)) => reservation.into_batch(batch),
            inline => drop(inline),
        }
    }
    pub(super) fn register(&mut self, memory: &Arc<NativeMemory>) {
        let (values, charge) = match self.backing.as_mut().unwrap() {
            Backing::Inline(slots) => (&*slots.values, &mut slots.charge),
            Backing::Deferred(reservation) => reservation.parts(),
        };
        let bytes = std::mem::size_of_val(values);
        if let Some(charge) = charge {
            assert!(Arc::ptr_eq(&charge.memory, memory), "native buffer belongs to another ledger");
            assert_eq!(charge.bytes, bytes);
        } else { *charge = Some(memory.charge(bytes)); }
    }
}
impl std::ops::Deref for NativeSlots {
    type Target = [std::sync::atomic::AtomicU64];
    fn deref(&self) -> &Self::Target {
        match self.backing.as_ref().unwrap() {
            Backing::Inline(slots) => &slots.values,
            Backing::Deferred(reservation) => reservation.values(),
        }
    }
}
impl Drop for NativeSlots {
    fn drop(&mut self) {
        match self.backing.take() {
            Some(Backing::Deferred(reservation)) => reservation.submit(),
            inline => drop(inline),
        }
    }
}

#[cfg(test)]
mod pressure_tests {
    use super::*;
    use crate::runtime::native_metadata::{self, MetadataAllocator};

    #[test]
    fn collector_native_pressure_execution_reset_preserves_retained_demand_and_metrics() {
        let memory = Arc::<NativeMemory>::default();
        let buffer = memory.charge(4 * MIN_THRESHOLD);
        let registry = native_metadata::vector::<u8>(
            MIN_THRESHOLD, MetadataAllocator::new(&memory.registry),
        );
        memory.collected(16 * MIN_THRESHOLD);
        let peak = memory.peak();
        let registry_peak = memory.registry.peak();
        memory.finish_execution();
        assert_eq!(memory.threshold.load(Ordering::Acquire), 10 * MIN_THRESHOLD);
        assert_eq!(memory.collections(), 1);
        assert_eq!(memory.peak(), peak);
        assert_eq!(memory.registry.peak(), registry_peak);
        assert!(!memory.needs_collection(1));
        drop(buffer);
        drop(registry);
        memory.finish_execution();
        assert_eq!(memory.threshold.load(Ordering::Acquire), MIN_THRESHOLD);
        assert_eq!(memory.collections(), 1);
        assert_eq!(memory.peak(), peak);
        assert_eq!(memory.registry.peak(), registry_peak);
        let unserviced = Arc::<NativeMemory>::default();
        let baseline = unserviced.charge(MIN_THRESHOLD);
        unserviced.collected(0);
        let retained = unserviced.charge(2 * MIN_THRESHOLD);
        assert!(unserviced.needs_collection(1));
        unserviced.finish_execution();
        assert!(unserviced.needs_collection(1));
        assert_eq!(unserviced.collections(), 1);
        drop(retained);
        drop(baseline);
    }

    #[test]
    fn collector_native_pressure_live_checks_lower_only_and_include_pending_request() {
        for registry_only in [false, true] {
            let memory = Arc::<NativeMemory>::default();
            let buffer;
            let registry;
            if registry_only {
                buffer = None;
                registry = Some(native_metadata::vector::<u8>(
                    8 * MIN_THRESHOLD, MetadataAllocator::new(&memory.registry),
                ));
            } else {
                buffer = Some(memory.charge(8 * MIN_THRESHOLD));
                registry = None;
            }
            memory.collected(0);
            let threshold = memory.threshold.load(Ordering::Acquire);
            let peak = memory.peak();
            let registry_peak = memory.registry.peak();
            assert!(!memory.needs_collection(1));
            assert_eq!(memory.threshold.load(Ordering::Acquire), threshold);
            drop(buffer);
            drop(registry);
            // A pending large replacement preserves its serviced budget even
            // while the old backing is gone; no empty collection is needed.
            assert!(!memory.needs_collection(8 * MIN_THRESHOLD));
            assert_eq!(memory.threshold.load(Ordering::Acquire), threshold);
            assert!(!memory.needs_collection(1));
            assert_eq!(memory.threshold.load(Ordering::Acquire), MIN_THRESHOLD);
            assert_eq!(memory.collections(), 1);
            assert_eq!(memory.peak(), peak);
            assert_eq!(memory.registry.peak(), registry_peak);
            let unserviced = memory.charge(2 * MIN_THRESHOLD);
            assert!(memory.needs_collection(0));
            assert_eq!(memory.threshold.load(Ordering::Acquire), MIN_THRESHOLD);
            assert!(memory.needs_collection(usize::MAX));
            assert_eq!(memory.threshold.load(Ordering::Acquire), MIN_THRESHOLD);
            drop(unserviced);
        }
    }

    #[test]
    fn collector_native_pressure_combines_ledgers_and_retunes_retained_demand() {
        let memory = Arc::<NativeMemory>::default();
        let buffer = memory.charge(MIN_THRESHOLD / 2);
        let registry = native_metadata::vector::<u8>(
            MIN_THRESHOLD / 2,
            MetadataAllocator::new(&memory.registry),
        );
        assert_eq!(memory.bytes(), MIN_THRESHOLD / 2);
        assert_eq!(memory.registry.bytes(), MIN_THRESHOLD / 2);
        assert_eq!(memory.pressure_bytes(), MIN_THRESHOLD);
        assert!(!memory.needs_collection(0));
        assert!(memory.needs_collection(1));
        memory.collected(4096);
        assert!(!memory.needs_collection(4096));
        assert_eq!(memory.threshold.load(Ordering::Acquire), 2 * (MIN_THRESHOLD + 4096));
        drop(registry);
        drop(buffer);
        assert_eq!(memory.pressure_bytes(), 0);
        assert!(!memory.needs_collection(usize::MAX));
        memory.collected(0);
        assert_eq!(memory.threshold.load(Ordering::Acquire), MIN_THRESHOLD);
    }
}
