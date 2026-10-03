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
