//! Per-component native allocation requests, distinct from rounded capacity.
pub(super) fn growth_request(capacity: usize, required: usize) -> usize {
    if capacity > required {
        return 0;
    }
    required
        .checked_add(1)
        .and_then(|n| n.checked_mul(2))
        .expect("native registry capacity overflow")
}
pub(super) struct RegistryStage<T, const N: usize = 3> {
    requested: [usize; N],
    pub(super) tables: T,
}
impl<T, const N: usize> RegistryStage<T, N> {
    #[cfg(test)]
    pub(super) fn new(capacity: usize, tables: T) -> Self {
        Self::components([capacity; N], tables)
    }
    pub(super) fn components(requested: [usize; N], tables: T) -> Self {
        Self { requested, tables }
    }
    pub(super) fn within_budget(&self, live: [usize; N]) -> bool {
        self.requested
            .iter()
            .zip(live)
            .all(|(&request, count)| request <= count.saturating_add(1).saturating_mul(4))
    }
}
