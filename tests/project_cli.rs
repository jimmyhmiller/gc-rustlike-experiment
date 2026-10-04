use std::path::PathBuf;
use std::process::{Command, Output};

struct Project(PathBuf);
impl Project {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("gcr-cli-{}-{name}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("gcr.toml"), "[package]\nname = \"fixture\"\n").unwrap();
        std::fs::write(
            dir.join("src/main.gcr"),
            r#"
            struct Cell { n: i64 }
            fn main() -> i64 {
                let mut i = 0;
                let mut total = 0;
                while i < 10 { let c = Cell { n: i }; total = total + c.n; i = i + 1; }
                total
            }
        "#,
        )
        .unwrap();
        Self(dir)
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
        cmd.current_dir(&self.0)
            .env_remove("GCR_GC_STRESS")
            .env("GCR_GC_STATS", "1");
        cmd
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn major_collections(output: &Output) -> usize {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find(|l| l.starts_with("gc-rust:") && l.ends_with("major collections"))
        .unwrap_or_else(|| panic!("missing collection statistics: {stderr}"));
    line.split_whitespace().nth(4).unwrap().parse().unwrap()
}

#[test]
fn project_run_and_standalone_native_binary_apply_gc_stress() {
    let p = Project::new("stress");
    let normal = p.command().arg("run").output().unwrap();
    assert_eq!(
        normal.status.code(),
        Some(45),
        "{}",
        String::from_utf8_lossy(&normal.stderr)
    );
    assert_eq!(major_collections(&normal), 0);
    let stress = p.command().args(["run", "--gc-stress"]).output().unwrap();
    assert_eq!(
        stress.status.code(),
        Some(45),
        "{}",
        String::from_utf8_lossy(&stress.stderr)
    );
    assert!(major_collections(&stress) >= 10);
    let bin = p.0.join("target/fixture");
    let standalone = Command::new(bin)
        .env("GCR_GC_STRESS", "1")
        .env("GCR_GC_STATS", "1")
        .output()
        .unwrap();
    assert_eq!(standalone.status.code(), Some(45));
    assert!(major_collections(&standalone) >= 10);
}

#[test]
fn bare_file_run_reports_invalid_discovered_manifest() {
    let p = Project::new("invalid");
    std::fs::write(p.0.join("gcr.toml"), "[package]\nname = 123\n").unwrap();
    let result = p.command().args(["run", "src/main.gcr"]).output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("gcr.toml"));
}

#[test]
fn relative_file_run_checks_manifest_above_current_directory() {
    let p = Project::new("relative-parent");
    std::fs::write(p.0.join("gcr.toml"), "[package]\nname = 123\n").unwrap();
    let result = p
        .command()
        .current_dir(p.0.join("src"))
        .args(["run", "main.gcr"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("gcr.toml"));
}

#[test]
fn program_arguments_are_not_compiler_options() {
    let p = Project::new("program-arguments");
    let source = std::fs::read_to_string(p.0.join("src/main.gcr")).unwrap();
    let source = source.replace("let mut i = 0;", r#"
        let args = process_args();
        let values = match args { Result::Ok(values) => values, Result::Err(_) => return 90 };
        if array_len(values) != 4 { return 91; }
        if !str_eq(opt_expect(array_get(values, 1), "missing argument"), "--link-arg") { return 92; }
        if !str_eq(opt_expect(array_get(values, 2), "missing argument"), "--gc-stress") { return 93; }
        if !str_eq(opt_expect(array_get(values, 3), "missing argument"), "-o") { return 94; }
        let mut i = 0;
    "#);
    std::fs::write(p.0.join("src/main.gcr"), source).unwrap();
    let output = p.command().args(["run", "--", "--link-arg", "--gc-stress", "-o"]).output().unwrap();
    assert_eq!(output.status.code(), Some(45), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(major_collections(&output), 0);
}

#[test]
fn jit_project_arguments_and_exit_status_match_native() {
    let p = Project::new("jit-arguments");
    std::fs::write(p.0.join("src/main.gcr"), r#"
    fn argument_score() -> i64 {
        let a = match process_args() { Result::Ok(v) => v, Result::Err(e) => return e.kind };
        if array_len(a) != 3 { return 91; }
        if !str_eq(opt_expect(array_get(a, 1), "arg"), "λ") { return 92; }
        if !str_eq(opt_expect(array_get(a, 2), "arg"), "--gc-stress") { return 93; }
        7
    }
    fn main() -> i64 { let worker = Thread::spawn(argument_score); argument_score() + worker.join() }
    "#).unwrap();
    for options in [vec!["run", "--", "λ", "--gc-stress"], vec!["run", "--jit", "--", "λ", "--gc-stress"], vec!["run", "--jit", "--gc-stress", "--", "λ", "--gc-stress"]] {
        let output = p.command().args(options).output().unwrap();
        assert_eq!(output.status.code(), Some(14), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout.is_empty());
    }
    #[cfg(unix)] {
        use std::os::unix::ffi::OsStringExt;
        for mode in [vec!["run", "--"], vec!["run", "--jit", "--"]] {
            let output = p.command().args(mode).arg(std::ffi::OsString::from_vec(vec![0xff])).output().unwrap();
            assert_eq!(output.status.code(), Some(6), "{}", String::from_utf8_lossy(&output.stderr));
        }
    }
}

#[test]
fn build_validates_all_options_and_accepts_debug_in_either_order() {
    let p = Project::new("build-options");
    std::fs::create_dir_all(p.0.join("target")).unwrap();
    for flags in [vec!["build", "--debug", "-o", "target/debug-first"], vec!["build", "-o", "target/output-first", "--debug"]] {
        let output = p.command().args(flags).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    for flags in [vec!["build", "-o", "target/bad", "--unknown"], vec!["build", "-o", "target/bad", "-o", "target/duplicate"], vec!["build", "--link-arg"]] {
        let output = p.command().args(flags).output().unwrap();
        assert!(!output.status.success());
        assert!(!p.0.join("target/bad").exists());
    }
    std::fs::write(p.0.join("gcr.toml"), "[package]\nname = \"fixture\"\n[link]\nlibs = [\"m\"]\n").unwrap();
    let output = p.command().args(["run", "--jit"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("native link configuration"));
}
