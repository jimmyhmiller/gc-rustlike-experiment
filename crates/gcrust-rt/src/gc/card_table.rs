use std::sync::atomic::{AtomicU8, Ordering};

/// Card table for tracking old→young pointer writes.
///
/// Each "card" covers a fixed-size region (512 bytes) of tenured heap space.
/// When a mutator stores a nursery pointer into a tenured object, the
/// corresponding card is marked dirty. During minor GC, only dirty cards
/// need scanning for nursery references.

const CARD_SHIFT: usize = 9; // 512 bytes per card
const CARD_CLEAN: u8 = 0;
const CARD_DIRTY: u8 = 1;

pub struct CardTable {
    cards: Vec<AtomicU8>,
    base_addr: usize,
}

impl CardTable {
    /// Create a new card table covering `region_size` bytes starting at `base`.
    pub fn new(base: *const u8, region_size: usize) -> Self {
        let num_cards = (region_size + (1 << CARD_SHIFT) - 1) >> CARD_SHIFT;
        CardTable {
            cards: (0..num_cards).map(|_| AtomicU8::new(CARD_CLEAN)).collect(),
            base_addr: base as usize,
        }
    }

    /// Mark the card containing `addr` as dirty.
    ///
    /// Multiple mutators can dirty the same card concurrently. Atomic bytes
    /// avoid a native data race; redundant dirty stores are idempotent. Relaxed
    /// ordering suffices because STW root publication supplies the release/
    /// acquire edge before the collector scans cards and object fields.
    #[inline(always)]
    pub fn mark_dirty(&self, addr: *const u8) {
        let offset = addr as usize - self.base_addr;
        let card_idx = offset >> CARD_SHIFT;
        if card_idx < self.cards.len() {
            self.cards[card_idx].store(CARD_DIRTY, Ordering::Relaxed);
        }
    }

    /// Check if a specific card is dirty.
    pub fn is_dirty(&self, card_idx: usize) -> bool {
        card_idx < self.cards.len() && self.cards[card_idx].load(Ordering::Relaxed) == CARD_DIRTY
    }

    /// Clear all cards to clean state.
    ///
    /// Takes `&self` rather than `&mut self` because this is called during
    /// STW when we only have shared references. Safety: no mutators are
    /// running, so there are no concurrent accesses.
    pub fn clear_all(&self) {
        for card in &self.cards {
            card.store(CARD_CLEAN, Ordering::Relaxed);
        }
    }

    /// Iterate over dirty cards, yielding (card_index, card_start_address).
    pub fn iter_dirty(&self) -> impl Iterator<Item = (usize, *const u8)> + '_ {
        self.cards.iter().enumerate().filter_map(move |(idx, val)| {
            if val.load(Ordering::Relaxed) == CARD_DIRTY {
                let addr = self.base_addr + (idx << CARD_SHIFT);
                Some((idx, addr as *const u8))
            } else {
                None
            }
        })
    }

    /// Number of cards in the table.
    pub fn card_count(&self) -> usize {
        self.cards.len()
    }

    /// Size in bytes of the region covered by one card.
    pub fn card_size(&self) -> usize {
        1 << CARD_SHIFT
    }

    /// Base address this card table covers.
    pub fn base_addr(&self) -> usize {
        self.base_addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_marking_preserves_shared_and_distinct_dirty_cards() {
        // Addresses are only used to choose cards; no memory is dereferenced.
        let base = 0x1000usize;
        let table = CardTable::new(base as *const u8, 16 * 512);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let table = &table;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        table.mark_dirty(base as *const u8);
                        table.mark_dirty((base + (worker + 1) * 512) as *const u8);
                    }
                });
            }
        });
        let dirty: Vec<_> = table.iter_dirty().map(|(index, _)| index).collect();
        assert_eq!(dirty, (0..9).collect::<Vec<_>>());
        for index in 0..9 {
            assert!(table.is_dirty(index));
        }
        table.clear_all();
        assert_eq!(table.iter_dirty().count(), 0);
        for index in 0..table.card_count() {
            assert!(!table.is_dirty(index));
        }
    }
}
