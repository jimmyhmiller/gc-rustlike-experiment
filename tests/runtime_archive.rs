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
    std::fs::write(&source,"fn main()->i64 { let mut i=0; while i<100 { let s=\"λ\" + i.to_string(); i=i+1; } 0 }").unwrap();
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
fn older_custom_runtime_cannot_silently_link_projected_metadata_contract() {
    let fixture = Fixture::new("abi-version");
    let source = fixture.0.join("main.gcr");
    std::fs::write(&source, "fn main() -> i64 { 0 }").unwrap();
    let legacy = fixture.0.join("legacy.c");
    std::fs::write(&legacy, "long gcr_runtime_main(void*a,long b,void*c,long d,void*e){return 0;} long gcr_runtime_main_v2(void*a,long b,void*c,long d,void*e){return 0;} long gcr_runtime_main_v3(void*a,long b,void*c,long d,void*e){return 0;}").unwrap();
    let object = fixture.0.join("legacy.o");
    let archive = fixture.0.join("legacy.a");
    assert!(Command::new("clang").arg("-c").arg(&legacy).arg("-o").arg(&object).status().unwrap().success());
    assert!(Command::new("ar").arg("rcs").arg(&archive).arg(&object).status().unwrap().success());
    let result = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr"))
        .arg("build").arg(&source).arg("-o").arg(fixture.0.join("app"))
        .env("GCRUST_RUNTIME_LIB", &archive), 60);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("gcr_runtime_process_main_v4"),
        "{}", String::from_utf8_lossy(&result.stderr));
}

#[test]
fn polling_scans_require_versioned_runtime_symbols() {
    let fixture = Fixture::new("scan-abi");
    let source = fixture.0.join("main.gcr");
    std::fs::write(&source, r#"fn main() -> i64 { let args = process_args().expect("args"); let s = args[0]; let value = str_to_float(s); if str_eq(s, "abc") { str_hash(s) + value as i64 } else { 0 } }"#).unwrap();
    let legacy = fixture.0.join("legacy.c");
    std::fs::write(&legacy, "long gcr_runtime_process_main_v4(void*a,long b,void*c,long d,void*e){return 0;} long ai_str_hash(void*a,void*b){return 0;} long ai_str_eq(void*a,void*b,void*c){return 1;} double ai_str_to_float(void*a,void*b){return 0;}").unwrap();
    let object = fixture.0.join("legacy.o");
    let archive = fixture.0.join("legacy.a");
    assert!(Command::new("clang").arg("-c").arg(&legacy).arg("-o").arg(&object).status().unwrap().success());
    assert!(Command::new("ar").arg("rcs").arg(&archive).arg(&object).status().unwrap().success());
    let output = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr")).arg("build").arg(&source).arg("-o").arg(fixture.0.join("app")).env("GCRUST_RUNTIME_LIB", &archive),60);
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("ai_str_hash_v2"), "{diagnostic}");
    assert!(diagnostic.contains("ai_str_eq_v2"), "{diagnostic}");
    assert!(diagnostic.contains("ai_str_to_float_v2"), "{diagnostic}");
}

#[test]
fn polling_foreign_copies_require_versioned_runtime_symbols() {
    let fixture = Fixture::new("ffi-copy-abi");
    let source = fixture.0.join("main.gcr");
    std::fs::write(&source, r#"
        extern "C" fn strlen(text: RawPtr) -> i64;
        extern "C" fn memset(mut buf: RawPtr, value: i32, bytes: i64) -> RawPtr;
        fn main() -> i64 {
            let a: Array<i64> = Array::new(1);
            (unsafe { memset(as_c_bytes(a), 0, 8); strlen(as_c_bytes("text")) }) + a[0]
        }
    "#).unwrap();
    let legacy = fixture.0.join("legacy.c");
    std::fs::write(&legacy, "long gcr_runtime_process_main_v4(void*a,long b,void*c,long d,void*e){return 0;} long ai_str_copy_to_buf(void*a,void*b,void*c){return 0;} void ai_buf_copy_in(void*a,void*b,void*c,long d,long e){} void ai_buf_copy_out(void*a,void*b,void*c,long d,long e){}").unwrap();
    let object = fixture.0.join("legacy.o");
    let archive = fixture.0.join("legacy.a");
    assert!(Command::new("clang").arg("-c").arg(&legacy).arg("-o").arg(&object).status().unwrap().success());
    assert!(Command::new("ar").arg("rcs").arg(&archive).arg(&object).status().unwrap().success());
    let result = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr"))
        .arg("build").arg(&source).arg("-o").arg(fixture.0.join("app"))
        .env("GCRUST_RUNTIME_LIB", &archive), 60);
    assert!(!result.status.success());
    let diagnostic = String::from_utf8_lossy(&result.stderr);
    for symbol in ["ai_str_copy_to_buf_v2", "ai_buf_copy_in_v2", "ai_buf_copy_out_v2"] {
        assert!(diagnostic.contains(symbol), "missing {symbol}: {diagnostic}");
    }
}

