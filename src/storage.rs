//! Application-owned persistent storage and recovery helpers.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const STORAGE_SCHEMA_VERSION: u32 = 1;
pub const CORRUPT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(0);

pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(path) = std::env::var_os("APPDATA") {
        return PathBuf::from(path).join("Markdown Editor");
    }

    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Markdown Editor");
    }

    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("markdown-editor");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("markdown-editor");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".markdown-editor")
}

pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "storage path has no parent"))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let operation_id = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = parent.join(format!(".{file_name}.{operation_id}.tmp"));
    if let Err(error) = fs::write(&temporary, bytes) {
        // 写入失败同样不能把半成品临时文件留在用户目录里。
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    // A normal save should replace the destination in one filesystem
    // operation.  Moving the old file to a backup first leaves a brief window
    // where readers observe a missing path.  `install_file` uses the native
    // Windows replacement primitive and `rename` on Unix, both of which keep
    // the destination name continuously present when it already exists.
    if let Err(error) = install_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// Install a temporary file under its final name without exposing a missing
/// destination during replacement.
fn install_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        // `ReplaceFileW` atomically swaps an existing file.  If the target was
        // concurrently removed, fall back to a regular rename; if it was
        // concurrently created, retry the replacement so we do not silently
        // leave the temporary file behind.
        match replace_file_windows(temporary, destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::rename(temporary, destination) {
                    Ok(()) => Ok(()),
                    Err(rename_error) if rename_error.kind() == io::ErrorKind::AlreadyExists => {
                        replace_file_windows(temporary, destination)
                    }
                    Err(rename_error) => Err(rename_error),
                }
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // POSIX rename replaces an existing destination atomically.
        fs::rename(temporary, destination)
    }
}

/// Install a file only when the destination is still absent. This is used by
/// compare-and-swap saves after the original inode has been moved aside; an
/// external writer recreating the path must win with a conflict, never be
/// overwritten by the pending save.
fn install_file_without_replacing(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        // Windows `fs::rename` passes MOVEFILE_REPLACE_EXISTING and would let a
        // pending save silently clobber a file an external writer recreated
        // after the CAS backup was moved aside. Move without the replace flag
        // instead: a destination that reappeared wins with ERROR_ALREADY_EXISTS
        // and no bytes are overwritten.
        move_file_without_replacing(temporary, destination)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let result = fs::hard_link(temporary, destination);
        if result.is_ok() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

#[cfg(target_os = "windows")]
fn move_file_without_replacing(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // Flags 0: without MOVEFILE_REPLACE_EXISTING the call fails when the
    // destination already exists. SAFETY: both vectors are null-terminated
    // wide strings valid for the duration of the call, and the flag value is a
    // documented constant.
    let result = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn replace_file_windows(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
    }

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // REPLACEFILE_IGNORE_MERGE_ERRORS; no backup is requested.
    // SAFETY: both vectors are null-terminated wide strings valid for the
    // duration of the call, the null backup/exclude/reserved pointers are
    // permitted by ReplaceFileW, and the flag value is a documented constant.
    let result = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0x0000_0002,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Replace `path` atomically only when its contents still match `expected`.
///
/// This is an optimistic compare-and-swap used by document saves. The source
/// is verified immediately before replacement, so another editor change is
/// reported instead of silently overwriting the newer contents.
pub fn write_atomic_if_unchanged(path: &Path, expected: &[u8], bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "storage path has no parent"))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let operation_id = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = parent.join(format!(".{file_name}.{operation_id}.tmp"));
    let backup = parent.join(format!(".{file_name}.{operation_id}.bak"));
    // Move the exact destination inode to a private backup before checking it.
    // This closes the check/replace race: a concurrent writer either wins the
    // rename and is detected by the backup comparison, or runs after the new
    // file is installed and remains visible to the caller's watcher. The
    // strict cross-process CAS path necessarily has a tiny missing-name window
    // between these renames; normal saves use install_file and are gap-free.
    if fs::read(path)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "file changed while saving",
        ));
    }
    fs::write(&temporary, bytes).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })?;
    if let Err(error) = fs::rename(path, &backup) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let backup_matches = fs::read(&backup)
        .map(|contents| contents.as_slice() == expected)
        .unwrap_or(false);
    if !backup_matches {
        let _ = fs::remove_file(&temporary);
        if !path.exists() {
            let _ = fs::rename(&backup, path);
        } else {
            let _ = fs::remove_file(&backup);
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "file changed while saving",
        ));
    }
    if let Err(error) = install_file_without_replacing(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        if backup.exists() {
            if !path.exists() {
                let _ = fs::rename(&backup, path);
            } else {
                let _ = fs::remove_file(&backup);
            }
        }
        return Err(error);
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

pub fn quarantine_corrupt(path: &Path) {
    if !path.exists() {
        return;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let quarantine = path.with_file_name(format!("{file_name}.corrupt-{}", unix_timestamp()));
    if fs::rename(path, quarantine).is_err() {
        let _ = fs::remove_file(path);
    }
}

pub fn cleanup_sidecars(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.contains(".corrupt-") && !name.ends_with(".tmp") && !name.ends_with(".bak") {
            continue;
        }
        let is_expired = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= CORRUPT_RETENTION);
        if is_expired {
            let _ = fs::remove_file(path);
        }
    }
}

