//! Allocator-owned FIFO storage for Copy evacuation tasks.
//!
//! The vector is an initialized arena, not the logical queue. Removed Copy
//! values can remain in inactive slots; only the head/len interval is exposed.
//! Initial filling initializes slots once, reuse overwrites them, and growth
//! copies the live interval in FIFO order. No private VecDeque layout or
//! uninitialized-memory operations are needed.
use crate::runtime::metadata_allocator::MetadataAllocator;
use allocator_api2::{alloc::Allocator, vec::Vec as NativeVec};

pub(super) struct Ring<T: Copy, A: Allocator + Clone = MetadataAllocator> {
    arena: NativeVec<T, A>,
    head: usize,
    len: usize,
}

impl<T: Copy, A: Allocator + Clone> Ring<T, A> {
    fn with_capacity_in(capacity: usize, allocator: A) -> Self {
        Self {
            arena: NativeVec::with_capacity_in(capacity, allocator),
            head: 0,
            len: 0,
        }
    }

    pub(super) fn from_iter_in<I: IntoIterator<Item = T>>(values: I, allocator: A) -> Self {
        let values = values.into_iter();
        let mut ring = Self::with_capacity_in(values.size_hint().0, allocator);
        ring.extend(values);
        ring
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    // offset <= capacity; subtraction avoids head + offset overflow, even for
    // zero-sized elements whose vector capacity is usize::MAX.
    #[inline]
    fn index(&self, offset: usize) -> usize {
        let tail = self.arena.capacity() - self.head;
        if offset >= tail {
            offset - tail
        } else {
            self.head + offset
        }
    }

    fn as_slices(&self) -> (&[T], &[T]) {
        let first = self.len.min(self.arena.capacity() - self.head);
        (
            &self.arena[self.head..self.head + first],
            &self.arena[..self.len - first],
        )
    }

    #[inline]
    fn reserve(&mut self, additional: usize) {
        let required = self
            .len
            .checked_add(additional)
            .expect("collector work queue capacity overflow");
        let capacity = self.arena.capacity();
        if required <= capacity {
            return;
        }
        self.grow(required);
    }

    // Keep allocation and FIFO relinearization outside the ordinary capacity check.
    fn grow(&mut self, required: usize) {
        let capacity = self.arena.capacity();
        let grown = capacity
            .checked_mul(2)
            .expect("collector work queue capacity overflow")
            .max(4)
            .max(required);
        let mut arena = NativeVec::with_capacity_in(grown, self.arena.allocator().clone());
        let (first, second) = self.as_slices();
        arena.extend_from_slice(first);
        arena.extend_from_slice(second);
        let old = std::mem::replace(&mut self.arena, arena);
        self.head = 0;
        // Both physical allocations remain charged until the old arena frees.
        drop(old);
    }

    fn push_back(&mut self, value: T) {
        self.reserve(1);
        self.push_reserved(value);
    }

    #[inline]
    fn push_reserved(&mut self, value: T) {
        debug_assert!(self.len < self.arena.capacity());
        let index = self.index(self.len);
        if index == self.arena.len() {
            self.arena.push(value);
        } else {
            self.arena[index] = value;
        }
        self.len += 1;
    }

    // The initialized arena prefix grows contiguously. Wrapped writes first
    // finish that prefix, then overwrite already-initialized slots at zero.
    #[inline]
    fn write_at(&mut self, index: usize, values: &[T]) {
        debug_assert!(index <= self.arena.len());
        let existing = values.len().min(self.arena.len() - index);
        self.arena[index..index + existing].copy_from_slice(&values[..existing]);
        self.arena.extend_from_slice(&values[existing..]);
    }

    #[inline]
    pub(super) fn extend_from_slice(&mut self, values: &[T]) {
        if values.is_empty() {
            return;
        }
        self.reserve(values.len());
        let index = self.index(self.len);
        let first = values.len().min(self.arena.capacity() - index);
        self.write_at(index, &values[..first]);
        self.write_at(0, &values[first..]);
        self.len += values.len();
    }

    #[cfg(test)]
    fn pop_front(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        let value = self.arena[self.head];
        self.advance(1);
        Some(value)
    }

    fn advance(&mut self, count: usize) {
        self.head = self.index(count);
        self.len -= count;
        if self.is_empty() {
            self.head = 0;
        }
    }

    /// Copy one bounded batch before retiring its logical queue interval.
    pub(super) fn drain_into<B: Allocator>(&mut self, count: usize, output: &mut NativeVec<T, B>) {
        assert!(
            count <= self.len,
            "collector work queue drain exceeds length"
        );
        output.reserve(count);
        let first = count.min(self.arena.capacity() - self.head);
        output.extend_from_slice(&self.arena[self.head..self.head + first]);
        output.extend_from_slice(&self.arena[..count - first]);
        self.advance(count);
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &T> {
        let (first, second) = self.as_slices();
        first.iter().chain(second)
    }
}

impl<T: Copy, A: Allocator + Clone> Extend<T> for Ring<T, A> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, values: I) {
        let mut values = values.into_iter();
        let reserved = values.size_hint().0;
        self.reserve(reserved);
        for _ in 0..reserved {
            let Some(value) = values.next() else {
                return;
            };
            self.push_reserved(value);
        }
        // A conservative lower bound may leave additional values.
        for value in values {
            self.push_back(value);
        }
    }
}

