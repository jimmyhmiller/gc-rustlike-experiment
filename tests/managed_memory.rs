//! Managed SC slot and aggregate snapshot conformance. Explicit Sync assertions
//! let these fixtures test backend races before the remaining library/API audit
//! permits removing the transitional capture restrictions project-wide.
use std::process::Command;
mod support;

const SOURCE: &str = include_str!("fixtures/managed_memory.gcr");

#[test]
fn mixed_sc_and_racing_snapshots_survive_gc_in_jit_and_aot() {
    let dir = std::env::temp_dir().join(format!("gcr-managed-memory-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("gcr.toml"),
        "[package]\nname = \"managed-memory\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.gcr"), SOURCE).unwrap();
    let executable = dir.join("managed-memory");
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .arg("build")
        .arg(&dir)
        .arg("-o")
        .arg(&executable)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let out = support::run_with_timeout(&mut build, 60);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for jit in [false, true] {
        for stress in [false, true] {
            let mut cmd = if jit {
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
                cmd.arg("run").arg(&dir).arg("--jit");
                cmd
            } else {
                Command::new(&executable)
            };
            cmd.env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                .env("GCR_GC_VERIFY", "1")
                .env("GCR_GC_WORKERS", "4")
                .env("GCR_NURSERY_MB", "1")
                .env("GCR_TENURED_MB", "8");
            let out = support::run_with_timeout(&mut cmd, 60);
            assert_eq!(
                out.status.code(),
                Some(0),
                "jit={jit} stress={stress}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn generated_shared_accesses_remain_atomic_and_snapshot_regions_cannot_collect() {
    use gcrust::compile::parse_with_prelude;
    use gcrust::lower::lower_program;
    use gcrust::resolve::resolve_module;
    let (module, _) = parse_with_prelude(SOURCE).unwrap();
    let resolved = resolve_module(module).unwrap();
    let program = lower_program(&resolved.globals).unwrap();
    for optimize in [false, true] {
        let ir = gcrust::codegen::emit_llvm_ir(&program, optimize).unwrap();
        assert!(ir.contains("load atomic i64") && ir.contains("store atomic i64"));
        assert!(ir.contains("load atomic i8") && ir.contains("store atomic i8"));
        assert!(ir.contains("load atomic double") && ir.contains("store atomic double"));
        let mut held = false;
        let mut snapshots = 0;
        for line in ir.lines() {
            if line.contains("call ptr @ai_managed_lock(") {
                assert!(!held, "nested generated aggregate region");
                held = true;
                snapshots += 1;
            } else if line.contains("call void @ai_managed_unlock(") {
                assert!(held, "unpaired generated aggregate release");
                held = false;
            } else if held && line.contains("call ") {
                assert!(
                    line.contains("@ai_gc_write_barrier("),
                    "unexpected call in snapshot region: {line}"
                );
            }
            if line.contains(" = load ") && line.contains("%fld") && !line.contains("%fld.snapshot")
            {
                assert!(
                    line.contains("load atomic"),
                    "non-atomic mutable field: {line}"
                );
            }
        }
        assert!(
            !held && snapshots > 0,
            "missing/unbalanced aggregate regions"
        );
    }
}
