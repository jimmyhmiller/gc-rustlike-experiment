//! Opt-in compiler phase timings, independent of generated-program output.
use std::time::Instant;

pub struct Phase {
    start: Option<Instant>,
    name: &'static str,
}

impl Phase {
    pub fn new(name: &'static str) -> Self {
        let setting = std::env::var_os("GCR_COMPILE_TIMINGS");
        let enabled = setting.as_ref().is_some_and(|value| value != "summary"
            || !matches!(name, "LLVM IR emission" | "LLVM module emission"));
        Self { start: enabled.then(Instant::now), name }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            eprintln!("gcr timing: {} {:.3} ms", self.name, start.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

pub fn measure<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    let _phase = Phase::new(name);
    f()
}
