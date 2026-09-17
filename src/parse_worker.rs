//! Small, latest-wins worker for CPU-heavy Markdown parsing.
//!
//! The worker has no UI dependency. The application polls immutable results
//! from the main thread and applies its own tab/revision guard.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use crate::document_core::Revision;
use crate::markdown::{ParsedDocument, parse_document};

#[derive(Debug, Clone)]
pub struct ParseRequest {
    pub tab_id: u64,
    pub revision: Revision,
    pub source: Arc<str>,
}

#[derive(Clone)]
pub struct ParseResult {
    pub tab_id: u64,
    pub revision: Revision,
    pub document: Arc<ParsedDocument>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerClosed;

/// One process-scoped parser thread. Requests queued while a parse is running
/// are coalesced to the newest revision *per tab* before the next parse starts.
/// The mailbox is bounded by the number of tab ids, preventing a long typing
/// burst from retaining one source copy per keystroke.
pub struct ParseWorker {
    inbox: Arc<WorkerInbox>,
    receiver: mpsc::Receiver<ParseResult>,
    join: Option<JoinHandle<()>>,
    dead: AtomicBool,
}

#[derive(Default)]
struct InboxState {
    pending: HashMap<u64, ParseRequest>,
    closed: bool,
}

#[derive(Default)]
struct WorkerInbox {
    state: Mutex<InboxState>,
    ready: Condvar,
}

impl ParseWorker {
    pub fn new() -> Self {
        let (result_sender, result_receiver) = mpsc::channel::<ParseResult>();
        let inbox = Arc::new(WorkerInbox::default());
        let worker_inbox = Arc::clone(&inbox);
        let join = thread::Builder::new()
            .name("markdown-parse-worker".to_string())
            .spawn(move || {
                loop {
                    let requests = {
                        let mut state = worker_inbox
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while state.pending.is_empty() && !state.closed {
                            state = worker_inbox
                                .ready
                                .wait(state)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        if state.closed {
                            break;
                        }
                        std::mem::take(&mut state.pending)
                    };

                    for request in requests.into_values() {
                        let document = Arc::new(parse_document(&request.source));
                        if result_sender
                            .send(ParseResult {
                                tab_id: request.tab_id,
                                revision: request.revision,
                                document,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            })
            .ok();
        let dead = join.is_none();
        if dead {
            let mut state = inbox
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
        }
        Self {
            inbox,
            receiver: result_receiver,
            join,
            dead: AtomicBool::new(dead),
        }
    }

    pub fn submit(&self, request: ParseRequest) -> Result<(), WorkerClosed> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(WorkerClosed);
        }
        let mut state = self
            .inbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(WorkerClosed);
        }
        state.pending.insert(request.tab_id, request);
        drop(state);
        self.inbox.ready.notify_one();
        Ok(())
    }

    /// Collect ready results. The second element reports that the worker
    /// thread is gone (a panic or a finished shutdown), letting the UI fall
    /// back to synchronous parsing instead of waiting for a result that can
    /// never arrive.
    pub fn drain(&self) -> (Vec<ParseResult>, bool) {
        let mut results = Vec::new();
        let mut disconnected = false;
        loop {
            match self.receiver.try_recv() {
                Ok(result) => results.push(result),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.dead.store(true, Ordering::Relaxed);
                    disconnected = true;
                    break;
                }
            }
        }
        (results, disconnected)
    }

    /// Drop a queued request for a tab that is no longer alive. A request
    /// already taken by the worker may still finish; the UI revision/tab guard
    /// handles that result safely.
    pub fn cancel_tab(&self, tab_id: u64) {
        let mut state = self
            .inbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending.remove(&tab_id);
    }
}

impl Default for ParseWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ParseWorker {
    fn drop(&mut self) {
        {
            let mut state = self
                .inbox
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            state.pending.clear();
        }
        self.inbox.ready.notify_one();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn collect_until(
        worker: &ParseWorker,
        mut done: impl FnMut(&[ParseResult]) -> bool,
    ) -> Vec<ParseResult> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut results = Vec::new();
        loop {
            results.extend(worker.drain().0);
            if done(&results) {
                return results;
            }
            assert!(Instant::now() < deadline, "worker did not return a result");
            thread::yield_now();
        }
    }

    #[test]
    fn worker_returns_a_revision_tagged_immutable_document() {
        let worker = ParseWorker::new();
        worker
            .submit(ParseRequest {
                tab_id: 3,
                revision: 8,
                source: Arc::from("# heading"),
            })
            .unwrap();

        let results = collect_until(&worker, |results| !results.is_empty());
        let result = &results[0];
        assert_eq!(result.tab_id, 3);
        assert_eq!(result.revision, 8);
        assert_eq!(result.document.source(), "# heading");
    }

    #[test]
    fn latest_request_is_kept_independently_for_each_tab() {
        let worker = ParseWorker::new();
        for revision in 1..=20 {
            worker
                .submit(ParseRequest {
                    tab_id: 1,
                    revision,
                    source: Arc::from(format!("tab one {revision}")),
                })
                .unwrap();
        }
        worker
            .submit(ParseRequest {
                tab_id: 2,
                revision: 7,
                source: Arc::from("tab two"),
            })
            .unwrap();

        let results = collect_until(&worker, |results| {
            results
                .iter()
                .any(|result| result.tab_id == 1 && result.revision == 20)
                && results
                    .iter()
                    .any(|result| result.tab_id == 2 && result.revision == 7)
        });
        assert!(results.iter().any(|result| {
            result.tab_id == 1 && result.revision == 20 && result.document.source() == "tab one 20"
        }));
        assert!(results.iter().any(|result| {
            result.tab_id == 2 && result.revision == 7 && result.document.source() == "tab two"
        }));
    }
}
