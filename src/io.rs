//! 文件读写、冲突检测、草稿恢复。

use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum ReadError {
    TooLarge { size: u64, limit: u64 },
    InvalidUtf8,
    Io(String),
}

pub fn check_size(size: u64) -> Result<(), ReadError> {
    if size > MAX_FILE_SIZE {
        Err(ReadError::TooLarge {
            size,
            limit: MAX_FILE_SIZE,
        })
    } else {
        Ok(())
    }
}

pub fn read_markdown(path: &Path) -> Result<String, ReadError> {
    let meta = fs::metadata(path).map_err(|e| ReadError::Io(e.to_string()))?;
    check_size(meta.len())?;
    let bytes = fs::read(path).map_err(|e| ReadError::Io(e.to_string()))?;
    let text = String::from_utf8(bytes).map_err(|_| ReadError::InvalidUtf8)?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub enum SaveError {
    ExternalModified,
    Io(String),
}

pub fn save_with_conflict_check(
    path: &Path,
    text: &str,
    snapshot: &[u8],
) -> Result<Vec<u8>, SaveError> {
    let current = fs::read(path).map_err(|e| SaveError::Io(e.to_string()))?;
    if current.as_slice() != snapshot {
        return Err(SaveError::ExternalModified);
    }
    let bytes = text.as_bytes();
    fs::write(path, bytes).map_err(|e| SaveError::Io(e.to_string()))?;
    Ok(bytes.to_vec())
}

pub fn save_overwrite(path: &Path, text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    fs::write(path, bytes).map_err(|e| e.to_string())?;
    Ok(bytes.to_vec())
}

pub fn read_snapshot(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

pub fn draft_path() -> PathBuf {
    std::env::temp_dir().join("markdown-editor-draft.md")
}

pub fn save_draft(text: &str) -> std::io::Result<()> {
    fs::write(draft_path(), text)
}

pub fn load_draft() -> Option<String> {
    let bytes = fs::read(draft_path()).ok()?;
    if bytes.is_empty() {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub fn clear_draft() {
    let _ = fs::remove_file(draft_path());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("md_editor_test_{}_{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn 大小边界检查() {
        assert!(check_size(9 * 1024 * 1024).is_ok());
        assert!(check_size(MAX_FILE_SIZE).is_ok());
        assert_eq!(
            check_size(MAX_FILE_SIZE + 1),
            Err(ReadError::TooLarge {
                size: MAX_FILE_SIZE + 1,
                limit: MAX_FILE_SIZE
            })
        );
    }

    #[test]
    fn 损坏文件拒绝读取() {
        let dir = temp_dir();
        let p = dir.join("bad.md");
        fs::write(&p, [0xff, 0xfe, 0x00, 0x41]).unwrap();
        assert_eq!(read_markdown(&p), Err(ReadError::InvalidUtf8));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 超限文件拒绝打开() {
        let dir = temp_dir();
        let p = dir.join("big.md");
        fs::write(&p, vec![b'a'; (MAX_FILE_SIZE + 1024) as usize]).unwrap();
        assert!(matches!(read_markdown(&p), Err(ReadError::TooLarge { .. })));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 外部修改触发冲突() {
        let dir = temp_dir();
        let p = dir.join("doc.md");
        fs::write(&p, "v1").unwrap();
        let snapshot = read_snapshot(&p).unwrap();

        // 磁盘内容与快照一致，正常保存
        assert_eq!(
            save_with_conflict_check(&p, "v2", &snapshot),
            Ok(b"v2".to_vec())
        );

        // 外部修改磁盘，保存被拒绝
        fs::write(&p, "external").unwrap();
        assert_eq!(
            save_with_conflict_check(&p, "v3", &snapshot),
            Err(SaveError::ExternalModified)
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), "external");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 草稿保存和恢复() {
        let p = draft_path();
        let _ = fs::remove_file(&p);
        assert!(load_draft().is_none());
        save_draft("草稿内容").unwrap();
        assert_eq!(load_draft().as_deref(), Some("草稿内容"));
        clear_draft();
        assert!(load_draft().is_none());
    }
}
