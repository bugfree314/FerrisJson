//! Independent bounded production and result-page/clipboard lanes for JSONPath.
use crate::{
    document::{Document, NodeIds}, engine::{budget::{Reservation, Resource}, file_source::Priority,
    query::{QueryEngine, QueryRequest, stream::{Event, Input}}, results::{ResultStore, Status}},
    export::{CopyError, Selection}, query_worker::QueryOutput, task_worker::{TaskWorker, Work},
};
use eframe::egui;
use std::{sync::{Arc, Mutex}, time::{Duration, Instant}};

pub const PAGE: u64 = 16;
#[derive(Default)]
struct Published { store: Option<ResultStore> }
struct Job { request: QueryRequest, source: Arc<Document>, shared: Arc<Mutex<Published>>, generation: u64, ctx: egui::Context }
impl Work for Job {
    type Output = ResultStore;
    fn input_bytes(&self) -> usize { self.request.expression.capacity().saturating_mul(2) }
    fn execute(self, _: &Document, cancel: &dyn Fn() -> bool) -> Result<ResultStore, String> {
        QueryEngine::run(Input::Document(self.source), &self.request, self.generation, cancel, &mut |event| {
            if cancel() { return Err("Query cancelled".into()); }
            let mut state = self.shared.lock().unwrap();
            if cancel() { return Err("Query cancelled".into()); }
            match event {
                Event::Started(store) => state.store = Some(store),
                Event::Progress { visited } => if let Some(store) = &state.store { store.progress(visited); },
                Event::Batch { .. } | Event::Finished(_) => {}
            }
            drop(state); self.ctx.request_repaint(); Ok(())
        })
    }
}
enum Read {
    Page { store: ResultStore, start: u64, count: u64 },
    Copy { selection: Selection, limit: Option<usize> },
}
pub struct Clipboard { pub selection: Selection, pub result: Result<String, CopyError>, _lease: Reservation }
enum ReadOutput { Page { page: QueryOutput, start: u64, count: u64 }, Clipboard(Clipboard) }
impl Work for Read {
    type Output = ReadOutput;
    fn input_bytes(&self) -> usize { 0 }
    fn priority(&self) -> Priority { match self { Self::Page { .. } => Priority::Interactive, Self::Copy { .. } => Priority::User } }
    fn execute(self, doc: &Document, cancel: &dyn Fn() -> bool) -> Result<ReadOutput, String> {
        match self {
            Self::Page { store, start, count } => {
                if store.session() != doc.session_id() { return Err("Query page belongs to another document".into()); }
                let mut ids = NodeIds::new(doc.budget()); ids.reserve_exact(count as usize)?;
                for index in start..start + count {
                    let node = store.node(index, cancel)?;
                    ids.push(doc.resolve(node.reference)? as u32)?;
                }
                if cancel() { return Err("Query page cancelled".into()); }
                let preview = doc.results_preview(&ids);
                Ok(ReadOutput::Page { page: QueryOutput { expression: String::new(), ids, preview }, start, count })
            }
            Self::Copy { selection, limit } => {
                let result = selection.json_cancellable(doc, limit, cancel);
                let bytes = result.as_ref().map_or(0, |text| text.capacity());
                let lease = doc.budget().reserve(Resource::Memory, bytes, "query clipboard transfer")?;
                Ok(ReadOutput::Clipboard(Clipboard { selection, result, _lease: lease }))
            }
        }
    }
}
pub struct Snapshot {
    pub expression: String, pub store: Option<ResultStore>, pub page: Option<QueryOutput>, pub start: u64,
    pub first: Option<usize>, pub elapsed: Option<Duration>, pub error: Option<String>, pub clipboard: Option<Clipboard>,
}
pub struct QueryWorker {
    producer: TaskWorker<Job>, reader: TaskWorker<Read>, shared: Arc<Mutex<Published>>, ctx: egui::Context,
    doc: Option<Arc<Document>>, expression: String, generation: u64, start: u64, rendered: Option<(u64, u64)>,
    reveal: bool, started: Option<Instant>, elapsed: Option<Duration>, error: Option<String>, pub busy: bool,
}
impl QueryWorker {
    pub fn new(ctx: egui::Context) -> Self {
        Self { producer: TaskWorker::new(ctx.clone()), reader: TaskWorker::new(ctx.clone()), shared: Arc::new(Mutex::new(Published::default())), ctx,
            doc: None, expression: String::new(), generation: 0, start: 0, rendered: None, reveal: true,
            started: None, elapsed: None, error: None, busy: false }
    }
    pub fn cancel(&mut self) {
        self.producer.cancel(); self.reader.cancel();
        if let Some(store) = self.shared.lock().unwrap().store.take() { store.cancel(); }
        self.doc = None; self.started = None; self.elapsed = None; self.error = None; self.rendered = None; self.busy = false;
    }
    pub fn submit(&mut self, doc: Arc<Document>, request: QueryRequest) -> Result<(), String> {
        self.cancel(); self.generation = self.generation.wrapping_add(1);
        self.expression = request.expression.clone(); self.start = 0; self.reveal = true;
        self.producer.submit(doc.clone(), Job { request, source: doc.clone(), shared: self.shared.clone(), generation: self.generation, ctx: self.ctx.clone() })?;
        self.doc = Some(doc); self.started = Some(Instant::now()); self.busy = true; Ok(())
    }
    pub fn cancel_scan(&mut self) {
        self.producer.cancel();
        if let Some(store) = &self.shared.lock().unwrap().store { store.cancel(); }
        else { self.error = Some("Query cancelled before results were published".into()); }
    }
    pub fn page(&mut self, start: u64) {
        self.reader.cancel(); self.start = start; self.rendered = None; self.reveal = true; self.error = None; self.busy = true;
    }
    pub fn copy(&mut self, selection: Selection, limit: Option<usize>) -> Result<(), String> {
        let doc = self.doc.as_ref().ok_or("No query document")?;
        self.reader.submit(doc.clone(), Read::Copy { selection, limit })?; self.busy = true; Ok(())
    }
    pub fn poll(&mut self) -> Option<Snapshot> {
        self.started?;
        if let Some(result) = self.producer.poll() {
            if let Err(error) = result.result { self.error = Some(error); }
            self.elapsed = Some(result.elapsed);
        }
        let store = self.shared.lock().unwrap().store.clone();
        let state = store.as_ref().map(ResultStore::state);
        let count = state.as_ref().map_or(0, |s| s.count);
        let scanning = self.producer.busy || state.as_ref().is_some_and(|s| s.status == Status::Scanning);
        let key = (self.start, count.saturating_sub(self.start).min(PAGE));
        let (mut page, mut first, mut clipboard) = (None, None, None);
        if let Some(result) = self.reader.poll() {
            match result.result {
                Ok(ReadOutput::Page { page: mut output, start, count }) if start == self.start => {
                    output.expression = self.expression.clone();
                    self.rendered = Some((start, count));
                    if self.reveal { first = output.ids.first().map(|id| *id as usize); self.reveal = false; }
                    page = Some(output);
                }
                Ok(ReadOutput::Clipboard(result)) => clipboard = Some(result),
                Err(error) => { self.error = Some(error); self.rendered = Some(key); }
                _ => {}
            }
        }
        if !self.reader.busy && self.rendered != Some(key) && (key.1 > 0 || !scanning) {
            if let (Some(store), Some(doc)) = (&store, &self.doc) {
                if let Err(error) = self.reader.submit(doc.clone(), Read::Page { store: store.clone(), start: key.0, count: key.1 }) {
                    self.error = Some(error); self.rendered = Some(key);
                }
            }
        }
        if !scanning && self.elapsed.is_none() { self.elapsed = self.started.map(|s| s.elapsed()); }
        self.busy = scanning || self.reader.busy;
        let error = self.error.clone().or_else(|| state.as_ref().and_then(|s| (!s.error.is_empty()).then(|| s.error.clone())));
        Some(Snapshot { expression: self.expression.clone(), store, page, start: self.start, first, elapsed: self.elapsed, error, clipboard })
    }
}
impl Drop for QueryWorker { fn drop(&mut self) { self.cancel(); } }
