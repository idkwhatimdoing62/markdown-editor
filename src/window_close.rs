#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    Allow,
    Confirm,
    KeepOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabId(u64);

impl From<u64> for TabId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<TabId> for u64 {
    fn from(value: TabId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseRequestId(u64);

#[derive(Default)]
enum CloseState {
    #[default]
    Idle,
    Confirming {
        request_id: CloseRequestId,
        unsaved_documents: Vec<UnsavedDocument>,
        failed_tab_id: Option<TabId>,
    },
    Approved,
}

#[derive(Default)]
pub struct CloseGuard {
    state: CloseState,
    next_request_id: u64,
}

impl CloseGuard {
    pub fn request_close(&mut self, unsaved_documents: Vec<UnsavedDocument>) -> CloseAction {
        if matches!(self.state, CloseState::Approved) {
            CloseAction::Allow
        } else if matches!(self.state, CloseState::Confirming { .. }) {
            CloseAction::Confirm
        } else if unsaved_documents.is_empty() {
            self.state = CloseState::Idle;
            CloseAction::Allow
        } else {
            let request_id = CloseRequestId(self.next_request_id);
            self.next_request_id = self.next_request_id.wrapping_add(1);
            self.state = CloseState::Confirming {
                request_id,
                unsaved_documents,
                failed_tab_id: None,
            };
            CloseAction::Confirm
        }
    }

    pub fn unsaved_documents(&self) -> &[UnsavedDocument] {
        match &self.state {
            CloseState::Confirming {
                unsaved_documents, ..
            } => unsaved_documents,
            CloseState::Idle | CloseState::Approved => &[],
        }
    }

    pub fn cancel(&mut self) -> CloseAction {
        self.state = CloseState::Idle;
        CloseAction::KeepOpen
    }

    pub fn is_confirmation_open(&self) -> bool {
        matches!(self.state, CloseState::Confirming { .. })
    }

    pub fn confirmation_id(&self) -> Option<CloseRequestId> {
        match self.state {
            CloseState::Confirming { request_id, .. } => Some(request_id),
            CloseState::Idle | CloseState::Approved => None,
        }
    }

    pub fn discard_all(&mut self) -> CloseAction {
        if self.is_confirmation_open() {
            self.state = CloseState::Approved;
            CloseAction::Allow
        } else {
            CloseAction::KeepOpen
        }
    }

    pub fn finish_save_all(
        &mut self,
        request_id: CloseRequestId,
        result: Result<(), TabId>,
    ) -> CloseAction {
        if self.confirmation_id() != Some(request_id) {
            return CloseAction::KeepOpen;
        }
        match result {
            Ok(()) => {
                self.state = CloseState::Approved;
                CloseAction::Allow
            }
            Err(tab_id) => {
                if let CloseState::Confirming { failed_tab_id, .. } = &mut self.state {
                    *failed_tab_id = Some(tab_id);
                }
                CloseAction::KeepOpen
            }
        }
    }

    pub fn failed_tab_id(&self) -> Option<TabId> {
        match self.state {
            CloseState::Confirming { failed_tab_id, .. } => failed_tab_id,
            CloseState::Idle | CloseState::Approved => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsavedDocument {
    pub tab_id: TabId,
    pub title: String,
}

#[cfg(test)]
mod tests {
    use super::{CloseAction, CloseGuard, CloseRequestId, TabId, UnsavedDocument};

    #[test]
    fn window_without_unsaved_documents_closes_immediately() {
        let mut guard = CloseGuard::default();

        assert_eq!(guard.request_close(Vec::new()), CloseAction::Allow);
    }

    #[test]
    fn window_with_multiple_unsaved_documents_requests_one_confirmation() {
        let mut guard = CloseGuard::default();
        let documents = vec![
            UnsavedDocument {
                tab_id: TabId::from(3),
                title: "notes.md".to_string(),
            },
            UnsavedDocument {
                tab_id: TabId::from(7),
                title: "未命名 7".to_string(),
            },
        ];

        assert_eq!(guard.request_close(documents.clone()), CloseAction::Confirm);
        assert_eq!(guard.unsaved_documents(), documents.as_slice());
    }

    #[test]
    fn cancel_keeps_the_window_open_and_dismisses_confirmation() {
        let mut guard = CloseGuard::default();
        guard.request_close(vec![UnsavedDocument {
            tab_id: TabId::from(1),
            title: "draft.md".to_string(),
        }]);

        assert_eq!(guard.cancel(), CloseAction::KeepOpen);
        assert!(!guard.is_confirmation_open());
    }

    #[test]
    fn discard_allows_the_follow_up_close_even_while_documents_remain_dirty() {
        let mut guard = CloseGuard::default();
        let documents = vec![UnsavedDocument {
            tab_id: TabId::from(1),
            title: "draft.md".to_string(),
        }];
        guard.request_close(documents.clone());

        assert_eq!(guard.discard_all(), CloseAction::Allow);
        assert_eq!(guard.request_close(documents), CloseAction::Allow);
    }

    #[test]
    fn saving_every_document_allows_the_follow_up_close() {
        let mut guard = CloseGuard::default();
        let documents = vec![UnsavedDocument {
            tab_id: TabId::from(4),
            title: "saved.md".to_string(),
        }];
        guard.request_close(documents.clone());
        let request_id = guard.confirmation_id().unwrap();

        assert_eq!(
            guard.finish_save_all(request_id, Ok(())),
            CloseAction::Allow
        );
        assert_eq!(guard.request_close(documents), CloseAction::Allow);
    }

    #[test]
    fn one_save_failure_keeps_the_window_open_and_identifies_the_document() {
        let mut guard = CloseGuard::default();
        guard.request_close(vec![UnsavedDocument {
            tab_id: TabId::from(9),
            title: "readonly.md".to_string(),
        }]);
        let request_id = guard.confirmation_id().unwrap();

        assert_eq!(
            guard.finish_save_all(request_id, Err(TabId::from(9))),
            CloseAction::KeepOpen
        );
        assert!(guard.is_confirmation_open());
        assert_eq!(guard.failed_tab_id(), Some(TabId::from(9)));
    }

    #[test]
    fn stale_save_completion_cannot_approve_a_future_close() {
        let mut guard = CloseGuard::default();

        assert_eq!(
            guard.finish_save_all(CloseRequestId(99), Ok(())),
            CloseAction::KeepOpen
        );
        assert_eq!(
            guard.request_close(vec![UnsavedDocument {
                tab_id: TabId::from(12),
                title: "later.md".to_string(),
            }]),
            CloseAction::Confirm
        );
    }

    #[test]
    fn save_completion_from_an_old_confirmation_cannot_approve_a_new_confirmation() {
        let mut guard = CloseGuard::default();
        guard.request_close(vec![UnsavedDocument {
            tab_id: TabId::from(20),
            title: "old.md".to_string(),
        }]);
        let old_request_id = guard.confirmation_id().unwrap();
        guard.cancel();
        guard.request_close(vec![UnsavedDocument {
            tab_id: TabId::from(21),
            title: "new.md".to_string(),
        }]);

        assert_eq!(
            guard.finish_save_all(old_request_id, Ok(())),
            CloseAction::KeepOpen
        );
        assert!(guard.is_confirmation_open());
    }

    #[test]
    fn repeated_close_request_preserves_the_open_confirmation() {
        let mut guard = CloseGuard::default();
        let original = vec![UnsavedDocument {
            tab_id: TabId::from(30),
            title: "original.md".to_string(),
        }];
        guard.request_close(original.clone());
        let request_id = guard.confirmation_id().unwrap();
        guard.finish_save_all(request_id, Err(TabId::from(30)));

        assert_eq!(
            guard.request_close(vec![UnsavedDocument {
                tab_id: TabId::from(31),
                title: "replacement.md".to_string(),
            }]),
            CloseAction::Confirm
        );
        assert_eq!(guard.confirmation_id(), Some(request_id));
        assert_eq!(guard.unsaved_documents(), original.as_slice());
        assert_eq!(guard.failed_tab_id(), Some(TabId::from(30)));
    }
}
