//! Runs every program in `tests/cases`. Each one checks itself and returns 42.
//! Programs blocked on a known compiler gap are listed in `KNOWN_FAILURES`;
//! they must still fail, so fixing one forces this list to be updated.

use gcrust::codegen::jit_run_i64_gc;
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;

/// (file, reason) for programs that do not compile yet.
const KNOWN_FAILURES: &[(&str, &str)] = &[
    ("rec_option_struct.gcr", "LLVM verify failure in tail-call lowering of an Option-returning recursion"),
    ("tuples_arrays.gcr", "fixed-size array literals `[a, b]` / `[x; n]` are not lowered"),
];

fn run(src: &str) -> Result<i64, String> {
    let (module, _) = parse_with_prelude(src).map_err(|e| e.msg)?;
    let resolved = resolve_module(module).map_err(|e| e.msg)?;
    let prog = lower_program(&resolved.globals).map_err(|e| e.msg)?;
    jit_run_i64_gc(&prog, false).map_err(|e| e.0)
}

#[test]
fn every_case_returns_42() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".gcr"))
        .collect();
    names.sort();
    assert!(!names.is_empty());
    let mut problems = Vec::new();
    for name in &names {
        let src = std::fs::read_to_string(dir.join(name)).unwrap();
        let known = KNOWN_FAILURES.iter().find(|(n, _)| n == name);
        match (run(&src), known) {
            (Ok(42), None) => {}
            (Ok(v), None) => problems.push(format!("{name}: returned {v}, expected 42")),
            (Err(e), None) => problems.push(format!("{name}: {e}")),
            (Ok(_), Some((_, why))) => problems.push(format!("{name} now works; remove it from KNOWN_FAILURES ({why})")),
            (Err(_), Some(_)) => {}
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
