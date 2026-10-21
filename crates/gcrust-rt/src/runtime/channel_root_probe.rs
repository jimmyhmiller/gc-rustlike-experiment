//! Test-only observation of actual minor collection root enrollment.
use std::cell::Cell;

thread_local! {
    static MINOR_PASSES: Cell<Option<usize>> = const { Cell::new(None) };
}

pub(crate) fn start() {
    MINOR_PASSES.with(|passes| assert!(passes.replace(Some(0)).is_none()));
}

pub(crate) fn record(minor: bool) {
    if minor {
        MINOR_PASSES.with(|passes| {
            if let Some(count) = passes.get() {
                passes.set(Some(count + 1));
            }
        });
    }
}

pub(crate) fn finish() -> usize {
    MINOR_PASSES.with(|passes| passes.replace(None).expect("root probe was not started"))
}
