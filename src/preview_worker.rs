//! Independent interactive lane: latest selection, latest viewport, bounded messages.
use crate::{
    document::Document,
    engine::{
        budget::{Reservation, Resource},
        file_source::{Cancellation, Priority, check_cancelled, with_io_context},
        session::{NodeRef, SessionId, TaskId},
    },
};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU64, Ordering},
};

pub const ROW_BATCH: usize = 32;
pub struct RowLabel {
    pub id: usize,
    pub key: String,
    pub preview: String,
}
pub enum Output {
    Selection {
        reference: NodeRef,
        preview: String,
        path: String,
    },
    Rows(Vec<RowLabel>),
}
enum Operation {
    Selection(NodeRef),
    Rows(Vec<NodeRef>),
}
struct Request {
    doc: Arc<Document>,
    task: TaskId,
    operation: Operation,
    lease: Reservation,
}
pub struct Completion {
    pub task: TaskId,
    pub selection: bool,
    pub result: Result<Output, String>,
    _lease: Reservation,
}
#[derive(Default)]
struct Inbox {
    selection: Option<Request>,
    rows: Option<Request>,
    selection_result: Option<Completion>,
    rows_result: Option<Completion>,
    stopped: bool,
}
pub struct PreviewWorker {
    inbox: Arc<(Mutex<Inbox>, Condvar)>,
    selection_epoch: Arc<AtomicU64>,
    rows_epoch: Arc<AtomicU64>,
    session: Option<SessionId>,
    pub selection_busy: bool,
    pub rows_busy: bool,
}
impl PreviewWorker {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let inbox = Arc::new((Mutex::new(Inbox::default()), Condvar::new()));
        let selection_epoch = Arc::new(AtomicU64::new(0));
        let rows_epoch = Arc::new(AtomicU64::new(0));
        let shared = Arc::clone(&inbox);
        let selected = Arc::clone(&selection_epoch);
        let viewport = Arc::clone(&rows_epoch);
        std::thread::spawn(move || {
            loop {
                let request = {
                    let mut inbox = shared.0.lock().unwrap();
                    while inbox.selection.is_none() && inbox.rows.is_none() && !inbox.stopped {
                        inbox = shared.1.wait(inbox).unwrap();
                    }
                    if inbox.stopped {
                        return;
                    }
                    inbox
                        .selection
                        .take()
                        .or_else(|| inbox.rows.take())
                        .unwrap()
                };
                let selection = matches!(request.operation, Operation::Selection(_));
                let epoch = if selection { &selected } else { &viewport };
                let stale = || epoch.load(Ordering::Relaxed) != request.task.generation;
                if stale() {
                    continue;
                }
                let selection_at_start = selected.load(Ordering::Relaxed);
                let mut tokens = vec![Cancellation::Generation(
                    Arc::clone(epoch),
                    request.task.generation,
                )];
                if !selection {
                    tokens.push(Cancellation::Generation(
                        Arc::clone(&selected),
                        selection_at_start,
                    ));
                }
                let result =
                    with_io_context(Priority::Interactive, Cancellation::Any(tokens), || {
                        match request.operation {
                            Operation::Selection(reference) => {
                                request.doc.resolve(reference)?;
                                let path = request.doc.display_path_ref(reference)?;
                                let preview = request.doc.preview_ref(reference)?;
                                check_cancelled().map_err(|e| e.to_string())?;
                                Ok(Output::Selection {
                                    reference,
                                    path,
                                    preview,
                                })
                            }
                            Operation::Rows(references) => {
                                let mut labels = Vec::with_capacity(references.len());
                                for reference in references {
                                    // A new selection interrupts a row batch between values/reads.
                                    if check_cancelled().is_err() {
                                        break;
                                    }
                                    let id = request.doc.resolve(reference)?;
                                    let source_key = request.doc.key(id);
                                    let mut characters = source_key.chars();
                                    let mut key: String = characters.by_ref().take(160).collect();
                                    if characters.next().is_some() {
                                        key.push('…');
                                    }
                                    let preview = request.doc.row_preview(id);
                                    if check_cancelled().is_err() {
                                        break;
                                    }
                                    labels.push(RowLabel { id, key, preview });
                                }
                                Ok(Output::Rows(labels))
                            }
                        }
                    });
                drop(request.doc);
                if stale() {
                    continue;
                }
                let completion = Completion {
                    task: request.task,
                    selection,
                    result,
                    _lease: request.lease,
                };
                // Each lane owns one replaceable completion slot. A slow UI cannot
                // block selection processing behind a viewport completion.
                let mut inbox = shared.0.lock().unwrap();
                if inbox.stopped {
                    return;
                }
                if selection {
                    inbox.selection_result = Some(completion);
                } else {
                    inbox.rows_result = Some(completion);
                }
                drop(inbox);
                ctx.request_repaint();
            }
        });
        Self {
            inbox,
            selection_epoch,
            rows_epoch,
            session: None,
            selection_busy: false,
            rows_busy: false,
        }
    }
    pub fn select(&mut self, doc: Arc<Document>, reference: NodeRef) -> Result<(), String> {
        doc.resolve(reference)?;
        if self
            .session
            .is_some_and(|session| session != doc.session_id())
        {
            self.cancel();
        }
        let generation = self.selection_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut inbox = self.inbox.0.lock().unwrap();
            inbox.selection = None;
            inbox.selection_result = None;
        }
        self.selection_busy = false;
        let lease = doc.budget().reserve(
            Resource::Memory,
            512 * 1024,
            "interactive selection request and result",
        )?;
        self.session = Some(doc.session_id());
        let mut inbox = self.inbox.0.lock().unwrap();
        inbox.selection_result = None;
        inbox.selection = Some(Request {
            task: TaskId {
                session: doc.session_id(),
                generation,
            },
            doc,
            operation: Operation::Selection(reference),
            lease,
        });
        self.selection_busy = true;
        self.inbox.1.notify_one();
        Ok(())
    }
    pub fn rows(&mut self, doc: Arc<Document>, references: Vec<NodeRef>) -> Result<(), String> {
        if references.len() > ROW_BATCH || references.capacity() > ROW_BATCH {
            return Err("Viewport batch exceeds 32 rows".into());
        }
        for &reference in &references {
            doc.resolve(reference)?;
        }
        if self
            .session
            .is_some_and(|session| session != doc.session_id())
        {
            self.cancel();
        }
        self.cancel_rows();
        let lease =
            doc.budget()
                .reserve(Resource::Memory, 512 * 1024, "viewport request and result")?;
        let generation = self.rows_epoch.load(Ordering::Relaxed);
        self.session = Some(doc.session_id());
        let mut inbox = self.inbox.0.lock().unwrap();
        inbox.rows_result = None;
        inbox.rows = Some(Request {
            task: TaskId {
                session: doc.session_id(),
                generation,
            },
            doc,
            operation: Operation::Rows(references),
            lease,
        });
        self.rows_busy = true;
        self.inbox.1.notify_one();
        Ok(())
    }
    pub fn cancel_rows(&mut self) {
        self.rows_epoch.fetch_add(1, Ordering::Relaxed);
        let mut inbox = self.inbox.0.lock().unwrap();
        inbox.rows = None;
        inbox.rows_result = None;
        self.rows_busy = false;
    }
    pub fn cancel(&mut self) {
        self.selection_epoch.fetch_add(1, Ordering::Relaxed);
        self.rows_epoch.fetch_add(1, Ordering::Relaxed);
        let mut inbox = self.inbox.0.lock().unwrap();
        inbox.selection = None;
        inbox.rows = None;
        inbox.selection_result = None;
        inbox.rows_result = None;
        self.selection_busy = false;
        self.rows_busy = false;
        self.session = None;
    }
    pub fn poll(&mut self) -> Option<Completion> {
        let mut inbox = self.inbox.0.lock().unwrap();
        while let Some(result) = inbox
            .selection_result
            .take()
            .or_else(|| inbox.rows_result.take())
        {
            let epoch = if result.selection {
                &self.selection_epoch
            } else {
                &self.rows_epoch
            };
            if result.task.generation == epoch.load(Ordering::Relaxed)
                && Some(result.task.session) == self.session
            {
                if result.selection {
                    self.selection_busy = false;
                } else {
                    self.rows_busy = false;
                }
                return Some(result);
            }
        }
        None
    }
}
impl Drop for PreviewWorker {
    fn drop(&mut self) {
        self.cancel();
        self.inbox.0.lock().unwrap().stopped = true;
        self.inbox.1.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn wait(worker: &mut PreviewWorker) -> Completion {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = worker.poll() {
                return result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn selection_and_viewport_results_are_bounded_and_reject_other_sessions() {
        let doc = Arc::new(Document::parse(r#"{"one":"first","two":"last"}"#).unwrap());
        let mut worker = PreviewWorker::new(eframe::egui::Context::default());
        worker
            .select(Arc::clone(&doc), doc.node_ref(1).unwrap())
            .unwrap();
        worker
            .select(Arc::clone(&doc), doc.node_ref(2).unwrap())
            .unwrap();
        assert!(
            matches!(wait(&mut worker).result, Ok(Output::Selection { reference, preview, .. }) if reference.address == 2 && preview == "\"last\"")
        );
        let other = Document::parse("null").unwrap();
        assert!(
            worker
                .select(Arc::clone(&doc), other.node_ref(0).unwrap())
                .is_err()
        );
        assert!(
            worker
                .rows(
                    Arc::clone(&doc),
                    vec![doc.node_ref(0).unwrap(); ROW_BATCH + 1]
                )
                .is_err()
        );
        worker
            .rows(
                Arc::clone(&doc),
                vec![doc.node_ref(1).unwrap(), doc.node_ref(2).unwrap()],
            )
            .unwrap();
        assert!(
            matches!(wait(&mut worker).result, Ok(Output::Rows(rows)) if rows.len() == 2 && rows[1].preview == "\"last\"")
        );
        worker
            .select(Arc::clone(&doc), doc.node_ref(1).unwrap())
            .unwrap();
        worker.cancel();
        assert!(worker.poll().is_none());
    }
    #[test]
    fn queued_completion_releases_document_and_retains_only_its_budget() {
        let manager = crate::engine::budget::BudgetManager::new(crate::engine::budget::Limits {
            memory: 8 * 1024 * 1024,
            disk: 1024,
        });
        let doc = Arc::new(
            Document::parse_owned_recover_with_budget(r#"{"value":1}"#.into(), manager.clone())
                .map_err(|(error, _)| error)
                .unwrap(),
        );
        let weak = Arc::downgrade(&doc);
        let mut worker = PreviewWorker::new(eframe::egui::Context::default());
        worker
            .select(Arc::clone(&doc), doc.node_ref(1).unwrap())
            .unwrap();
        drop(doc);
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.inbox.0.lock().unwrap().selection_result.is_none() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(weak.upgrade().is_none());
        assert_eq!(manager.usage().memory, 512 * 1024);
        worker.cancel();
        assert_eq!(manager.usage().memory, 0);
    }
}