#[cfg(test)]
impl<T: Copy> Ring<T> {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_in(capacity, MetadataAllocator::default())
    }
}

#[cfg(test)]
impl<T: Copy> FromIterator<T> for Ring<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        Self::from_iter_in(values, MetadataAllocator::default())
    }
}

#[cfg(test)]
impl<T: Copy, const N: usize> From<[T; N]> for Ring<T> {
    fn from(values: [T; N]) -> Self {
        values.into_iter().collect()
    }
}

#[cfg(test)]
impl<'a, T: Copy, A: Allocator + Clone> IntoIterator for &'a Ring<T, A> {
    type Item = &'a T;
    type IntoIter = std::iter::Chain<std::slice::Iter<'a, T>, std::slice::Iter<'a, T>>;
    fn into_iter(self) -> Self::IntoIter {
        let (first, second) = self.as_slices();
        first.iter().chain(second)
    }
}

#[cfg(test)]
mod tests {
    use allocator_api2::{vec, vec::Vec};
    use super::*;
    use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe};
    use std::{collections::VecDeque, sync::Arc};

    #[test]
    fn collector_work_ring_matches_fifo_model_across_seeded_wrap_and_growth() {
        for mut seed in [1u64, 17, 0x9e3779b97f4a7c15] {
            let counter = Arc::<AllocationCounter>::default();
            let mut ring = Ring::from_iter_in([], MetadataAllocator::new(&counter));
            let mut model = VecDeque::new();
            let mut output = Vec::with_capacity(33);
            let mut coverage = [0usize; 6];
            for step in 0..3000 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                match if model.len() > 512 {
                    2
                } else {
                    (seed >> 32) % 4
                } {
                    0 => {
                        coverage[0] += 1;
                        ring.push_back(seed);
                        model.push_back(seed);
                    }
                    1 => {
                        coverage[1] += 1;
                        assert_eq!(ring.pop_front(), model.pop_front());
                    }
                    2 => {
                        coverage[2] += 1;
                        let count = ((seed >> 16) as usize % 33).min(model.len());
                        output.clear();
                        output.push(u64::MAX);
                        ring.drain_into(count, &mut output);
                        assert!(
                            output
                                .iter()
                                .copied()
                                .eq(std::iter::once(u64::MAX).chain(model.drain(..count)))
                        );
                    }
                    _ => {
                        let count = (seed >> 16) as usize % 33;
                        let mut values = [0u64; 32];
                        for (i, value) in values[..count].iter_mut().enumerate() {
                            *value = seed.wrapping_add(i as u64);
                        }
                        if step % 2 == 0 {
                            coverage[4] += 1;
                            ring.extend_from_slice(&values[..count]);
                        } else if step % 3 == 0 {
                            coverage[5] += 1;
                            ring.extend(values[..count].iter().copied().filter(|_| true));
                        } else {
                            coverage[3] += 1;
                            ring.extend(values[..count].iter().copied());
                        }
                        model.extend(values[..count].iter().copied());
                    }
                }
                assert_eq!(ring.len(), model.len());
                assert_eq!(
                    counter.bytes(),
                    ring.arena.capacity() * std::mem::size_of::<u64>()
                );
                if step % 73 == 0 {
                    assert!(ring.iter().copied().eq(model.iter().copied()));
                }
            }
            assert!(ring.iter().copied().eq(model.iter().copied()));
            assert!(
                coverage.iter().all(|&count| count > 0),
                "missing FIFO operation coverage: {coverage:?}"
            );
            drop(ring);
            assert_eq!(counter.bytes(), 0);
        }
    }

    #[test]
    fn collector_work_ring_growth_preserves_wrapped_order_and_actual_backing() {
        let counter = Arc::<AllocationCounter>::default();
        let mut ring = Ring::from_iter_in([1u64, 2, 3, 4], MetadataAllocator::new(&counter));
        let old = counter.bytes();
        assert_eq!(ring.pop_front(), Some(1));
        assert_eq!(ring.pop_front(), Some(2));
        ring.extend([5, 6]);
        ring.push_back(7);
        assert!(ring.iter().copied().eq([3, 4, 5, 6, 7]));
        assert_eq!(counter.bytes(), ring.arena.capacity() * 8);
        assert_eq!(counter.peak(), old + counter.bytes());
        drop(ring);
        assert_eq!(counter.bytes(), 0);
    }

    #[test]
    fn collector_work_ring_warm_batches_allocate_nothing_and_release_owner() {
        let counter = Arc::<AllocationCounter>::default();
        let weak = Arc::downgrade(&counter);
        allocation_probe::start();
        let mut ring = Ring::with_capacity_in(8, MetadataAllocator::new(&counter));
        ring.extend(0u64..8);
        let cold = allocation_probe::finish_bytes();
        assert_eq!(counter.bytes(), cold.2 - cold.3);
        let mut output = Vec::with_capacity(4);
        let charged = counter.bytes();
        allocation_probe::start();
        for i in 0..64 {
            output.clear();
            ring.drain_into(4, &mut output);
            ring.extend((0..4).map(|j| i * 4 + j));
        }
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(counter.bytes(), charged);
        output.clear();
        ring.drain_into(ring.len(), &mut output);
        assert!(ring.is_empty());
        assert_eq!(
            counter.bytes(),
            charged,
            "empty logical queue still owns its backing"
        );
        drop(counter);
        assert!(weak.upgrade().is_some());
        drop(ring);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn collector_work_ring_invalid_drain_preserves_queue_and_destination() {
        let mut ring = Ring::from([1u64, 2]);
        let mut output = vec![99];
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || ring.drain_into(3, &mut output)
            ))
            .is_err()
        );
        assert!(ring.iter().copied().eq([1, 2]));
        assert_eq!(output, [99]);
    }

    #[test]
    fn collector_work_ring_zero_sized_tasks_preserve_counts() {
        let counter = Arc::<AllocationCounter>::default();
        let mut ring = Ring::from_iter_in([(); 17], MetadataAllocator::new(&counter));
        assert_eq!(ring.pop_front(), Some(()));
        let mut output = NativeVec::new();
        ring.drain_into(7, &mut output);
        ring.extend([(); 8]);
        assert_eq!(output.len(), 7);
        assert_eq!(ring.len(), 17);
        assert_eq!(ring.iter().count(), 17);
        assert_eq!(counter.bytes(), 0);
        drop(ring);
        assert_eq!(Arc::strong_count(&counter), 1);
    }
}

#[cfg(test)]
mod borrowed_allocator_tests {
    use super::*;
    use crate::runtime::allocation_counter::AllocationCounter;
    use std::sync::Arc;

    #[test]
    fn collector_work_ring_borrows_owner_across_wrap_growth_and_release() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let mut ring = Ring::from_iter_in([1u64, 2, 3, 4], &allocator);
        assert_eq!(Arc::strong_count(&counter), 2);
        assert_eq!(ring.pop_front(), Some(1));
        assert_eq!(ring.pop_front(), Some(2));
        ring.extend([5, 6]);
        ring.extend_from_slice(&[7, 8, 9]);
        assert!(ring.iter().copied().eq([3, 4, 5, 6, 7, 8, 9]));
        assert_eq!(counter.bytes(), ring.arena.capacity() * 8);
        assert_eq!(Arc::strong_count(&counter), 2);
        let mut output = NativeVec::new();
        ring.drain_into(ring.len(), &mut output);
        assert_eq!(output, [3, 4, 5, 6, 7, 8, 9]);
        assert!(counter.bytes() > 0);
        drop(ring);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 2);
        drop(allocator);
        assert_eq!(Arc::strong_count(&counter), 1);
    }
}