/// 文档目录的保存 sidecar 清扫。只匹配本应用产生的精确命名
/// `.文件名.操作号.tmp/.bak`，且只删超过冷却期（不可能有进程仍在写）的
/// 文件——用户自己的 `.bak`、其他工具的临时文件一概不碰。
pub fn cleanup_save_sidecars(directory: &Path, file_name: &str) {
    cleanup_save_sidecars_before(directory, file_name, SystemTime::now());
}

fn cleanup_save_sidecars_before(directory: &Path, file_name: &str, now: SystemTime) {
    const SIDECAR_COOLDOWN: Duration = Duration::from_secs(60);
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let prefix = format!(".{file_name}.");
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(operation_id) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some((middle, suffix)) = operation_id.rsplit_once('.') else {
            continue;
        };
        if !(suffix == "tmp" || suffix == "bak") {
            continue;
        }
        // 只匹配本应用生成的三段 `pid-纳秒-序号` 操作号。纳秒段自 1970 年起算
        // 恒为 16 位以上，用户自己的 `.doc.md.2026-09-17.bak`（日期）或
        // `.doc.md.备份.v2.bak` 之类同名文件不满足该形状，不会被清扫。
        let segments: Vec<&str> = middle.split('-').collect();
        let is_app_operation_id = segments.len() == 3
            && segments
                .iter()
                .all(|segment| !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()))
            && segments[1].len() >= 16;
        let is_stale = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= SIDECAR_COOLDOWN);
        if is_app_operation_id && is_stale {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn test_dir() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "markdown-editor-storage-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn atomic_write_replaces_previous_value_without_leaving_sidecar() {
        let directory = test_dir();
        let path = directory.join("state.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn 文档sidecar清扫只删本应用命名且过冷却期的文件() {
        let directory = test_dir();
        let stale_tmp = directory.join(".doc.md.123-1789212345678901234-1.tmp");
        let stale_bak = directory.join(".doc.md.123-1789212345678901234-2.bak");
        let foreign_bak = directory.join("doc.md.bak");
        let other_prefix = directory.join(".other.md.123-1789212345678901234-3.tmp");
        // 用户自己的备份恰好形如 `.doc.md.<日期>.bak`，不属于本应用
        // 三段数字操作号，不应被清扫。
        let user_dated_bak = directory.join(".doc.md.2026-09-17.bak");
        for path in [
            &stale_tmp,
            &stale_bak,
            &foreign_bak,
            &other_prefix,
            &user_dated_bak,
        ] {
            fs::write(path, b"x").unwrap();
        }
        // 以"一小时后"作为清扫时点：匹配本应用命名的全部过期被删。
        cleanup_save_sidecars_before(
            &directory,
            "doc.md",
            SystemTime::now() + Duration::from_secs(3600),
        );
        assert!(!stale_tmp.exists());
        assert!(!stale_bak.exists());
        assert!(foreign_bak.exists(), "用户自己的 .bak 不应被删除");
        assert!(other_prefix.exists(), "其他文件的 sidecar 不应被误删");
        assert!(
            user_dated_bak.exists(),
            "形如日期命名的用户 .bak 不应被删除"
        );
        // 冷却期：刚重建的同名临时文件不能被立即清扫（可能有进程正在写）。
        fs::write(&stale_tmp, b"x").unwrap();
        cleanup_save_sidecars_before(&directory, "doc.md", SystemTime::now());
        assert!(stale_tmp.exists(), "冷却期内的临时文件不能被并发清扫");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn compare_and_swap_save_rejects_changed_contents() {
        let directory = test_dir();
        let path = directory.join("note.md");
        fs::write(&path, b"before").unwrap();
        let error = write_atomic_if_unchanged(&path, b"stale", b"new").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"before");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn compare_and_swap_save_replaces_matching_contents_atomically() {
        let directory = test_dir();
        let path = directory.join("note.md");
        fs::write(&path, b"before").unwrap();
        write_atomic_if_unchanged(&path, b"before", b"after").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"after");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn install_without_replacing_fails_when_destination_exists() {
        let directory = test_dir();
        let temporary = directory.join("pending.txt");
        let destination = directory.join("note.md");
        fs::write(&temporary, b"new").unwrap();
        fs::write(&destination, b"external").unwrap();
        let error = install_file_without_replacing(&temporary, &destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"external",
            "外部重建的文件不能被待保存内容覆盖"
        );
        assert!(temporary.exists(), "临时文件应保留以便调用方回滚");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn install_without_replacing_installs_when_destination_absent() {
        let directory = test_dir();
        let temporary = directory.join("pending.txt");
        let destination = directory.join("note.md");
        fs::write(&temporary, b"new").unwrap();
        install_file_without_replacing(&temporary, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(!temporary.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn corrupt_state_is_quarantined_instead_of_loaded_again() {
        let directory = test_dir();
        let path = directory.join("state.json");
        fs::write(&path, b"broken").unwrap();
        quarantine_corrupt(&path);
        assert!(!path.exists());
        assert!(fs::read_dir(&directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("state.json.corrupt-")
        }));
        let _ = fs::remove_dir_all(directory);
    }
}
