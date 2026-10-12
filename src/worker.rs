//! One active operation and one replaceable pending request per worker.
use crate::document::{Document, SearchScope};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc::{self, Receiver},
};
use std::time::{Duration, Instant};

pub enum Operation {
    Search {
        term: String,
        case: bool,
        regex: bool,
        scope: SearchScope,
    },
    Query(String),
}
pub enum Output {
    Search(Vec<usize>),
    Query {
        expression: String,
        ids: Vec<usize>,
        preview: String,
    },
}
pub struct Completion {
    generation: u64,
    pub elapsed: Duration,
    pub result: Result<Output, String>,
}
struct Request {
    generation: u64,
    doc: Arc<Document>,
    operation: Operation,
}
#[derive(Default)]
struct Inbox {
    pending: Option<Request>,
    stopped: bool,
}
pub struct Worker {
    inbox: Arc<(Mutex<Inbox>, Condvar)>,
    generation: Arc<AtomicU64>,
    results: Receiver<Completion>,
    pub busy: bool,
}
impl Worker {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let inbox = Arc::new((Mutex::new(Inbox::default()), Condvar::new()));
        let generation = Arc::new(AtomicU64::new(0));
        let (tx, results) = mpsc::sync_channel(1);
        let shared = Arc::clone(&inbox);
        let epoch = Arc::clone(&generation);
        std::thread::spawn(move || {
            loop {
                let request = {
                    let (lock, signal) = &*shared;
                    let mut inbox = lock.lock().unwrap();
                    while inbox.pending.is_none() && !inbox.stopped {
                        inbox = signal.wait(inbox).unwrap();
                    }
                    if inbox.stopped {
                        return;
                    }
                    inbox.pending.take().unwrap()
                };
                let cancelled = || epoch.load(Ordering::Relaxed) != request.generation;
                if cancelled() {
                    continue;
                }
                let started = Instant::now();
                let result = match request.operation {
                    Operation::Search {
                        term,
                        case,
                        regex,
                        scope,
                    } => request
                        .doc
                        .search_cancellable(&term, case, regex, scope, cancelled)
                        .map(Output::Search),
                    Operation::Query(expression) => {
                        // The library evaluates a whole query without a cancellation hook.
                        // Suppress stale work before formatting; pending queries are coalesced.
                        request
                            .doc
                            .query_path_cancellable(&expression, cancelled)
                            .map(|ids| {
                                let preview = if cancelled() {
                                    String::new()
                                } else {
                                    request.doc.results_preview(&ids)
                                };
                                Output::Query {
                                    expression,
                                    ids,
                                    preview,
                                }
                            })
                    }
                };
                if cancelled() {
                    continue;
                }
                let completion = Completion {
                    generation: request.generation,
                    elapsed: started.elapsed(),
                    result,
                };
                // Bound retained results. Only the worker can wait; the UI polls
                // without blocking and closing the receiver wakes a waiting send.
                if tx.send(completion).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
        Self {
            inbox,
            generation,
            results,
            busy: false,
        }
    }
    pub fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.inbox.0.lock().unwrap().pending = None;
        while self.results.try_recv().is_ok() {}
        self.busy = false;
    }
    pub fn submit(&mut self, doc: Arc<Document>, operation: Operation) {
        self.cancel();
        let generation = self.generation.load(Ordering::Relaxed);
        self.inbox.0.lock().unwrap().pending = Some(Request {
            generation,
            doc,
            operation,
        });
        self.busy = true;
        self.inbox.1.notify_one();
    }
    pub fn poll(&mut self) -> Option<Completion> {
        while let Ok(result) = self.results.try_recv() {
            if result.generation == self.generation.load(Ordering::Relaxed) {
                self.busy = false;
                return Some(result);
            }
        }
        None
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        self.inbox.0.lock().unwrap().stopped = true;
        self.inbox.1.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wait(worker: &mut Worker) -> Completion {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = worker.poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "worker did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn newer_requests_replace_old_searches_and_use_the_new_document() {
        let mut worker = Worker::new(eframe::egui::Context::default());
        let old =
            Arc::new(Document::parse(&serde_json::json!(vec!["old"; 20_000]).to_string()).unwrap());
        worker.submit(
            old,
            Operation::Search {
                term: "old".into(),
                case: false,
                regex: false,
                scope: SearchScope::All,
            },
        );
        let new = Arc::new(Document::parse(r#"{"answer":42}"#).unwrap());
        for _ in 0..20 {
            worker.submit(Arc::clone(&new), Operation::Query("$.missing".into()));
        }
        worker.submit(new, Operation::Query("$.answer".into()));
        match wait(&mut worker).result.unwrap() {
            Output::Query {
                ids,
                preview,
                expression,
            } => {
                assert_eq!(ids, vec![1]);
                assert_eq!(preview, "[\n  42\n]");
                assert_eq!(expression, "$.answer");
            }
            _ => panic!("obsolete search was delivered"),
        }
        assert!(!worker.busy);
    }
    #[test]
    fn cancellation_resets_busy_and_worker_accepts_new_requests() {
        let mut worker = Worker::new(eframe::egui::Context::default());
        let doc = Arc::new(Document::parse(r#"{"x":1}"#).unwrap());
        worker.submit(Arc::clone(&doc), Operation::Query("$.x".into()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.results.try_recv().is_err() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        worker.cancel();
        assert!(!worker.busy);
        assert!(worker.poll().is_none());
        worker.submit(doc, Operation::Query("$[?".into()));
        assert!(
            matches!(wait(&mut worker).result, Err(error) if error.contains("Invalid JSONPath"))
        );
    }
    #[test]
    fn obsolete_queued_results_cannot_replace_the_current_result() {
        let (tx, results) = mpsc::sync_channel(1);
        let mut worker = Worker {
            inbox: Arc::new((Mutex::new(Inbox::default()), Condvar::new())),
            generation: Arc::new(AtomicU64::new(2)),
            results,
            busy: true,
        };
        tx.send(Completion {
            generation: 1,
            elapsed: Duration::ZERO,
            result: Ok(Output::Search(vec![99])),
        })
        .unwrap();
        assert!(worker.poll().is_none());
        assert!(worker.busy);
        tx.send(Completion {
            generation: 2,
            elapsed: Duration::ZERO,
            result: Ok(Output::Search(vec![1])),
        })
        .unwrap();
        assert!(matches!(worker.poll().unwrap().result, Ok(Output::Search(ids)) if ids == vec![1]));
        assert!(!worker.busy);
    }
}
