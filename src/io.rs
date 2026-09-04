//! 文件读写、冲突检测、草稿恢复。

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::storage;

pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
pub const MAX_IMAGE_FILE_SIZE: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum ReadError {
    TooLarge { size: u64, limit: u64 },
    InvalidUtf8,
    Io(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
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
    let bytes = read_snapshot_checked(path)?;
    decode_markdown_bytes(&bytes)
}

pub fn decode_markdown_bytes(bytes: &[u8]) -> Result<String, ReadError> {
    let text = String::from_utf8(bytes.to_vec()).map_err(|_| ReadError::InvalidUtf8)?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

pub fn file_stamp(path: &Path) -> Result<FileStamp, ReadError> {
    let metadata = fs::metadata(path).map_err(|error| ReadError::Io(error.to_string()))?;
    check_size(metadata.len())?;
    Ok(FileStamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

pub fn read_snapshot_checked(path: &Path) -> Result<Vec<u8>, ReadError> {
    let mut file = fs::File::open(path).map_err(|error| ReadError::Io(error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| ReadError::Io(error.to_string()))?;
    check_size(metadata.len())?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| ReadError::Io(error.to_string()))?;
    Ok(bytes)
}

/// Read a bounded asset from an already-open file handle. Opening the file
/// before checking its metadata keeps the size check tied to the same inode,
/// avoiding a metadata/read race when previewing or exporting local images.
pub fn read_file_limited(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {limit} byte limit"),
        ));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {limit} byte limit"),
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq)]
pub enum SaveError {
    ExternalModified,
    Io(String),
}

fn save_lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document");
    path.with_file_name(format!(".{name}.markdown-editor.lock"))
}

fn acquire_save_lock(path: &Path) -> Result<fs::File, String> {
    let lock_path = save_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|error| error.to_string())?;
    lock.try_lock_exclusive()
        .map_err(|error| format!("document is already being saved: {error}"))?;
    Ok(lock)
}

pub fn save_with_conflict_check(
    path: &Path,
    text: &str,
    snapshot: &[u8],
) -> Result<Vec<u8>, SaveError> {
    let _lock = acquire_save_lock(path).map_err(SaveError::Io)?;
    let current = fs::read(path).map_err(|e| SaveError::Io(e.to_string()))?;
    if current.as_slice() != snapshot {
        return Err(SaveError::ExternalModified);
    }
    let bytes = text.as_bytes();
    storage::write_atomic(path, bytes).map_err(|e| SaveError::Io(e.to_string()))?;
    Ok(bytes.to_vec())
}

pub fn save_overwrite(path: &Path, text: &str) -> Result<Vec<u8>, String> {
    let _lock = acquire_save_lock(path)?;
    let bytes = text.as_bytes();
    storage::write_atomic(path, bytes).map_err(|e| e.to_string())?;
    Ok(bytes.to_vec())
}

pub fn read_snapshot(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

const DRAFT_SCHEMA_VERSION: u32 = 2;
const DRAFT_RETENTION_SECONDS: u64 = 30 * 24 * 60 * 60;
const DRAFT_SESSION_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftTab {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub text: String,
    pub saved_at_unix: u64,
    #[serde(default)]
    disk_snapshot_base64: String,
}

impl DraftTab {
    pub fn new(id: u64, path: Option<PathBuf>, text: String, disk_snapshot: &[u8]) -> Self {
        use base64::Engine as _;
        Self {
            id,
            path,
            text,
            saved_at_unix: storage::unix_timestamp(),
            disk_snapshot_base64: base64::engine::general_purpose::STANDARD.encode(disk_snapshot),
        }
    }

    pub fn disk_snapshot(&self) -> Option<Vec<u8>> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(&self.disk_snapshot_base64)
            .ok()
    }

    fn has_valid_snapshot_encoding(&self) -> bool {
        use base64::Engine as _;
        if self.path.is_none() && self.disk_snapshot_base64.is_empty() {
            return true;
        }
        !self.disk_snapshot_base64.is_empty()
            && base64::engine::general_purpose::STANDARD
                .decode(&self.disk_snapshot_base64)
                .is_ok_and(|snapshot| snapshot.len() as u64 <= MAX_FILE_SIZE + 3)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftSession {
    pub schema_version: u32,
    pub saved_at_unix: u64,
    pub active_tab_id: u64,
    pub tabs: Vec<DraftTab>,
}

impl DraftSession {
    pub fn new(active_tab_id: u64, tabs: Vec<DraftTab>) -> Self {
        Self {
            schema_version: DRAFT_SCHEMA_VERSION,
            saved_at_unix: storage::unix_timestamp(),
            active_tab_id,
            tabs,
        }
    }
}

#[derive(Debug, Deserialize)]
struct LegacyDraftEnvelope {
    schema_version: u32,
    saved_at_unix: u64,
    text: String,
}

pub fn draft_path() -> PathBuf {
    draft_path_for_window(None)
}

pub fn draft_path_for_window(window_id: Option<u32>) -> PathBuf {
    let file_name = window_id.map_or_else(
        || "draft.json".to_string(),
        |id| format!("draft-window-{id}.json"),
    );
    storage::config_dir().join("state").join(file_name)
}

fn legacy_draft_path() -> PathBuf {
    std::env::temp_dir().join("markdown-editor-draft.md")
}

fn save_draft_at(path: &Path, session: &DraftSession) -> std::io::Result<()> {
    if session.schema_version != DRAFT_SCHEMA_VERSION || session.tabs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "draft session has an invalid version or no tabs",
        ));
    }
    let mut ids = std::collections::HashSet::new();
    let invalid_tab = session.tabs.iter().any(|tab| {
        (tab.text.is_empty() && tab.path.is_none())
            || tab.text.len() as u64 > MAX_FILE_SIZE
            || !ids.insert(tab.id)
            || !tab.has_valid_snapshot_encoding()
    });
    if invalid_tab {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "draft session contains an invalid tab",
        ));
    }
    let bytes = serde_json::to_vec_pretty(session).map_err(std::io::Error::other)?;
    if bytes.len() as u64 > DRAFT_SESSION_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "draft session exceeds the storage limit",
        ));
    }
    storage::write_atomic(path, &bytes)
}

