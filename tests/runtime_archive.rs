//! AOT runtime production and linking share one archive owner.
use std::{path::PathBuf, process::Command};
#[allow(dead_code)]
mod support;
struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("gcr-archive-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn concurrent_native_builds_ignore_an_unrelated_cargo_target_dir() {
    let f = Fixture::new("concurrent");
    let source = f.0.join("main.gcr");
    std::fs::write(&source,"fn main()->i64 { let mut i=0; while i<100 { let s=str_concat(\"λ\",to_string(i)); i=i+1; } 0 }").unwrap();
    // This path deliberately is a file. The AOT producer must select its own
    // cache rather than honor a caller target directory its consumer won't use.
    let unrelated = f.0.join("unrelated-target");
    std::fs::write(&unrelated, "not a directory").unwrap();
    std::thread::scope(|scope| {
        for index in 0..4 {
            let source = &source;
            let unrelated = &unrelated;
            let bin = f.0.join(format!("app-{index}"));
            scope.spawn(move || {
                let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                command
                    .arg("build")
                    .arg(source)
                    .arg("-o")
                    .arg(&bin)
                    .env_remove("GCRUST_RUNTIME_LIB")
                    .env("CARGO_TARGET_DIR", unrelated);
                let output = support::run_with_timeout(&mut command, 60);
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let mut command = Command::new(&bin);
                command
                    .env("GCR_GC_STRESS", "1")
                    .env("GCR_GC_VERIFY", "1")
                    .env("GCR_GC_WORKERS", "4");
                let output = support::run_with_timeout(&mut command, 20);
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            });
        }
    });
    assert_eq!(
        std::fs::read_to_string(unrelated).unwrap(),
        "not a directory"
    );
}
#[cfg(unix)]
#[test]
fn runtime_build_failure_is_fatal_without_falling_back_to_an_existing_archive() {
    let f = Fixture::new("failure");
    let source = f.0.join("main.gcr");
    let bin = f.0.join("app");
    std::fs::write(&source, "fn main()->i64 { 0 }").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let cargo = f.0.join("failing-cargo");
    std::fs::write(
        &cargo,
        "#!/bin/sh\necho controlled runtime build failure >&2\nexit 23\n",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
    command
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&bin)
        .env_remove("GCRUST_RUNTIME_LIB");
    let primed = support::run_with_timeout(&mut command, 60);
    assert!(
        primed.status.success(),
        "{}",
        String::from_utf8_lossy(&primed.stderr)
    );
    let existing = std::fs::read(&bin).unwrap();
    command.env("CARGO", &cargo);
    let output = support::run_with_timeout(&mut command, 20);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("native runtime build failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&bin).unwrap(), existing);
}

#[test]
fn older_custom_runtime_cannot_silently_link_new_allocation_contract() {
    let fixture = Fixture::new("abi-version");
    let source = fixture.0.join("main.gcr");
    std::fs::write(&source, "fn main() -> i64 { 0 }").unwrap();
    let legacy = fixture.0.join("legacy.c");
    std::fs::write(&legacy, "long gcr_runtime_main(void*a,long b,void*c,long d,void*e){return 0;}").unwrap();
    let object = fixture.0.join("legacy.o");
    let archive = fixture.0.join("legacy.a");
    assert!(Command::new("clang").arg("-c").arg(&legacy).arg("-o").arg(&object).status().unwrap().success());
    assert!(Command::new("ar").arg("rcs").arg(&archive).arg(&object).status().unwrap().success());
    let result = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr"))
        .arg("build").arg(&source).arg("-o").arg(fixture.0.join("app"))
        .env("GCRUST_RUNTIME_LIB", &archive), 60);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("gcr_runtime_main_v2"),
        "{}", String::from_utf8_lossy(&result.stderr));
}
