//! Checked host I/O. Native resources never escape a call; owned data is copied
//! into managed objects only after the blocking region has ended.
use super::{
    STR_DATA_OFF, Thread, alloc_string_from_bytes, alloc_with_published_frame, blocking_region,
    str_bytes,
};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const ARGS: u32 = 0;
pub const READ_TEXT: u32 = 1;
pub const READ_DIR: u32 = 2;
pub const KIND: u32 = 3;
pub const ABSOLUTE: u32 = 4;
pub const WRITE_ATOMIC: u32 = 5;
pub const STDERR: u32 = 6;
pub const STDOUT: u32 = 7;

#[derive(Debug, Default)]
struct Outcome {
    text: String,
    entries: Vec<String>,
    kind: i64,
}

fn error_code(error: &io::Error) -> i64 {
    match error.kind() {
        io::ErrorKind::NotFound => 1,
        io::ErrorKind::PermissionDenied => 2,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => 3,
        _ => 4,
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn utf8_path(path: PathBuf) -> io::Result<String> {
    path.into_os_string()
        .into_string()
        .map_err(|_| invalid("path is not UTF-8"))
}
fn absolute(path: &Path) -> io::Result<PathBuf> {
    // Preserve `..`: eliminating it lexically changes meaning after a symlink.
    std::path::absolute(path)
}

/// Write and sync a sibling temporary before an atomic rename. A failed write
/// before rename preserves the old destination and removes the temporary. Symlinks are never
/// followed for the destination. Syncing the parent makes the rename durable.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = absolute(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("destination has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("destination has no file name"))?;
    let permissions = match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => Some(meta.permissions()),
        Ok(_) => None,
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mut attempt = 0;
    let (temp, mut file) = loop {
        let mut temp_name = name.to_os_string();
        temp_name.push(format!(
            ".tmp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = parent.join(temp_name);
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 100 => {
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn perform(op: u32, path: &str, text: &str, limit: i64) -> io::Result<Outcome> {
    if path.contains('\0') {
        return Err(invalid("path contains a NUL byte"));
    }
    let mut out = Outcome::default();
    match op {
        ARGS => {
            out.entries = std::env::args_os()
                .map(|arg| {
                    arg.into_string()
                        .map_err(|_| invalid("argument is not UTF-8"))
                })
                .collect::<io::Result<_>>()?;
        }
        READ_TEXT => {
            let limit =
                u64::try_from(limit).map_err(|_| invalid("read limit must be nonnegative"))?;
            let file = File::open(path)?;
            if !file.metadata()?.is_file() {
                return Err(invalid("expected a regular file"));
            }
            let mut reader = BufReader::new(file).take(
                limit
                    .checked_add(1)
                    .ok_or_else(|| invalid("read limit overflow"))?,
            );
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            drop(reader); // closes the file before returning or allocating managed data
            if bytes.len() as u64 > limit {
                return Err(invalid("file exceeds read limit"));
            }
            out.text = String::from_utf8(bytes).map_err(|_| invalid("file is not UTF-8"))?;
        }
        READ_DIR => {
            let max =
                usize::try_from(limit).map_err(|_| invalid("entry limit must be nonnegative"))?;
            let entries = fs::read_dir(path)?;
            for entry in entries {
                if out.entries.len() == max {
                    return Err(invalid("directory exceeds entry limit"));
                }
                out.entries.push(
                    entry?
                        .file_name()
                        .into_string()
                        .map_err(|_| invalid("directory entry is not UTF-8"))?,
                );
            }
            out.entries.sort();
        }
        KIND => {
            let meta = fs::symlink_metadata(path)?;
            out.kind = if meta.is_file() {
                1
            } else if meta.is_dir() {
                2
            } else if meta.file_type().is_symlink() {
                3
            } else {
                4
            };
        }
        ABSOLUTE => {
            out.text = utf8_path(absolute(Path::new(path))?)?;
        }
        WRITE_ATOMIC => {
            write_atomic(Path::new(path), text)?;
        }
        STDERR => {
            let mut stderr = io::stderr().lock();
            stderr.write_all(text.as_bytes())?;
            stderr.write_all(b"\n")?;
        }
        STDOUT => {
            let mut stdout = io::stdout().lock();
            stdout.write_all(text.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
        _ => return Err(invalid("unknown host I/O operation")),
    }
    Ok(out)
}

/// Managed ABI: IoResponse has two traced fields (text, entries), followed by
/// code and kind. Lowering validates this shape and supplies all three type IDs.
/// Every object under construction stays in scratch roots across allocations.
///
/// # Safety
/// Arguments are live Strings; layout IDs describe IoResponse/String/Array<String>.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_host_call(
    thread: *mut Thread,
    op: u32,
    response_id: u32,
    string_id: u32,
    array_id: u32,
    path: *const u8,
    text: *const u8,
    limit: i64,
) -> *mut u8 {
    unsafe {
        // Copy before publishing BLOCKED; borrowed managed bytes cannot outlive
        // a GC by another mutator while the filesystem call is in progress.
        let path = std::str::from_utf8(str_bytes(path)).map(str::to_owned);
        let text = std::str::from_utf8(str_bytes(text)).map(str::to_owned);
        let result = match (path, text) {
            (Ok(path), Ok(text)) => {
                let arguments = (*thread).arguments.clone();
                blocking_region(thread, || {
                    if op == ARGS {
                        let entries = arguments.iter().cloned().map(|arg| arg.into_string().map_err(|_| invalid("argument is not UTF-8"))).collect::<io::Result<Vec<_>>>()?;
                        Ok(Outcome { entries, ..Outcome::default() })
                    } else { perform(op, &path, &text, limit) }
                })
            },
            _ => Err(invalid("host I/O input is not UTF-8")),
        };
        let (out, code) = match result {
            Ok(out) => (out, 0),
            Err(e) => (
                Outcome {
                    text: e.to_string(),
                    ..Outcome::default()
                },
                error_code(&e),
            ),
        };
        let t = &*thread;
        let dyna = &*t.dyna_thread;
        let heap = &*t.heap;
        let mark = dyna.scratch_mark();
        let message = alloc_string_from_bytes(thread, string_id, out.text.as_bytes());
        let message_slot = dyna.push_scratch(message);
        let array = alloc_with_published_frame(
            t,
            heap,
            heap.type_info_by_id(array_id as u16),
            out.entries.len(),
        );
        *(array.add(16) as *mut u64) = out.entries.len() as u64;
        let array_slot = dyna.push_scratch(array);
        for (index, entry) in out.entries.iter().enumerate() {
            let value = alloc_string_from_bytes(thread, string_id, entry.as_bytes());
            let array = dyna.scratch_at(array_slot) as *mut u8;
            *(array.add(STR_DATA_OFF + index * 8) as *mut *mut u8) = value;
            super::ai_gc_write_barrier(thread, array, value);
        }
        let response =
            alloc_with_published_frame(t, heap, heap.type_info_by_id(response_id as u16), 0);
        *(response.add(16) as *mut *const u8) = dyna.scratch_at(message_slot);
        *(response.add(24) as *mut *const u8) = dyna.scratch_at(array_slot);
        *(response.add(32) as *mut i64) = code;
        *(response.add(40) as *mut i64) = out.kind;
        super::ai_gc_write_barrier(thread, response, dyna.scratch_at(message_slot) as *mut u8);
        super::ai_gc_write_barrier(thread, response, dyna.scratch_at(array_slot) as *mut u8);
        dyna.scratch_reset(mark);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_read_and_atomic_replacement() {
        let dir = std::env::temp_dir().join(format!("gcr-io-unit-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("text");
        write_atomic(&path, "old").unwrap();
        assert!(perform(READ_TEXT, path.to_str().unwrap(), "", 2).is_err());
        write_atomic(&path, "new\nλ").unwrap();
        assert_eq!(
            perform(READ_TEXT, path.to_str().unwrap(), "", 100)
                .unwrap()
                .text,
            "new\nλ"
        );
        let entries = perform(READ_DIR, dir.to_str().unwrap(), "", 100)
            .unwrap()
            .entries;
        assert_eq!(entries, ["text"]);
        assert!(write_atomic(&dir, "cannot replace directory").is_err());
        assert_eq!(
            perform(READ_DIR, dir.to_str().unwrap(), "", 100)
                .unwrap()
                .entries,
            ["text"]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            write_atomic(&path, "private").unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        for _ in 0..2048 {
            assert!(perform(READ_TEXT, path.to_str().unwrap(), "", 100).is_ok());
        }
        assert!(perform(READ_TEXT, dir.to_str().unwrap(), "", 100).is_err());
        assert!(perform(READ_TEXT, "bad\0path", "", 100).is_err());
        fs::write(&path, [0xff]).unwrap();
        assert_eq!(
            error_code(&perform(READ_TEXT, path.to_str().unwrap(), "", 100).unwrap_err()),
            3
        );
        #[cfg(unix)]
        {
            fs::create_dir_all(dir.join("a")).unwrap();
            fs::create_dir_all(dir.join("b/c")).unwrap();
            fs::write(dir.join("a/data"), "wrong").unwrap();
            fs::write(dir.join("b/data"), "right").unwrap();
            std::os::unix::fs::symlink(dir.join("b/c"), dir.join("a/link")).unwrap();
            let abs = absolute(&dir.join("a/link/../data")).unwrap();
            assert_eq!(
                perform(READ_TEXT, abs.to_str().unwrap(), "", 100)
                    .unwrap()
                    .text,
                "right"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