fn load_draft_at(path: &Path, now: u64) -> Option<DraftSession> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() == 0 || metadata.len() > DRAFT_SESSION_LIMIT {
        storage::quarantine_corrupt(path);
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let mut session: DraftSession = match serde_json::from_slice(&bytes) {
        Ok(session) => session,
        Err(_) => match serde_json::from_slice::<LegacyDraftEnvelope>(&bytes) {
            Ok(legacy)
                if legacy.schema_version == storage::STORAGE_SCHEMA_VERSION
                    && !legacy.text.is_empty()
                    && legacy.text.len() as u64 <= MAX_FILE_SIZE =>
            {
                let tab = DraftTab {
                    id: 1,
                    path: None,
                    text: legacy.text,
                    saved_at_unix: legacy.saved_at_unix,
                    disk_snapshot_base64: String::new(),
                };
                let migrated = DraftSession {
                    schema_version: DRAFT_SCHEMA_VERSION,
                    saved_at_unix: legacy.saved_at_unix,
                    active_tab_id: 1,
                    tabs: vec![tab],
                };
                let _ = save_draft_at(path, &migrated);
                migrated
            }
            _ => {
                storage::quarantine_corrupt(path);
                return None;
            }
        },
    };
    let invalid_version = session.schema_version != DRAFT_SCHEMA_VERSION;
    let invalid_time = session.saved_at_unix > now.saturating_add(24 * 60 * 60);
    if invalid_version || invalid_time {
        storage::quarantine_corrupt(path);
        return None;
    }
    if now.saturating_sub(session.saved_at_unix) > DRAFT_RETENTION_SECONDS {
        let _ = fs::remove_file(path);
        return None;
    }

    let original_count = session.tabs.len();
    let mut ids = std::collections::HashSet::new();
    session.tabs.retain(|tab| {
        (!tab.text.is_empty() || tab.path.is_some())
            && tab.text.len() as u64 <= MAX_FILE_SIZE
            && tab.saved_at_unix <= now.saturating_add(24 * 60 * 60)
            && now.saturating_sub(tab.saved_at_unix) <= DRAFT_RETENTION_SECONDS
            && ids.insert(tab.id)
            && tab.has_valid_snapshot_encoding()
    });
    if session.tabs.is_empty() {
        let _ = fs::remove_file(path);
        return None;
    }
    if !session
        .tabs
        .iter()
        .any(|tab| tab.id == session.active_tab_id)
    {
        session.active_tab_id = session.tabs[0].id;
    }
    if session.tabs.len() != original_count {
        session.saved_at_unix = session
            .tabs
            .iter()
            .map(|tab| tab.saved_at_unix)
            .max()
            .unwrap_or(session.saved_at_unix);
        let _ = save_draft_at(path, &session);
    }
    Some(session)
}

pub fn save_draft(session: &DraftSession) -> std::io::Result<()> {
    save_draft_for_window(None, session)
}

pub fn save_draft_for_window(
    window_id: Option<u32>,
    session: &DraftSession,
) -> std::io::Result<()> {
    save_draft_at(&draft_path_for_window(window_id), session)
}

