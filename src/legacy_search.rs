//! Default viewer adapter for SearchHub. Disk reads run on a separate bounded lane.
use crate::{
    document::Document,
    engine::{
        results::{ResultStore, Status},
        budget::{Reservation, Resource},
        file_source::Priority,
        search::{Input, Query},
        session::SessionId,
    },
    search_worker::SearchHub,
    task_worker::{TaskWorker, Work},
};
use eframe::egui;
use std::{
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub type SearchRequest = Query;

#[derive(Clone, Default)]
pub struct SearchResults {
    pub store: Option<ResultStore>,
    visible_hits: Option<Arc<HitPage>>,
}

struct HitPage {
    ids: Vec<u32>,
    _lease: Reservation,
}

impl SearchResults {
    pub fn len(&self) -> usize {
        self.store.as_ref().map_or(0, |s| s.state().count as usize)
    }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn contains(&self, id: u32) -> bool { self.visible_hits.as_ref().is_some_and(|p| p.ids.binary_search(&id).is_ok()) }
    pub fn completed(&self) -> bool {
        self.store.as_ref().is_some_and(|s| s.state().status == Status::Completed)
    }
}

enum Selection {
    Initial { origin: usize, reverse: bool },
    Ordinal(u64),
    None,
}

struct ReadRequest {
    store: ResultStore,
    selection: Selection,
    visible: Option<Arc<HitPage>>,
    revision: u64,
    count: u64,
    generation: u64,
}

struct ReadOutput {
    generation: u64,
    revision: u64,
    count: u64,
    selection: Option<(u64, usize)>,
    visible_hits: Arc<HitPage>,
}

/// Search-only optimization: legacy search results are in ascending node-ID order.
/// The shared result store makes no such ordering assumption.
fn bound(store: &ResultStore, count: u64, address: u64, upper: bool, cancel: &dyn Fn() -> bool) -> Result<u64, String> {
    let (mut lo, mut hi) = (0, count);
    while lo < hi {
        if cancel() { return Err("Result read cancelled".into()); }
        let mid = lo + (hi - lo) / 2;
        let found = store.node(mid, cancel)?.reference.address;
        if found < address || (upper && found == address) { lo = mid + 1; } else { hi = mid; }
    }
    Ok(lo)
}

impl Work for ReadRequest {
    type Output = ReadOutput;
    fn input_bytes(&self) -> usize { 0 }
    fn priority(&self) -> Priority { Priority::Interactive }
    fn execute(self, doc: &Document, cancel: &dyn Fn() -> bool) -> Result<ReadOutput, String> {
        if self.store.session() != doc.session_id() { return Err("Search results belong to another document".into()); }
        let state = self.store.state();
        let done = state.status != Status::Scanning && state.count == self.count;
        let index = match self.selection {
            Selection::Ordinal(index) => Some(index),
            Selection::Initial { origin, reverse: false } => {
                let index = bound(&self.store, self.count, origin as u64, true, cancel)?;
                if index < self.count { Some(index) }
                else if done && state.status == Status::Completed && self.count > 0 { Some(0) }
                else { None }
            }
            Selection::Initial { origin, reverse: true } => {
                let index = bound(&self.store, self.count, origin as u64, false, cancel)?;
                if index > 0 && (index < self.count || done) { Some(index - 1) }
                else if index == 0 && done && state.status == Status::Completed && self.count > 0 { Some(self.count - 1) }
                else { None }
            }
            Selection::None => None,
        };
        let selection = index.map(|i| -> Result<(u64, usize), String> {
            let node = self.store.node(i, cancel)?;
            Ok((i, doc.resolve(node.reference)?))
        }).transpose()?;
        let visible = self.visible.as_ref().map_or(&[][..], |p| p.ids.as_slice());
        let mut lease = doc.budget().reserve(Resource::Memory, visible.len() * std::mem::size_of::<u32>(), "search viewport membership page")?;
        let mut visible_hits = Vec::new();
        visible_hits.try_reserve_exact(visible.len()).map_err(|e| format!("Cannot allocate search viewport: {e}"))?;
        lease.resize(visible_hits.capacity() * std::mem::size_of::<u32>())?;
        for &id in visible {
            let index = bound(&self.store, self.count, id as u64, false, cancel)?;
            if index < self.count && self.store.node(index, cancel)?.reference.address == id as u64 {
                visible_hits.push(id);
            }
        }
        Ok(ReadOutput { generation: self.generation, revision: self.revision, count: self.count, selection, visible_hits: Arc::new(HitPage { ids: visible_hits, _lease: lease }) })
    }
}

pub struct Update {
    pub results: SearchResults,
    pub selection: Option<(u64, usize)>,
    pub elapsed: Option<Duration>,
    pub error: Option<String>,
}

pub struct SearchWorker {
    ctx: egui::Context,
    hub: Option<SearchHub>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    reader: TaskWorker<ReadRequest>,
    doc: Option<Arc<Document>>,
    session: Option<SessionId>,
    initial: Option<(usize, bool)>,
    results: SearchResults,
    visible: Option<Arc<HitPage>>,
    revision: u64,
    read_revision: Option<(u64, u64, bool)>,
    started: Option<Instant>,
    elapsed: Option<Duration>,
    read_error: Option<String>,
    pub busy: bool,
}

impl SearchWorker {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            ctx: ctx.clone(), hub: None, stop: Arc::new(AtomicBool::new(false)), thread: None,
            reader: TaskWorker::new(ctx), doc: None, session: None, initial: None,
            results: SearchResults::default(), visible: None, revision: 0,
            read_revision: None, started: None, elapsed: None, read_error: None, busy: false,
        }
    }
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(hub) = self.hub.take() { hub.clear(); hub.stop(); }
        if let Some(thread) = self.thread.take() { crate::shutdown::defer(thread); }
        self.reader.cancel();
    }
    pub fn cancel(&mut self) {
        if let Some(hub) = &self.hub { hub.clear(); }
        self.reader.cancel();
        self.doc = None;
        self.visible = None;
        self.initial = None;
        self.results = SearchResults::default();
        self.read_revision = None;
        self.started = None;
        self.elapsed = None;
        self.read_error = None;
        self.busy = false;
    }
    pub fn submit(&mut self, doc: Arc<Document>, query: Query, origin: usize, reverse: bool) -> Result<(), String> {
        self.cancel();
        if self.session != Some(doc.session_id()) {
            self.shutdown();
            self.stop = Arc::new(AtomicBool::new(false));
            let (hub, thread) = SearchHub::new(doc.budget(), self.stop.clone(), self.ctx.clone(), Arc::new(|| {}))?;
            self.hub = Some(hub);
            self.thread = Some(thread);
            self.session = Some(doc.session_id());
        }
        let hub = self.hub.as_ref().ok_or("Search lane unavailable")?;
        hub.request(query, reverse)?;
        hub.cancel_selection();
        hub.dispatch(Input::Legacy(doc.clone()));
        self.initial = Some((origin, reverse));
        self.doc = Some(doc);
        self.started = Some(Instant::now());
        self.busy = true;
        Ok(())
    }
    pub fn visible(&mut self, ids: &[u32]) {
        let Some(doc) = &self.doc else { return; };
        if self.visible.as_ref().is_some_and(|p| p.ids.as_slice() == ids) { return; }
        // Only the rendered viewport is retained, never the full hit list.
        // Old and replacement viewport pages are both charged during replacement.
        let replacement = (|| -> Result<Arc<HitPage>, String> {
            let bytes = ids.len().checked_mul(std::mem::size_of::<u32>()).ok_or("Viewport byte overflow")?;
            let mut lease = doc.budget().reserve(Resource::Memory, bytes, "search visible node page")?;
            let mut page = Vec::new();
            page.try_reserve_exact(ids.len()).map_err(|e| format!("Cannot allocate search viewport: {e}"))?;
            lease.resize(page.capacity() * std::mem::size_of::<u32>())?;
            page.extend_from_slice(ids);
            Ok(Arc::new(HitPage { ids: page, _lease: lease }))
        })();
        match replacement {
            Ok(page) => {
                self.visible = Some(page);
                self.results.visible_hits = None;
                self.revision = self.revision.wrapping_add(1);
            }
            Err(e) => self.read_error = Some(e),
        }
    }
    pub fn next(&mut self, backwards: bool, count: usize) {
        if let Some(hub) = &self.hub {
            self.initial = None;
            self.read_error = None;
            // Prevent an older selection read from overwriting the new target.
            self.reader.cancel();
            hub.next(!backwards, count as u64);
            self.read_revision = None;
            self.busy = true;
        }
    }
    pub fn cancel_scan(&mut self) {
        if let Some(hub) = &self.hub {
            hub.cancel();
            self.reader.cancel();
            self.initial = None;
            self.read_revision = None;
            self.busy = false;
        }
    }
    pub fn poll(&mut self) -> Option<Update> {
        self.started?;
        let hub = self.hub.as_ref()?;
        let snapshot = hub.poll();
        self.results.store = hub.store();
        let state = snapshot.state.as_ref();
        let count = state.map_or(0, |s| s.count);
        let scanning = state.is_some_and(|s| s.status == Status::Scanning) || snapshot.waiting;
        let mut error = (!snapshot.error.is_empty()).then_some(snapshot.error.clone());
        if let Some(s) = state && !s.error.is_empty() { error = Some(s.error.clone()); }
        let mut selection = None;
        if let Some(completion) = self.reader.poll() {
            match completion.result {
                Ok(read) if read.generation == snapshot.generation => {
                    if read.revision == self.revision { self.results.visible_hits = Some(read.visible_hits); }
                    if let Some((index, node)) = read.selection {
                        hub.accept(read.generation, index);
                        self.initial = None;
                        selection = Some((index, node));
                    }
                    self.read_revision = Some((read.count, read.revision, !scanning));
                }
                Err(e) => { self.read_error = Some(e); self.initial = None; hub.cancel_selection(); }
                _ => {}
            }
        }
        if !scanning && count == 0 { self.initial = None; hub.cancel_selection(); }
        let target = hub.take_selection().map(|(_, index, _)| index);
        let key = (count, self.revision, !scanning);
        if self.read_error.is_none() && !self.reader.busy && count > 0 && (target.is_some() || self.read_revision != Some(key)) {
            if let (Some(store), Some(doc)) = (&self.results.store, &self.doc) {
                let selection = if let Some((origin, reverse)) = self.initial {
                    Selection::Initial { origin, reverse }
                } else { target.map_or(Selection::None, Selection::Ordinal) };
                if let Err(e) = self.reader.submit(doc.clone(), ReadRequest {
                    store: store.clone(), selection, visible: self.visible.clone(), revision: self.revision, count, generation: snapshot.generation,
                }) { self.read_error = Some(e); self.initial = None; hub.cancel_selection(); }
            }
        }
        // On a partial failure do not leave an unresolved initial target spinning forever.
        if !scanning && !self.reader.busy && self.read_revision == Some(key) && self.initial.is_some() {
            self.initial = None;
        }
        if !scanning && self.elapsed.is_none() { self.elapsed = self.started.map(|t| t.elapsed()); }
        self.busy = scanning || self.reader.busy || self.initial.is_some();
        Some(Update { results: self.results.clone(), selection, elapsed: self.elapsed, error: self.read_error.clone().or(error) })
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) { self.cancel(); self.shutdown(); }
}

/// Synchronous adapter only for existing headless callers; no whole result vector.
pub fn run(doc: Arc<Document>, query: Query) -> Result<SearchResults, String> {
    let mut input = Input::Legacy(doc);
    let store = input.run(query, 0, &|| false, &mut |_| Ok(()))?;
    let state = store.state();
    if state.status != Status::Completed { return Err(state.error); }
    Ok(SearchResults { store: Some(store), visible_hits: None })
}

pub fn initial(store: &ResultStore, origin: usize, reverse: bool) -> Result<Option<(u64, usize)>, String> {
    let count = store.state().count;
    if count == 0 { return Ok(None); }
    let index = bound(store, count, origin as u64, !reverse, &|| false)?;
    let index = if reverse { if index == 0 { count - 1 } else { index - 1 } }
        else if index == count { 0 } else { index };
    Ok(Some((index, store.node(index, &|| false)?.reference.address as usize)))
}
