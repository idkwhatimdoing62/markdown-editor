//! Background literal-search worker.
//!
//! Search requests are keyed by tab id. While a query is being computed, a
//! newer request for that tab replaces the old one in the mailbox; results
//! carry both the document revision and query generation for the UI guard.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use crate::document_core::Revision;
use crate::search::SearchResults;

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub tab_id: u64,
    pub revision: Revision,
    pub generation: u64,
    pub source: Arc<str>,
    pub query: String,
}

#[derive(Clone)]
pub struct SearchResult {
    pub tab_id: u64,
    pub revision: Revision,
    pub generation: u64,
    pub results: SearchResults,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerClosed;

pub struct SearchWorker {
    inbox: Arc<WorkerInbox>,
    receiver: mpsc::Receiver<SearchResult>,
    join: Option<JoinHandle<()>>,
    dead: AtomicBool,
}

#[derive(Default)]
struct InboxState {
    pending: HashMap<u64, SearchRequest>,
    closed: bool,
}

#[derive(Default)]
struct WorkerInbox {
    state: Mutex<InboxState>,
    ready: Condvar,
}

impl SearchWorker {
    pub fn new() -> Self {
        let (result_sender, result_receiver) = mpsc::channel::<SearchResult>();
        let inbox = Arc::new(WorkerInbox::default());
        let worker_inbox = Arc::clone(&inbox);
        let join = thread::Builder::new()
            .name("markdown-search-worker".to_string())
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
                        let results = SearchResults::new(&request.source, &request.query);
                        if result_sender
                            .send(SearchResult {
                                tab_id: request.tab_id,
                                revision: request.revision,
                                generation: request.generation,
                                results,
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

    pub fn submit(&self, request: SearchRequest) -> Result<(), WorkerClosed> {
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

    /// Collect ready results plus a `worker gone` flag, mirroring the parse
    /// worker: on disconnect the UI falls back to synchronous searching
    /// instead of waiting forever for a result that can never arrive.
    pub fn drain(&self) -> (Vec<SearchResult>, bool) {
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

    pub fn cancel_tab(&self, tab_id: u64) {
        let mut state = self
            .inbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending.remove(&tab_id);
    }
}

impl Default for SearchWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SearchWorker {
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

    #[test]
    fn search_result_carries_document_and_query_generations() {
        let worker = SearchWorker::new();
        worker
            .submit(SearchRequest {
                tab_id: 4,
                revision: 12,
                generation: 3,
                source: Arc::from("alpha beta alpha"),
                query: "alpha".to_string(),
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = worker.drain().0.into_iter().next() {
                assert_eq!(
                    (result.tab_id, result.revision, result.generation),
                    (4, 12, 3)
                );
                assert_eq!(result.results.ranges(), &[0..5, 11..16]);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "search worker did not return a result"
            );
            thread::yield_now();
        }
    }
}
