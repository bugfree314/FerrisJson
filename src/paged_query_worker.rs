//! Independent query producer for paged providers; one pending request and one state slot.
use crate::engine::{
    budget::{BudgetManager, Reservation, Resource},
    file_source::{Cancellation, Priority, with_io_context},
    query::{QueryEngine, QueryRequest, stream::{Event, Input}},
    results::{ResultStore, State},
};
use eframe::egui;
use std::{sync::{Arc, Condvar, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}}, thread::JoinHandle, time::{Duration, Instant}};

struct Request { generation: u64, query: QueryRequest, _lease: Arc<Reservation> }
struct Inbox {
    pending: Option<Request>, job: Option<(Request, Input)>, active: Option<u64>,
    store: Option<ResultStore>, expression: String, error: String, elapsed: Option<Duration>,
    _lease: Option<Arc<Reservation>>,
}
struct Shared {
    inbox: Mutex<Inbox>, changed: Condvar, epoch: Arc<AtomicU64>, stop: Arc<AtomicBool>,
    budget: BudgetManager, ctx: egui::Context, wake: Arc<dyn Fn() + Send + Sync>,
}
#[derive(Clone)]
pub struct QueryHub(Arc<Shared>);
pub struct Snapshot {
    pub generation: u64, pub expression: String, pub state: Option<State>, pub waiting: bool,
    pub error: String, pub elapsed: Option<Duration>,
    _lease: Option<Arc<Reservation>>,
}
impl QueryHub {
    pub fn new(budget: BudgetManager, stop: Arc<AtomicBool>, ctx: egui::Context, wake: Arc<dyn Fn() + Send + Sync>) -> (Self, JoinHandle<()>) {
        let hub = Self(Arc::new(Shared {
            inbox: Mutex::new(Inbox { pending: None, job: None, active: None, store: None, expression: String::new(), error: String::new(), elapsed: None, _lease: None }),
            changed: Condvar::new(), epoch: Arc::new(AtomicU64::new(0)), stop, budget, ctx, wake,
        }));
        let worker = hub.clone();
        let thread = std::thread::spawn(move || loop {
            let (request, input) = {
                let mut b = worker.0.inbox.lock().unwrap();
                while b.job.is_none() && !worker.0.stop.load(Ordering::Relaxed) { b = worker.0.changed.wait(b).unwrap(); }
                if worker.0.stop.load(Ordering::Relaxed) { return; }
                let job = b.job.take().unwrap(); b.active = Some(job.0.generation); job
            };
            let generation = request.generation;
            let cancel = || worker.0.stop.load(Ordering::Relaxed) || worker.generation() != generation;
            let started = Instant::now();
            let result = with_io_context(Priority::Background, Cancellation::Any(vec![
                Cancellation::Flag(worker.0.stop.clone()), Cancellation::Generation(worker.0.epoch.clone(), generation),
            ]), || QueryEngine::run(input, &request.query, generation, &cancel, &mut |event| {
                let mut b = worker.0.inbox.lock().unwrap();
                if cancel() { return Err("Query cancelled".into()); }
                match event {
                    Event::Started(store) => b.store = Some(store),
                    Event::Progress { visited } => if let Some(store) = &b.store { store.progress(visited); },
                    Event::Batch { .. } | Event::Finished(_) => {}
                }
                drop(b); worker.wake(); Ok(())
            }));
            let mut b = worker.0.inbox.lock().unwrap();
            if b.active == Some(generation) { b.active = None; }
            if !cancel() {
                b.elapsed = Some(started.elapsed());
                if let Err(error) = result { b.error = bounded(error); if let Some(store) = &b.store { store.fail(b.error.clone()); } }
            }
            drop(b); worker.wake();
        });
        (hub, thread)
    }
    fn wake(&self) { (self.0.wake)(); self.0.ctx.request_repaint(); }
    pub fn generation(&self) -> u64 { self.0.epoch.load(Ordering::Relaxed) }
    pub fn request(&self, query: QueryRequest) -> Result<(), String> {
        let bytes = query.expression.capacity().checked_mul(4).and_then(|n| n.checked_add(64 * 1024)).ok_or("Query request size overflow")?;
        let lease = Arc::new(self.0.budget.reserve(Resource::Memory, bytes, "paged query request and presentation")?);
        let mut b = self.0.inbox.lock().unwrap();
        self.0.epoch.fetch_add(1, Ordering::Relaxed);
        if let Some(store) = &b.store { store.cancel(); }
        b.expression = query.expression.clone(); b.error.clear(); b.elapsed = None; b.store = None; b.job = None;
        b._lease = Some(lease.clone());
        b.pending = Some(Request { generation: self.generation(), query, _lease: lease });
        drop(b); self.0.changed.notify_all(); self.wake(); Ok(())
    }
    pub fn pending(&self) -> bool { self.0.inbox.lock().unwrap().pending.is_some() }
    pub fn dispatch(&self, input: Input) {
        let mut b = self.0.inbox.lock().unwrap();
        if let Some(request) = b.pending.take() { b.job = Some((request, input)); }
        drop(b); self.0.changed.notify_all();
    }
    pub fn fail_pending(&self, error: String) {
        let mut b = self.0.inbox.lock().unwrap(); b.pending = None; b.error = bounded(error); drop(b); self.wake();
    }
    pub fn store(&self) -> Option<ResultStore> { self.0.inbox.lock().unwrap().store.clone() }
    pub fn poll(&self) -> Snapshot {
        let b = self.0.inbox.lock().unwrap();
        Snapshot { generation: self.generation(), expression: b.expression.clone(), state: b.store.as_ref().map(ResultStore::state),
            waiting: b.pending.is_some() || b.job.is_some() || b.active == Some(self.generation()), error: b.error.clone(), elapsed: b.elapsed, _lease: b._lease.clone() }
    }
    pub fn cancel(&self, clear: bool) {
        let mut b = self.0.inbox.lock().unwrap(); self.0.epoch.fetch_add(1, Ordering::Relaxed);
        b.pending = None; b.job = None;
        if let Some(store) = &b.store { store.cancel(); }
        if clear { b.store = None; b.expression = String::new(); b.error = String::new(); b.elapsed = None; b._lease = None; }
        else if b.store.is_none() { b.error = "Query cancelled before results were published".into(); }
        drop(b); self.0.changed.notify_all(); self.wake();
    }
    pub fn stop(&self) { self.0.changed.notify_all(); }
}
fn bounded(mut text: String) -> String {
    if text.len() > 16 * 1024 { let mut n = 16 * 1024; while !text.is_char_boundary(n) { n -= 1; } text.truncate(n); text.push('…'); }
    text
}
