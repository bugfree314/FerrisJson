//! One bounded search lane per document. Events carry ranges, never path vectors.
use crate::engine::{
    budget::{BudgetManager, Reservation, Resource},
    file_source::{Cancellation, Priority, with_io_context},
    search::{Event, Input, Query, ResultStore, State, Status},
};
use crate::{
    document::{Document, NodeIds},
    task_worker::{TaskWorker, Work},
};
use eframe::egui;

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
};
/// The default viewer keeps its collected result adapter until the result-store migration.
pub type SearchRequest = Query;
pub type SearchWorker = TaskWorker<SearchRequest>;

impl Work for SearchRequest {
    type Output = NodeIds;

    fn input_bytes(&self) -> usize {
        self.term.capacity()
    }

    fn execute(self, doc: &Document, cancelled: &dyn Fn() -> bool) -> Result<NodeIds, String> {
        doc.search_cancellable(&self.term, self.case, self.regex, self.scope, cancelled)
    }
}

#[derive(Clone)]
pub struct Snapshot {
    pub generation: u64,
    pub state: Option<State>,
    pub waiting: bool,
    pub error: String,
    pub cursor: Option<u64>,
    pub queue_peak: usize,
}
#[derive(Clone, Copy)]
enum Target {
    Absolute(u64),
    FromEnd(u64),
}
struct Inbox {
    pending: Option<(u64, Query)>,
    job: Option<(u64, Query, Input)>,
    active: Option<u64>,
    store: Option<ResultStore>,
    events: VecDeque<Event>,
    error: String,
    cursor: Option<u64>,
    target: Option<Target>,
    queue_peak: usize,
    _lease: Reservation,
}
struct Shared {
    inbox: Mutex<Inbox>,
    changed: Condvar,
    epoch: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    ctx: egui::Context,
    wake: Arc<dyn Fn() + Send + Sync>,
}
#[derive(Clone)]
pub struct SearchHub(Arc<Shared>);
impl SearchHub {
    pub fn new(
        budget: &BudgetManager,
        stop: Arc<AtomicBool>,
        ctx: egui::Context,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(Self, JoinHandle<()>), String> {
        let shared = Arc::new(Shared {
            inbox: Mutex::new(Inbox {
                pending: None,
                job: None,
                active: None,
                store: None,
                events: VecDeque::with_capacity(4),
                error: String::new(),
                cursor: None,
                target: None,
                queue_peak: 0,
                _lease: budget.reserve(
                    Resource::Memory,
                    512 * 1024,
                    "bounded search lane requests/events",
                )?,
            }),
            changed: Condvar::new(),
            epoch: Arc::new(AtomicU64::new(0)),
            stop,
            ctx,
            wake,
        });
        let hub = Self(shared);
        let worker = hub.clone();
        let thread = std::thread::spawn(move || {
            loop {
                let (generation, query, mut input) = {
                    let mut b = worker.0.inbox.lock().unwrap();
                    while b.job.is_none() && !worker.0.stop.load(Ordering::Relaxed) {
                        b = worker.0.changed.wait(b).unwrap();
                    }
                    if worker.0.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let job = b.job.take().unwrap();
                    b.active = Some(job.0);
                    job
                };
                let cancel = || {
                    worker.0.stop.load(Ordering::Relaxed)
                        || worker.0.epoch.load(Ordering::Relaxed) != generation
                };
                let result = with_io_context(
                    Priority::Background,
                    Cancellation::Any(vec![
                        Cancellation::Flag(worker.0.stop.clone()),
                        Cancellation::Generation(worker.0.epoch.clone(), generation),
                    ]),
                    || {
                        input.run(query, generation, &cancel, &mut |event| {
                            if cancel() {
                                return Err("Search event cancelled".into());
                            }
                            let mut b = worker.0.inbox.lock().unwrap();
                            if matches!(event, Event::Progress { .. }) {
                                b.events.retain(|e| !matches!(e, Event::Progress { .. }));
                            }
                            while b.events.len() == 4 && !cancel() {
                                b = worker.0.changed.wait(b).unwrap();
                            }
                            if cancel() {
                                return Err("Search event cancelled".into());
                            }
                            if let Event::Started(store) = &event {
                                b.store = Some(store.clone());
                            }
                            b.events.push_back(event);
                            b.queue_peak = b.queue_peak.max(b.events.len());
                            drop(b);
                            (worker.0.wake)();
                            worker.0.ctx.request_repaint();
                            Ok(())
                        })
                    },
                );
                {
                    let mut b = worker.0.inbox.lock().unwrap();
                    if b.active == Some(generation) {
                        b.active = None;
                    }
                }
                if let Err(e) = result
                    && !cancel()
                {
                    let mut b = worker.0.inbox.lock().unwrap();
                    b.error = short(e);
                    if let Some(s) = &b.store {
                        s.fail(b.error.clone());
                    }
                    drop(b);
                    (worker.0.wake)();
                    worker.0.ctx.request_repaint();
                }
            }
        });
        Ok((hub, thread))
    }
    pub fn request(&self, query: Query, reverse: bool) -> Result<(), String> {
        if query.term.len() > 64 * 1024 {
            return Err("Search term exceeds 64 KiB".into());
        }
        let mut b = self.0.inbox.lock().unwrap();
        let generation = self.0.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        b.job = None;
        b.store = None;
        b.events.clear();
        b.error.clear();
        b.cursor = None;
        b.target = Some(if reverse {
            Target::FromEnd(1)
        } else {
            Target::Absolute(0)
        });
        b.pending = Some((generation, query));
        drop(b);
        self.0.changed.notify_all();
        Ok(())
    }
    pub fn pending(&self) -> bool {
        self.0.inbox.lock().unwrap().pending.is_some()
    }
    pub fn dispatch(&self, input: Input) {
        let mut b = self.0.inbox.lock().unwrap();
        if let Some((g, q)) = b.pending.take() {
            b.job = Some((g, q, input));
        }
        drop(b);
        self.0.changed.notify_all();
    }
    pub fn clear(&self) {
        let mut b = self.0.inbox.lock().unwrap();
        self.0.epoch.fetch_add(1, Ordering::Relaxed);
        b.pending = None;
        b.job = None;
        b.store = None;
        b.events.clear();
        b.target = None;
        b.cursor = None;
        b.error.clear();
        drop(b);
        self.0.changed.notify_all();
        (self.0.wake)();
    }
    pub fn stop(&self) {
        self.0.changed.notify_all();
    }
    pub fn cancel(&self) {
        let mut b = self.0.inbox.lock().unwrap();
        self.0.epoch.fetch_add(1, Ordering::Relaxed);
        b.pending = None;
        b.job = None;
        b.events.clear();
        b.target = None;
        if let Some(s) = &b.store {
            s.cancel();
        }
        drop(b);
        self.0.changed.notify_all();
        (self.0.wake)();
    }
    pub fn generation(&self) -> u64 {
        self.0.epoch.load(Ordering::Relaxed)
    }
    pub fn store(&self) -> Option<ResultStore> {
        self.0.inbox.lock().unwrap().store.clone()
    }
    pub fn poll(&self) -> Snapshot {
        let mut b = self.0.inbox.lock().unwrap();
        b.events.clear();
        let state = b.store.as_ref().map(ResultStore::state);
        let s = Snapshot {
            generation: self.0.epoch.load(Ordering::Relaxed),
            waiting: b.pending.is_some() || b.job.is_some() || b.active == Some(self.generation()),
            state,
            error: b.error.clone(),
            cursor: b.cursor,
            queue_peak: b.queue_peak,
        };
        drop(b);
        self.0.changed.notify_all();
        s
    }
    pub fn select(&self, index: u64) {
        self.0.inbox.lock().unwrap().target = Some(Target::Absolute(index));
        (self.0.wake)();
    }
    pub fn next(&self, forward: bool, count: u64) {
        let mut b = self.0.inbox.lock().unwrap();
        let count = count.max(1);
        let base = b.target.or(b.cursor.map(Target::Absolute));
        b.target = Some(match (base, forward) {
            (Some(Target::Absolute(i)), true) => Target::Absolute(i.saturating_add(count)),
            (Some(Target::Absolute(i)), false) if i >= count => Target::Absolute(i - count),
            (Some(Target::Absolute(i)), false) => Target::FromEnd(count - i),
            (Some(Target::FromEnd(n)), false) => Target::FromEnd(n.saturating_add(count)),
            (Some(Target::FromEnd(n)), true) if n > count => Target::FromEnd(n - count),
            (Some(Target::FromEnd(n)), true) => Target::Absolute(count - n),
            (None, true) => Target::Absolute(count - 1),
            (None, false) => Target::FromEnd(count),
        });
        drop(b);
        (self.0.wake)();
    }
    pub fn cancel_selection(&self) {
        self.0.inbox.lock().unwrap().target = None;
    }
    fn resolve(b: &mut Inbox) -> Option<u64> {
        let target = b.target?;
        let state = b.store.as_ref()?.state();
        if state.count == 0 {
            if state.status != Status::Scanning {
                b.target = None;
                b.error = "No committed matches".into();
            }
            return None;
        }
        match target {
            Target::Absolute(i) if i < state.count => Some(i),
            Target::Absolute(i) if state.status == Status::Completed => Some(i % state.count),
            Target::FromEnd(n) if state.status == Status::Completed => {
                Some((state.count - n % state.count) % state.count)
            }
            _ if state.status == Status::Scanning => None,
            _ => {
                b.target = None;
                b.error = "Requested match unavailable in partial results".into();
                None
            }
        }
    }
    pub fn selection_ready(&self) -> bool {
        Self::resolve(&mut self.0.inbox.lock().unwrap()).is_some()
    }
    pub fn take_selection(&self) -> Option<(u64, u64, ResultStore)> {
        let mut b = self.0.inbox.lock().unwrap();
        let index = Self::resolve(&mut b)?;
        Some((
            self.0.epoch.load(Ordering::Relaxed),
            index,
            b.store.clone()?,
        ))
    }
    pub fn accept(&self, generation: u64, index: u64) {
        let mut b = self.0.inbox.lock().unwrap();
        if self.0.epoch.load(Ordering::Relaxed) == generation {
            if Self::resolve(&mut b) == Some(index) {
                b.target = None;
            }
            b.cursor = Some(index);
        }
    }
}
fn short(mut s: String) -> String {
    if s.len() > 16 * 1024 {
        let mut n = 16 * 1024;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
        s.push('…');
    }
    s
}
