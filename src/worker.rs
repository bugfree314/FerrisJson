//! One active operation and one replaceable pending request per worker.
use crate::document::{Document, NodeIds, SearchScope};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc::{self, Receiver},
};
use std::time::{Duration, Instant};

pub enum Operation {
    StreamSearch {
        term: String,
        case: bool,
        regex: bool,
        scope: SearchScope,
        from: u32,
        next: bool,
        reverse: bool,
        count: usize,
    },
    Search {
        term: String,
        case: bool,
        regex: bool,
        scope: SearchScope,
    },
    Query(String),
}
pub enum Output {
    SearchBatch {
        ids: NodeIds,
        scanned: usize,
        total: usize,
        finished: bool,
        next: bool,
    },
    Search(NodeIds),
    Query {
        expression: String,
        ids: NodeIds,
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
                    Operation::StreamSearch {
                        term,
                        case,
                        regex,
                        scope,
                        from,
                        next,
                        reverse,
                        count,
                    } => {
                        if next {
                            let result = request
                                .doc
                                .find_next(
                                    &term, case, regex, scope, from, reverse, count, &cancelled,
                                )
                                .map(|id| Output::SearchBatch {
                                    ids: id.into_iter().collect(),
                                    scanned: request.doc.node_count(),
                                    total: request.doc.node_count(),
                                    finished: true,
                                    next: true,
                                });
                            drop(request.doc);
                            if !cancelled() {
                                if tx
                                    .send(Completion {
                                        generation: request.generation,
                                        elapsed: started.elapsed(),
                                        result,
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                ctx.request_repaint();
                            }
                            continue;
                        }
                        let mut batch = Vec::new();
                        let mut scanned = 0;
                        let total = request.doc.node_count();
                        let mut last_emit = Instant::now();
                        let send = |ids, scanned| -> Result<(), String> {
                            let mut message = Completion {
                                generation: request.generation,
                                elapsed: started.elapsed(),
                                result: Ok(Output::SearchBatch {
                                    ids,
                                    scanned,
                                    total,
                                    finished: false,
                                    next,
                                }),
                            };
                            loop {
                                if cancelled() {
                                    return Err("Search cancelled".into());
                                }
                                match tx.try_send(message) {
                                    Ok(()) => {
                                        ctx.request_repaint();
                                        return Ok(());
                                    }
                                    Err(mpsc::TrySendError::Full(value)) => {
                                        message = value;
                                        std::thread::sleep(Duration::from_millis(5));
                                    }
                                    Err(mpsc::TrySendError::Disconnected(_)) => {
                                        return Err("Search receiver closed".into());
                                    }
                                }
                            }
                        };
                        let mut first_report = true;
                        request
                            .doc
                            .stream_search(
                                &term,
                                case,
                                regex,
                                scope,
                                0,
                                &cancelled,
                                |id, done, _| {
                                    scanned = done;
                                    if let Some(id) = id {
                                        batch.push(id);
                                    }
                                    if (!batch.is_empty() && first_report)
                                        || batch.len() >= 65_536
                                        || last_emit.elapsed() >= Duration::from_millis(100)
                                    {
                                        send(std::mem::take(&mut batch), scanned)?;
                                        first_report = false;
                                        last_emit = Instant::now();
                                    }
                                    Ok(true)
                                },
                            )
                            .map(|()| Output::SearchBatch {
                                ids: batch,
                                scanned,
                                total,
                                finished: true,
                                next: false,
                            })
                    }
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
                // A result-channel send can block. Never retain a document while
                // waiting to deliver IDs/preview, especially during low-memory loading.
                drop(request.doc);
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
                self.busy = matches!(
                    &result.result,
                    Ok(Output::SearchBatch {
                        finished: false,
                        ..
                    })
                );
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
    fn incremental_batches_keep_busy_and_cancellation_discards_old_generations() {
        let ctx = eframe::egui::Context::default();
        let mut worker = Worker::new(ctx);
        let doc = Arc::new(
            Document::open_lazy(
                std::path::Path::new("examples/sample.json"),
                || false,
                &crate::document::LoadProgress::default(),
            )
            .unwrap(),
        );
        let submit = |worker: &mut Worker, term: &str| {
            worker.submit(
                Arc::clone(&doc),
                Operation::StreamSearch {
                    term: term.into(),
                    case: false,
                    regex: term == ".*",
                    scope: SearchScope::Keys,
                    from: 0,
                    next: false,
                    reverse: false,
                    count: 1,
                },
            )
        };
        submit(&mut worker, ".*");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(event) = worker.poll() {
                assert!(matches!(
                    event.result,
                    Ok(Output::SearchBatch {
                        finished: false,
                        ..
                    })
                ));
                assert!(worker.busy);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        worker.cancel();
        submit(&mut worker, "email");
        let mut found = Vec::new();
        while worker.busy {
            if let Some(event) = worker.poll() {
                match event.result.unwrap() {
                    Output::SearchBatch { ids, .. } => found.extend(ids),
                    _ => panic!("Wrong operation output"),
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            found,
            doc.search("email", false, false, SearchScope::Keys)
                .unwrap()
        );
    }
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