#[test]
fn accounted_foreign_buffers_require_versioned_runtime_symbols() {
    let fixture = Fixture::new("ffi-buffer-abi");
    let source = fixture.0.join("main.gcr");
    std::fs::write(&source, r#"
        extern "C" fn strlen(text: RawPtr) -> i64;
        fn main() -> i64 { unsafe { strlen(as_c_bytes("text")) } }
    "#).unwrap();
    let legacy = fixture.0.join("legacy.c");
    std::fs::write(&legacy, "long gcr_runtime_process_main_v4(void*a,long b,void*c,long d,void*e){return 0;} void *ai_ffi_buffer_alloc_v1(long n){return 0;} void ai_ffi_buffer_free_v1(void*p,long n){} long ai_str_copy_to_buf_v2(void*a,void*b,void*c){return 0;}").unwrap();
    let object = fixture.0.join("legacy.o");
    let archive = fixture.0.join("legacy.a");
    assert!(Command::new("clang").arg("-c").arg(&legacy).arg("-o").arg(&object).status().unwrap().success());
    assert!(Command::new("ar").arg("rcs").arg(&archive).arg(&object).status().unwrap().success());
    let result = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr"))
        .arg("build").arg(&source).arg("-o").arg(fixture.0.join("app"))
        .env("GCRUST_RUNTIME_LIB", &archive), 60);
    assert!(!result.status.success());
    let diagnostic = String::from_utf8_lossy(&result.stderr);
    for symbol in ["ai_ffi_buffer_alloc_v2", "ai_ffi_buffer_free_v2", "ai_ffi_buffer_data_v2"] {
        assert!(diagnostic.contains(symbol), "missing {symbol}: {diagnostic}");
    }
}

#[test]
fn native_linker_honors_explicit_cc_and_reports_its_path() {
    let f = Fixture::new("explicit-cc");
    let source = f.0.join("main.gcr");
    let binary = f.0.join("app");
    let missing = f.0.join("missing linker with spaces");
    std::fs::write(&source, "fn main()->i64 { 42 }").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
    command.arg("build").arg(&source).arg("-o").arg(&binary)
        .env("CC", &missing).env_remove("GCR_CODEGEN_TSAN");
    let output = support::run_with_timeout(&mut command, 60);
    assert!(!output.status.success(), "AOT linking ignored the explicit CC");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to invoke linker"), "{stderr}");
    assert!(stderr.contains(missing.to_str().unwrap()), "{stderr}");
    assert!(!binary.exists());
    // An executable path with spaces must be passed whole, without splitting
    // it into a compiler name and accidental extra arguments.
    #[cfg(unix)]
    {
        let directory = f.0.join("linker directory with spaces");
        std::fs::create_dir_all(&directory).unwrap();
        let compiler = directory.join("cc");
        std::os::unix::fs::symlink("/usr/bin/cc", &compiler).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
        command.arg("build").arg(&source).arg("-o").arg(&binary)
            .env("CC", &compiler).env_remove("GCR_CODEGEN_TSAN");
        let output = support::run_with_timeout(&mut command, 60);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let output = support::run_with_timeout(&mut Command::new(&binary), 20);
        assert_eq!(output.status.code(), Some(42));
    }
}