pub fn load_draft() -> Option<DraftSession> {
    let path = draft_path();
    if let Some(parent) = path.parent() {
        storage::cleanup_sidecars(parent);
    }
    if path.exists() {
        return load_draft_at(&path, storage::unix_timestamp());
    }

    // One-time migration from releases that stored the draft in the OS temp directory.
    let legacy = legacy_draft_path();
    let bytes = fs::read(&legacy).ok()?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_FILE_SIZE {
        let _ = fs::remove_file(legacy);
        return None;
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            let _ = fs::remove_file(legacy);
            return None;
        }
    };
    let session = DraftSession::new(1, vec![DraftTab::new(1, None, text, &[])]);
    if save_draft(&session).is_ok() {
        let _ = fs::remove_file(legacy);
    }
    Some(session)
}

pub fn clear_draft_for_window(window_id: Option<u32>) {
    let _ = fs::remove_file(draft_path_for_window(window_id));
    if window_id.is_none() {
        let _ = fs::remove_file(legacy_draft_path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn 多窗口草稿使用彼此隔离的文件() {
        let primary = draft_path_for_window(None);
        let secondary = draft_path_for_window(Some(42));

        assert_ne!(primary, secondary);
        assert_eq!(primary.file_name().unwrap(), "draft.json");
        assert_eq!(secondary.file_name().unwrap(), "draft-window-42.json");
    }

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
    fn 有界文件读取拒绝超限资源() {
        let dir = temp_dir();
        let p = dir.join("asset.bin");
        fs::write(&p, b"12345").unwrap();
        assert_eq!(read_file_limited(&p, 5).unwrap(), b"12345");
        assert!(read_file_limited(&p, 4).is_err());
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
    fn 并发保存会被文件锁拒绝() {
        let dir = temp_dir();
        let p = dir.join("locked.md");
        let lock = acquire_save_lock(&p).unwrap();
        assert!(matches!(
            save_overwrite(&p, "blocked"),
            Err(error) if error.contains("already being saved")
        ));
        drop(lock);
        assert_eq!(save_overwrite(&p, "ok").unwrap(), b"ok");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 新文档可以直接创建并写入() {
        let dir = temp_dir();
        let p = dir.join("new.md");
        assert!(!p.exists());
        assert_eq!(
            save_overwrite(&p, "新文档"),
            Ok("新文档".as_bytes().to_vec())
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), "新文档");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 草稿保存和恢复() {
        let dir = temp_dir();
        let p = dir.join("draft.json");
        assert!(load_draft_at(&p, storage::unix_timestamp()).is_none());
        let session = DraftSession::new(
            2,
            vec![
                DraftTab::new(
                    1,
                    Some(PathBuf::from("one.md")),
                    "草稿一".to_string(),
                    b"base-one",
                ),
                DraftTab::new(2, None, "草稿二".to_string(), &[]),
            ],
        );
        save_draft_at(&p, &session).unwrap();
        let restored = load_draft_at(&p, storage::unix_timestamp()).unwrap();
        assert_eq!(restored.active_tab_id, 2);
        assert_eq!(restored.tabs.len(), 2);
        assert_eq!(restored.tabs[0].path, Some(PathBuf::from("one.md")));
        assert_eq!(restored.tabs[0].text, "草稿一");
        assert_eq!(restored.tabs[0].disk_snapshot().unwrap(), b"base-one");
        assert_eq!(restored.tabs[1].text, "草稿二");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn 损坏或过期草稿降级为无恢复() {
        let dir = temp_dir();
        let corrupt = dir.join("corrupt.json");
        fs::write(&corrupt, b"not-json").unwrap();
        assert!(load_draft_at(&corrupt, storage::unix_timestamp()).is_none());
        assert!(!corrupt.exists());

        let expired = dir.join("expired.json");
        let envelope = DraftSession {
            schema_version: DRAFT_SCHEMA_VERSION,
            saved_at_unix: 1,
            active_tab_id: 1,
            tabs: vec![DraftTab {
                id: 1,
                path: None,
                text: "old".to_string(),
                saved_at_unix: 1,
                disk_snapshot_base64: String::new(),
            }],
        };
        fs::write(&expired, serde_json::to_vec(&envelope).unwrap()).unwrap();
        assert!(load_draft_at(&expired, DRAFT_RETENTION_SECONDS + 2).is_none());
        assert!(!expired.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn 单份版本一草稿迁移为多标签会话() {
        let dir = temp_dir();
        let path = dir.join("draft.json");
        let legacy = serde_json::json!({
            "schema_version": 1,
            "saved_at_unix": storage::unix_timestamp(),
            "text": "旧版草稿"
        });
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let restored = load_draft_at(&path, storage::unix_timestamp()).unwrap();
        assert_eq!(restored.schema_version, DRAFT_SCHEMA_VERSION);
        assert_eq!(restored.tabs.len(), 1);
        assert_eq!(restored.tabs[0].text, "旧版草稿");
        let migrated: DraftSession = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(migrated.schema_version, DRAFT_SCHEMA_VERSION);
        let _ = fs::remove_dir_all(dir);
    }
}
