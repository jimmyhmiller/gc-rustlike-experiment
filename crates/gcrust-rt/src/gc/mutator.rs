use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_MUTATOR_ID: AtomicU64 = AtomicU64::new(1);

use crate::gc::alloc::{alloc_obj, Alloc};
use crate::gc::header::ObjHeader;
use crate::gc::roots::RootSource;
use crate::gc::type_info::TypeInfo;

// ─── Root ───────────────────────────────────────────────────────────

/// Opaque handle to a GC root slot.
///
/// Identifies one generation of a slot in one mutator. `Copy` because it has no
/// ownership semantics — multiple copies refer to the same slot. The underlying
/// slot is updated in-place by the GC (forwarding pointers).
///
/// Invalidated when [`Mutator::restore`] removes its slot. All builds reject
/// stale and cross-mutator handles, including after a slot index is reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Root {
    owner: u64,
    index: usize,
    generation: u64,
}

// ─── GcRef ──────────────────────────────────────────────────────────

/// A lifetime-guarded snapshot of a rooted value.
///
/// Borrows `&'a Mutator`, preventing any `&mut Mutator` operations
/// (allocation, safepoint, restore) while this reference exists.
/// After any GC point, you must call [`Mutator::get`] again to re-read.
///
/// `Copy` so multiple reads in the same scope are fine — all invalidated
/// together when the borrow ends.
#[derive(Clone, Copy)]
pub struct GcRef<'a> {
    bits: u64,
    _borrow: PhantomData<&'a Mutator>,
}

impl<'a> GcRef<'a> {
    /// Get the raw `u64` bits.
    ///
    /// **Escape hatch**: if this encodes a heap pointer, the caller must
    /// root it before the next GC point.
    #[inline(always)]
    pub fn bits(self) -> u64 {
        self.bits
    }
}

// ─── RootScope ──────────────────────────────────────────────────────

/// Watermark token for scoped root cleanup.
///
/// Created by [`Mutator::save`], consumed by [`Mutator::restore`].
/// Identifies a surviving prefix of one mutator. Repeated restoration is valid
/// while that prefix survives; replacing its boundary invalidates the token.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct RootScope {
    owner: u64,
    watermark: usize,
    boundary_generation: u64,
}

struct RootSlot {
    value: Cell<u64>,
    generation: u64,
}

// ─── Mutator ────────────────────────────────────────────────────────

/// Safe wrapper over root storage.
///
/// Uses Rust's borrow checker to enforce GC safety:
/// - **`&self`** methods: read roots (`get`), update values (`set`), save scope
/// - **`&mut self`** methods: allocate, push new roots, restore scope, safepoint
///
/// Since `&self` and `&mut self` conflict, all [`GcRef`]s must be dropped
/// before any GC-triggering operation.
///
/// Allocation is decoupled from root management: the `alloc` method takes
/// any `&dyn Alloc`, so a Mutator can work with any allocator (BumpAllocator,
/// SemiSpace, Heap, etc.).
pub struct Mutator {
    owner: u64,
    next_generation: u64,
    roots: Vec<RootSlot>,
}

impl Mutator {
    /// Create a new mutator with empty root storage.
    pub fn new() -> Self {
        let owner = NEXT_MUTATOR_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("Mutator identity space exhausted");
        Mutator {
            owner,
            next_generation: 0,
            roots: Vec::new(),
        }
    }

    // ─── &mut self (structural changes / GC points) ─────────────

    /// Register a value as a GC root, returning a [`Root`] handle.
    ///
    /// Takes `&mut self` because `Vec::push` may reallocate the root
    /// storage's backing buffer, which would invalidate any outstanding
    /// `scan_roots` slot pointers.
    pub fn root(&mut self, bits: u64) -> Root {
        let index = self.roots.len();
        let generation = self
            .next_generation
            .checked_add(1)
            .expect("Mutator root generation space exhausted");
        self.roots.push(RootSlot {
            value: Cell::new(bits),
            generation,
        });
        self.next_generation = generation;
        Root {
            owner: self.owner,
            index,
            generation,
        }
    }

    /// Allocate a heap object, initialize its header, and auto-root it.
    ///
    /// Returns `Some(Root)` on success, `None` if the allocator is full.
    /// The root slot stores the raw pointer (`ptr as u64`), not a tagged value.
    /// Use [`Mutator::set`] to replace it with a tagged encoding if needed.
    ///
    /// Takes `&mut self` because it pushes a new root.
    ///
    /// # Safety
    /// `info` must accurately describe the object layout for header type `H`.
    pub unsafe fn alloc<H: ObjHeader>(
        &mut self,
        allocator: &dyn Alloc,
        info: &'static TypeInfo,
        varlen_len: usize,
    ) -> Option<Root> {
        let ptr = unsafe { alloc_obj::<H>(allocator, info, varlen_len) };
        if ptr.is_null() {
            return None;
        }
        let root = self.root(ptr as u64);
        Some(root)
    }

    /// Explicit GC trigger point (no-op for now).
    ///
    /// Takes `&mut self` to force all `GcRef`s to be dropped first,
    /// ensuring no stale pointers survive across a potential collection.
    pub fn safepoint(&mut self) {
        // Future: trigger GC here
    }

