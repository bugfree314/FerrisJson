//! One active operation and one replaceable pending request per worker.
use crate::document::{Document, NodeIds, SearchScope};
use crate::engine::budget::{Reservation, Resource};
use crate::engine::file_source::{Cancellation, Priority, with_io_context};
use crate::engine::session::{SessionId, TaskId};
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
    Search(NodeIds),
    Query {
        expression: String,
        ids: NodeIds,
        preview: String,
    },
}
pub struct Completion {
    generation: u64,
    pub task: TaskId,
    _reservation: Option<Reservation>,
    pub elapsed: Duration,
    pub result: Result<Output, String>,
}
struct Request {
    generation: u64,
    doc: Arc<Document>,
    operation: Operation,
    reservation: Option<Reservation>,
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
    session: Option<SessionId>,
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
                let result = with_io_context(
                    Priority::User,
                    Cancellation::Generation(Arc::clone(&epoch), request.generation),
                    || match request.operation {
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
                    },
                );
                // A result-channel send can block. Never retain a document while
                // waiting to deliver IDs/preview, especially during low-memory loading.
                let task = TaskId {
                    session: request.doc.session_id(),
                    generation: request.generation,
                };
                drop(request.doc);
                if cancelled() {
                    continue;
                }
                let completion = Completion {
                    generation: request.generation,
                    task,
                    _reservation: request.reservation,
                    elapsed: started.elapsed(),
                    result,
                };
                // Bound retained results. Only the worker can wait; the UI polls
                // without blocking and closing the receiver wakes a waiting send.
                let mut completion = completion;
                loop {
                    if cancelled() {
                        break;
                    }
                    match tx.try_send(completion) {
                        Ok(()) => break,
                        Err(mpsc::TrySendError::Disconnected(_)) => return,
                        Err(mpsc::TrySendError::Full(value)) => {
                            completion = value;
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
                ctx.request_repaint();
            }
        });
        Self {
            inbox,
            generation,
            results,
            busy: false,
            session: None,
        }
    }
    pub fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.inbox.0.lock().unwrap().pending = None;
        while self.results.try_recv().is_ok() {}
        self.busy = false;
        self.session = None;
    }
    pub fn submit(&mut self, doc: Arc<Document>, operation: Operation) -> Result<(), String> {
        self.cancel();
        let input_bytes = match &operation {
            Operation::Search { term, .. } => term.capacity(),
            Operation::Query(expression) => expression.capacity(),
        };
        let reservation = doc.budget().reserve(
            Resource::Memory,
            input_bytes.saturating_add(512 * 1024),
            "worker request and preview allowance",
        )?;
        self.session = Some(doc.session_id());
        let generation = self.generation.load(Ordering::Relaxed);
        self.inbox.0.lock().unwrap().pending = Some(Request {
            generation,
            doc,
            operation,
            reservation: Some(reservation),
        });
        self.busy = true;
        self.inbox.1.notify_one();
        Ok(())
    }
    pub fn poll(&mut self) -> Option<Completion> {
        while let Ok(result) = self.results.try_recv() {
            if result.generation == self.generation.load(Ordering::Relaxed)
                && Some(result.task.session) == self.session
            {
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
    #[test]
    fn full_result_channel_does_not_retain_documents() {
        let ctx = eframe::egui::Context::default();
        let worker = Worker::new(ctx);
        let send = || {
            let doc = Arc::new(Document::parse("[1,2,3]").unwrap());
            let weak = Arc::downgrade(&doc);
            worker.inbox.0.lock().unwrap().pending = Some(Request {
                generation: 0,
                doc,
                operation: Operation::Query("$[*]".into()),
                reservation: None,
            });
            worker.inbox.1.notify_one();
            let deadline = Instant::now() + Duration::from_secs(2);
            while weak.strong_count() != 0 {
                assert!(
                    Instant::now() < deadline,
                    "Worker retained document while sending a result"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        send();
        // Leave the first completion in the capacity-one channel, then request another.
        send();
        drop(worker);
    }
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
        worker
            .submit(
                old,
                Operation::Search {
                    term: "old".into(),
                    case: false,
                    regex: false,
                    scope: SearchScope::All,
                },
            )
            .unwrap();
        let new = Arc::new(Document::parse(r#"{"answer":42}"#).unwrap());
        for _ in 0..20 {
            worker
                .submit(Arc::clone(&new), Operation::Query("$.missing".into()))
                .unwrap();
        }
        worker
            .submit(new, Operation::Query("$.answer".into()))
            .unwrap();
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
        worker
            .submit(Arc::clone(&doc), Operation::Query("$.x".into()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.results.try_recv().is_err() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        worker.cancel();
        assert!(!worker.busy);
        assert!(worker.poll().is_none());
        worker.submit(doc, Operation::Query("$[?".into())).unwrap();
        assert!(
            matches!(wait(&mut worker).result, Err(error) if error.contains("Invalid JSONPath"))
        );
    }
    #[test]
    fn obsolete_queued_results_cannot_replace_the_current_result() {
        let (tx, results) = mpsc::sync_channel(1);
        let session = Document::parse("null").unwrap().session_id();
        let mut worker = Worker {
            inbox: Arc::new((Mutex::new(Inbox::default()), Condvar::new())),
            generation: Arc::new(AtomicU64::new(2)),
            results,
            busy: true,
            session: Some(session),
        };
        tx.send(Completion {
            generation: 1,
            task: TaskId {
                session,
                generation: 1,
            },
            _reservation: None,
            elapsed: Duration::ZERO,
            result: Ok(Output::Search(
                NodeIds::from_slice(&Default::default(), &[99]).unwrap(),
            )),
        })
        .unwrap();
        assert!(worker.poll().is_none());
        assert!(worker.busy);
        tx.send(Completion {
            generation: 2,
            task: TaskId {
                session: Document::parse("null").unwrap().session_id(),
                generation: 2,
            },
            _reservation: None,
            elapsed: Duration::ZERO,
            result: Ok(Output::Search(NodeIds::default())),
        })
        .unwrap();
        assert!(
            worker.poll().is_none(),
            "same task generation from a different session must be rejected"
        );
        tx.send(Completion {
            generation: 2,
            task: TaskId {
                session,
                generation: 2,
            },
            _reservation: None,
            elapsed: Duration::ZERO,
            result: Ok(Output::Search(
                NodeIds::from_slice(&Default::default(), &[1]).unwrap(),
            )),
        })
        .unwrap();
        assert!(matches!(worker.poll().unwrap().result, Ok(Output::Search(ids)) if ids == vec![1]));
        assert!(!worker.busy);
    }
}
