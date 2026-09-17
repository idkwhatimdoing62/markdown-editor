//! Renderer-independent document state.
//!
//! This module deliberately knows nothing about egui, windows, or file dialogs.
//! It owns the source revision and the last parsed, immutable document.  UI
//! code can keep editing the source while a worker prepares a newer parse; a
//! parse result is installable only when its revision still matches the
//! current source.

use std::sync::Arc;

use crate::markdown::{ParsedDocument, parse_document};

/// Monotonically increasing revision for a document source.
pub type Revision = u64;

/// State values that are meaningful to every document consumer, not just the
/// renderer.  The UI may choose how to present `SaveFailed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentStatus {
    Unsaved,
    Saved,
    Modified,
    Conflict,
    SaveFailed(String),
}

/// The mutable source together with the most recent immutable parse result.
///
/// `document_revision` identifies the current source. `parsed_revision` tells
/// consumers whether `document` has caught up.  Keeping both values is what
/// makes asynchronous parsing safe: an old worker result cannot silently
/// become the preview for newer text.
#[derive(Clone)]
pub struct DocumentState {
    /// Current UTF-8 source. Private on purpose: every mutation must either
    /// go through [`Self::set_source`] or pair an in-place edit via
    /// [`Self::source_mut`] with exactly one [`Self::mark_source_changed`],
    /// otherwise the revision check that keeps stale parses out stops
    /// working and the UI silently renders the previous AST.
    text: String,
    /// Last successfully parsed document. It is immutable once published.
    pub document: Arc<ParsedDocument>,
    /// Revision of [`Self::text`].
    pub document_revision: Revision,
    /// Revision represented by [`Self::document`].
    pub parsed_revision: Revision,
    /// Persistence state shared by the UI, autosave, and conflict handling.
    pub status: DocumentStatus,
}

impl DocumentState {
    /// Parse an initial source synchronously. Startup/open can use this while
    /// the worker boundary is introduced incrementally.
    pub fn from_source(text: String, revision: Revision) -> Self {
        let document = Arc::new(parse_document(&text));
        Self {
            text,
            document,
            document_revision: revision,
            parsed_revision: revision,
            status: DocumentStatus::Modified,
        }
    }

    pub fn is_parsed_current(&self) -> bool {
        // The revision is advanced by `set_source`/`mark_source_changed` and
        // copied into `parsed_revision` only after `install_parsed` validates
        // the worker result against the source.  Keeping this check O(1) is
        // important: it runs from the frame path for every open tab.
        let current = self.document_revision == self.parsed_revision;
        // Keep migration mistakes loud in debug/test builds without putting a
        // full-document comparison on the release frame path.
        debug_assert!(!current || self.document.source() == self.text.as_str());
        current
    }

    /// The most recent parse result, current or not.
    ///
    /// Rendering may keep using it while a newer revision is still being
    /// parsed: consumers must pair it with [`Self::is_parsed_current`] to know
    /// whether it describes `text`, and must not derive indices (block clicks,
    /// search targets) from it while it does not.
    pub fn document(&self) -> Arc<ParsedDocument> {
        Arc::clone(&self.document)
    }

    /// Read-only view of the current source.
    pub fn source(&self) -> &str {
        &self.text
    }

    /// In-place editing handle for the live text editor. The caller MUST
    /// call [`Self::mark_source_changed`] exactly once when the content
    /// actually changed; skipping it leaves the parse revision stale and the
    /// document renders the previous AST.
    pub fn source_mut(&mut self) -> &mut String {
        &mut self.text
    }

    /// Replace the source and advance its revision. The previous parse is
    /// intentionally retained until a matching worker result is installed.
    pub fn set_source(&mut self, text: String) -> bool {
        if self.text == text {
            return false;
        }
        self.text = text;
        self.mark_source_changed();
        true
    }

    /// Mark an already-mutated source as new. Call exactly once for each user
    /// edit (or external reload); the caller owns the source mutation.
    pub fn mark_source_changed(&mut self) -> Revision {
        self.document_revision = self.document_revision.wrapping_add(1);
        self.document_revision
    }

    /// Install a worker result only if it belongs to the current source.
    /// Returns `false` for stale or mismatched results.
    pub fn install_parsed(&mut self, revision: Revision, document: Arc<ParsedDocument>) -> bool {
        if revision != self.document_revision {
            return false;
        }
        // 修订号相等即证明结果属于当前源码；内容一致性只在调试构建里断言，
        // 发布版的每帧路径不做全文档 memcmp。
        debug_assert!(
            document.source() == self.text.as_str(),
            "解析结果与源码内容不一致"
        );
        self.document = document;
        self.parsed_revision = revision;
        true
    }

    /// Synchronous parse, bypassing the worker. Used for external reload and
    /// as the degradation path when the background worker is gone (spawn
    /// failed or the thread exited).
    pub fn reparse_current_source(&mut self) {
        self.document = Arc::new(parse_document(&self.text));
        self.parsed_revision = self.document_revision;
    }

    /// Produce a consistent immutable AST snapshot for consumers. A stale
    /// parse intentionally yields `None`; the source is already owned by the
    /// immutable `ParsedDocument`, so creating a snapshot does not copy the
    /// whole document.
    pub fn snapshot(&self, tab_id: u64) -> Option<DocumentSnapshot> {
        self.is_parsed_current().then(|| DocumentSnapshot {
            tab_id,
            revision: self.document_revision,
            document: Arc::clone(&self.document),
        })
    }
}

/// Immutable, renderer-neutral view of one document revision.
#[derive(Clone)]
pub struct DocumentSnapshot {
    pub tab_id: u64,
    pub revision: Revision,
    pub document: Arc<ParsedDocument>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 修改源码只推进源码版本并保留旧解析结果() {
        let mut state = DocumentState::from_source("# old".to_string(), 1);
        assert!(state.is_parsed_current());

        state.text = "# new".to_string();
        let revision = state.mark_source_changed();

        assert_eq!(revision, 2);
        assert_eq!(state.parsed_revision, 1);
        assert!(!state.is_parsed_current());
        assert_eq!(state.document.source(), "# old");
        assert!(state.snapshot(7).is_none());
    }

    #[test]
    fn 旧解析结果不能覆盖新版本() {
        let mut state = DocumentState::from_source("old".to_string(), 4);
        state.text = "new".to_string();
        state.mark_source_changed();

        let old = Arc::new(parse_document("old"));
        let current = Arc::new(parse_document("new"));
        assert!(!state.install_parsed(4, old));
        assert!(!state.is_parsed_current());
        assert!(state.install_parsed(5, current));
        assert!(state.is_parsed_current());
        assert_eq!(state.snapshot(9).unwrap().revision, 5);
    }

    #[test]
    fn 同样的源码不会制造空版本() {
        let mut state = DocumentState::from_source("same".to_string(), 3);
        assert!(!state.set_source("same".to_string()));
        assert_eq!(state.document_revision, 3);
    }
}
