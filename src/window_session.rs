//! Last-window state persisted independently from unsaved draft contents.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const WINDOW_SESSION_SCHEMA_VERSION: u32 = 1;
const MAX_WINDOW_TABS: usize = 256;
const MAX_WINDOW_SESSION_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSession {
    pub paths: Vec<PathBuf>,
    pub active_path: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WindowSessionEnvelope {
    schema_version: u32,
    saved_at_unix: u64,
    session: WindowSession,
}

pub fn load(window_id: Option<u32>) -> Option<WindowSession> {
    load_at(&session_path(window_id))
}

pub fn save(window_id: Option<u32>, session: &WindowSession) -> io::Result<()> {
    save_at(&session_path(window_id), session)
}

pub fn clear(window_id: Option<u32>) {
    let _ = fs::remove_file(session_path(window_id));
}

fn session_path(window_id: Option<u32>) -> PathBuf {
    let file_name = window_id.map_or_else(
        || "window.json".to_string(),
        |id| format!("window-{id}.json"),
    );
    crate::storage::config_dir().join("state").join(file_name)
}

fn save_at(path: &Path, session: &WindowSession) -> io::Result<()> {
    if session.paths.is_empty() || session.paths.len() > MAX_WINDOW_TABS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "window session has an invalid tab count",
        ));
    }
    let unique = session
        .paths
        .iter()
        .collect::<std::collections::HashSet<_>>();
    if unique.len() != session.paths.len()
        || session
            .active_path
            .as_ref()
            .is_some_and(|active| !unique.contains(active))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "window session contains invalid paths",
        ));
    }
    let envelope = WindowSessionEnvelope {
        schema_version: WINDOW_SESSION_SCHEMA_VERSION,
        saved_at_unix: crate::storage::unix_timestamp(),
        session: session.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_WINDOW_SESSION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "window session exceeds the storage limit",
        ));
    }
    crate::storage::write_atomic(path, &bytes)
}

fn load_at(path: &Path) -> Option<WindowSession> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() == 0 || metadata.len() > MAX_WINDOW_SESSION_BYTES {
        crate::storage::quarantine_corrupt(path);
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let envelope: WindowSessionEnvelope = match serde_json::from_slice(&bytes) {
        Ok(envelope) => envelope,
        Err(_) => {
            crate::storage::quarantine_corrupt(path);
            return None;
        }
    };
    let session = envelope.session;
    let unique = session
        .paths
        .iter()
        .collect::<std::collections::HashSet<_>>();
    let invalid = envelope.schema_version != WINDOW_SESSION_SCHEMA_VERSION
        || session.paths.is_empty()
        || session.paths.len() > MAX_WINDOW_TABS
        || unique.len() != session.paths.len()
        || session
            .active_path
            .as_ref()
            .is_some_and(|active| !unique.contains(active));
    if invalid {
        crate::storage::quarantine_corrupt(path);
        return None;
    }
    Some(session)
}

impl WindowSession {
    pub fn new(paths: Vec<PathBuf>, active_path: Option<PathBuf>) -> Self {
        Self { paths, active_path }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 窗口会话往返保留路径顺序和活动文件() {
        let directory = std::env::temp_dir().join(format!(
            "markdown-editor-window-session-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let state_path = directory.join("window.json");
        let session = WindowSession::new(
            vec![
                PathBuf::from("C:/笔记/一.md"),
                PathBuf::from("C:/notes/two.md"),
            ],
            Some(PathBuf::from("C:/notes/two.md")),
        );

        save_at(&state_path, &session).unwrap();
        let restored = load_at(&state_path).unwrap();

        assert_eq!(restored, session);
        let _ = std::fs::remove_dir_all(directory);
    }
}