    /// Pop all roots pushed after the given scope watermark.
    ///
    /// Takes `&mut self` to prevent use while `GcRef`s are outstanding.
    /// Panics without changing roots for another mutator's token or a prefix
    /// that was removed and replaced. Tokens for surviving prefixes are reusable.
    pub fn restore(&mut self, scope: RootScope) {
        assert_eq!(
            scope.owner, self.owner,
            "Mutator::restore: scope belongs to another mutator"
        );
        assert!(
            scope.watermark <= self.roots.len(),
            "Mutator::restore: watermark {} > length {}",
            scope.watermark,
            self.roots.len()
        );
        let boundary = scope
            .watermark
            .checked_sub(1)
            .map(|i| self.roots[i].generation)
            .unwrap_or(0);
        assert_eq!(
            scope.boundary_generation, boundary,
            "Mutator::restore: scope prefix was removed and replaced"
        );
        self.roots.truncate(scope.watermark);
    }

    // ─── &self (reads / interior-mutable writes) ────────────────

    /// Read a root's current value, returning a [`GcRef`] that borrows `&self`.
    ///
    /// The `GcRef` prevents `&mut self` operations until dropped,
    /// forcing a re-read after any GC point.
    pub fn get(&self, root: &Root) -> GcRef<'_> {
        GcRef {
            bits: self.root_slot(root).get(),
            _borrow: PhantomData,
        }
    }

    /// Update a root's value.
    ///
    /// Uses interior mutability (`Cell`), so this takes `&self`.
    /// No GC, no reallocation — safe to call while other roots are read.
    pub fn set(&self, root: &Root, bits: u64) {
        self.root_slot(root).set(bits);
    }

    /// Snapshot the current root stack length for later [`restore`](Self::restore).
    pub fn save(&self) -> RootScope {
        RootScope {
            owner: self.owner,
            watermark: self.roots.len(),
            boundary_generation: self.roots.last().map(|slot| slot.generation).unwrap_or(0),
        }
    }

    fn root_slot(&self, root: &Root) -> &Cell<u64> {
        assert_eq!(
            root.owner, self.owner,
            "Mutator root belongs to another mutator"
        );
        let slot = self
            .roots
            .get(root.index)
            .expect("Mutator root was removed");
        assert_eq!(
            root.generation, slot.generation,
            "Mutator root slot was removed and reused"
        );
        &slot.value
    }

    /// Return the number of active root slots.
    pub fn root_count(&self) -> usize {
        self.roots.len()
    }
}

unsafe impl RootSource for Mutator {
    unsafe fn scan_roots(&self, visitor: &mut dyn FnMut(*mut u64)) {
        for slot in &self.roots {
            visitor(slot.value.as_ptr());
        }
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn reused_slots_reject_stale_reads_and_writes() {
        let mut mutator = Mutator::new();
        let prefix = mutator.root(7);
        let scope = mutator.save();
        let stale = mutator.root(11);
        mutator.restore(scope);
        let replacement = mutator.root(42);
        assert!(catch_unwind(AssertUnwindSafe(|| mutator.get(&stale))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| mutator.set(&stale, 99))).is_err());
        assert_eq!(mutator.get(&replacement).bits(), 42);
        assert_eq!(mutator.get(&prefix).bits(), 7);
        mutator.restore(scope);
        assert!(catch_unwind(AssertUnwindSafe(|| mutator.get(&replacement))).is_err());
    }

    #[test]
    fn cross_mutator_handles_and_scopes_are_rejected_without_changes() {
        let mut first = Mutator::new();
        let handle = first.root(7);
        let scope = first.save();
        let mut second = Mutator::new();
        let other = second.root(42);
        assert!(catch_unwind(AssertUnwindSafe(|| second.get(&handle))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| second.set(&handle, 99))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| second.restore(scope))).is_err());
        assert_eq!(second.root_count(), 1);
        assert_eq!(second.get(&other).bits(), 42);
    }

    #[test]
    fn scope_tokens_follow_their_prefix_across_repeated_restore_and_moves() {
        let mut original = Mutator::new();
        let empty = original.save();
        let first = original.root(7);
        let prefix = original.save();
        let mut moved = original;
        for n in 0..3 {
            moved.root(n);
            moved.restore(prefix);
            assert_eq!(moved.get(&first).bits(), 7);
        }
        moved.restore(empty);
        let replacement = moved.root(42);
        assert!(catch_unwind(AssertUnwindSafe(|| moved.restore(prefix))).is_err());
        assert_eq!(moved.get(&replacement).bits(), 42);
        unsafe {
            moved.scan_roots(&mut |slot| *slot += 1);
        }
        assert_eq!(moved.get(&replacement).bits(), 43);
        moved.restore(empty);
        assert_eq!(moved.root_count(), 0);
    }

    #[test]
    fn root_generation_exhaustion_never_wraps_or_changes_existing_slots() {
        let mut mutator = Mutator::new();
        mutator.next_generation = u64::MAX - 1;
        let last = mutator.root(42);
        assert!(catch_unwind(AssertUnwindSafe(|| mutator.root(99))).is_err());
        assert_eq!(mutator.root_count(), 1);
        assert_eq!(mutator.get(&last).bits(), 42);
    }
}
