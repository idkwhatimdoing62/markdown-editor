//! Lossless source edits from the single-surface browser editor.
//!
//! The browser edits source slices, never converts rendered HTML back to Markdown.
use std::collections::VecDeque;

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Message {
    pub session: u64,
    pub sequence: u64,
    pub start: usize,
    pub end: usize,
    pub replacement: String,
    pub editing: bool,
    #[serde(default)]
    pub command: Option<String>,
}

pub struct Update {
    pub tab_id: u64,
    pub expected: String,
    pub text: String,
    pub command: Option<String>,
}

#[derive(Default)]
pub struct Bridge {
    pub session: u64,
    pub acknowledged: bool,
    pub editing: bool,
    tab_id: u64,
    baseline: String,
    applied: String,
    current: String,
    sequence: u64,
    pending: bool,
    command: Option<String>,
    undo: VecDeque<String>,
    redo: VecDeque<String>,
    checkpointed: bool,
}

impl Bridge {
    pub fn configure(&mut self, tab_id: u64, source: &str) -> u64 {
        if self.tab_id != tab_id || self.current != source {
            self.undo.clear();
            self.redo.clear();
        }
        self.session = self.session.wrapping_add(1);
        self.tab_id = tab_id;
        self.baseline = source.to_owned();
        self.applied = source.to_owned();
        self.current = source.to_owned();
        self.sequence = 0;
        self.editing = false;
        self.acknowledged = false;
        self.pending = false;
        self.checkpointed = false;
        self.command = None;
        self.session
    }

    pub fn matches(&self, tab_id: u64, source: &str) -> bool {
        self.session != 0 && self.tab_id == tab_id && self.current == source
    }

    pub fn receive(&mut self, message: Message) {
        if message.session != self.session || message.sequence <= self.sequence {
            return;
        }
        if message.start > message.end
            || !self.baseline.is_char_boundary(message.start)
            || !self.baseline.is_char_boundary(message.end)
            || message.end > self.baseline.len()
            || self.baseline.len() - (message.end - message.start) + message.replacement.len()
                > crate::io::MAX_FILE_SIZE as usize
        {
            return;
        }
        self.sequence = message.sequence;
        self.editing = message.editing;
        let mut next = self.baseline.clone();
        next.replace_range(message.start..message.end, &message.replacement);
        if next != self.current {
            if !self.checkpointed {
                self.undo.push_back(self.current.clone());
                trim_history(&mut self.undo);
                self.redo.clear();
                self.checkpointed = true;
            }
            self.current = next;
        }
        if let Some(command) = message.command {
            match command.as_str() {
                "undo" | "redo" => {
                    let (from, to) = if command == "undo" {
                        (&mut self.undo, &mut self.redo)
                    } else {
                        (&mut self.redo, &mut self.undo)
                    };
                    if let Some(text) = from.pop_back() {
                        to.push_back(std::mem::replace(&mut self.current, text));
                        trim_history(to);
                    }
                    self.editing = false;
                }
                "save" | "save-as" | "write" | "read" | "split" | "find" | "close" => {
                    self.command = Some(command);
                }
                _ => {}
            }
        }
        self.pending = true;
    }

    pub fn take_update(&mut self) -> Option<Update> {
        if !std::mem::take(&mut self.pending) {
            return None;
        }
        let expected = std::mem::replace(&mut self.applied, self.current.clone());
        Some(Update {
            tab_id: self.tab_id,
            expected,
            text: self.current.clone(),
            command: self.command.take(),
        })
    }
}

fn trim_history(history: &mut VecDeque<String>) {
    let mut bytes: usize = history.iter().map(String::len).sum();
    while history.len() > 1 && (history.len() > 50 || bytes > 16 * 1024 * 1024) {
        bytes -= history.pop_front().map_or(0, |text| text.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(session: u64, sequence: u64, start: usize, end: usize, text: &str) -> Message {
        Message {
            session,
            sequence,
            start,
            end,
            replacement: text.into(),
            editing: true,
            command: None,
        }
    }

    #[test]
    fn edits_preserve_unicode_and_unedited_source_exactly() {
        let mut bridge = Bridge::default();
        let original = "# 标题\r\n\r\n正文😀\r\n\r\n[ref]: target\r\n";
        let start = original.find("正文").unwrap();
        let end = start + "正文😀".len();
        let session = bridge.configure(1, original);
        bridge.receive(edit(session, 1, start, end, "修改😊"));
        bridge.receive(edit(session, 2, start, end, "最新😊"));
        let update = bridge.take_update().unwrap();
        assert_eq!(update.expected, original);
        assert_eq!(update.text, original.replace("正文😀", "最新😊"));
        assert!(bridge.editing);
    }

    #[test]
    fn stale_sessions_sequences_and_invalid_ranges_are_rejected() {
        let mut bridge = Bridge::default();
        let old = bridge.configure(1, "one");
        let current = bridge.configure(2, "中文");
        bridge.receive(edit(old, 1, 0, 3, "wrong tab"));
        bridge.receive(edit(current, 1, 1, 3, "invalid unicode boundary"));
        bridge.receive(edit(current, 1, 0, 999, "invalid range"));
        assert!(bridge.take_update().is_none());
        bridge.receive(edit(current, 2, 0, 6, "two"));
        bridge.receive(edit(current, 1, 0, 6, "old"));
        assert_eq!(bridge.take_update().unwrap().text, "two");
    }

    #[test]
    fn undo_redo_survives_render_reconfiguration() {
        let mut bridge = Bridge::default();
        let session = bridge.configure(1, "first");
        bridge.receive(edit(session, 1, 0, 5, "second"));
        bridge.take_update();
        let session = bridge.configure(1, "second");
        let mut message = edit(session, 1, 0, 6, "second");
        message.command = Some("undo".into());
        bridge.receive(message);
        assert_eq!(bridge.take_update().unwrap().text, "first");
        let session = bridge.configure(1, "first");
        let mut message = edit(session, 1, 0, 5, "first");
        message.command = Some("redo".into());
        bridge.receive(message);
        assert_eq!(bridge.take_update().unwrap().text, "second");
    }
}
