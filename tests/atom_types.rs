//! Atom scalar contracts and malformed/unsupported-input diagnostics.
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;
use std::process::Command;
mod support;

#[test]
fn scalar_atoms_preserve_byte_and_bit_equality_in_jit_and_native() {
    let dir = std::env::temp_dir().join(format!("gcr-atom-types-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("gcr.toml"), "[package]\nname = \"atom-types\"\n").unwrap();
    std::fs::write(dir.join("src/main.gcr"), r#"
        fn main() -> i64 {
            let flag: Atom<bool> = Atom::new(false);
            let payload: Atom<i64> = Atom::new(0);
            let child = Thread::spawn(|| {
                payload.reset(81);
                if !flag.compare_and_set(false, true) { return 1; }
                0
            });
            while !flag.deref() {}
            if payload.deref() != 81 || child.join() != 0 { return 1; }
            if flag.compare_and_set(false, true) { return 2; }
            if flag.reset(false) || flag.deref() { return 3; }
            let byte: Atom<i8> = Atom::new(-128 as i8);
            if !byte.compare_and_set(-128 as i8, 127 as i8) || byte.deref() != 127 as i8 { return 4; }
            let word: Atom<u16> = Atom::new(65535 as u16);
            if !word.compare_and_set(65535 as u16, 0 as u16) || word.deref() != 0 as u16 { return 5; }
            let letter: Atom<char> = Atom::new('α');
            if !letter.compare_and_set('α', 'λ') || letter.deref() != 'λ' { return 6; }
            let f: Atom<f32> = Atom::new(1.5 as f32);
            if !f.compare_and_set(1.5 as f32, 2.5 as f32) || f.deref() != 2.5 as f32 { return 7; }
            let zero: Atom<f64> = Atom::new(-0.0);
            if zero.compare_and_set(0.0, 1.0) { return 8; }
            if !zero.compare_and_set(-0.0, 2.0) || zero.deref() != 2.0 { return 9; }
            let nan = 0.0 / 0.0;
            let f64atom: Atom<f64> = Atom::new(nan);
            let old = f64atom.deref();
            // CAS compares identical NaN bits, unlike floating-point ==.
            if !f64atom.compare_and_set(old, 3.0) || f64atom.deref() != 3.0 { return 10; }
            0
        }
    "#).unwrap();
    let lib = support::runtime_staticlib();
    let bin = dir.join("atom-types");
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .arg("build")
        .arg(&dir)
        .arg("-o")
        .arg(&bin)
        .env("GCRUST_RUNTIME_LIB", lib);
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
                Command::new(&bin)
            };
            cmd.env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                .env("GCR_GC_VERIFY", "1");
            let out = support::run_with_timeout(&mut cmd, 30);
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
fn unsupported_atoms_and_wrong_intrinsic_arity_are_frontend_errors() {
    for (source, message) in [
        ("fn main() -> i64 { atom_load(); 0 }", "takes 1 argument"),
        ("fn main() -> i64 { atom_cas(); 0 }", "takes 3 argument"),
        (
            "#[value] struct Pair { a: i64, b: i64 } fn main() -> i64 { let a: Atom<Pair> = Atom::new(Pair { a: 1, b: 2 }); a.deref(); 0 }",
            "inline aggregates",
        ),
        (
            "fn main() -> i64 { let a: Atom<()> = Atom::new(()); a.deref(); 0 }",
            "scalar or managed reference",
        ),
    ] {
        let (module, _) = parse_with_prelude(source).unwrap();
        let resolved = resolve_module(module).unwrap();
        let error = lower_program(&resolved.globals).unwrap_err();
        assert!(error.msg.contains(message), "{}", error.msg);
    }
}
