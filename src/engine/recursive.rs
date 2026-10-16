//! Progressive file session with resident tree metadata and search results.
//! Source JSON is read on demand; exports stream to their destination file.
use super::{
    budget::{BudgetManager, Limits, Reservation, Resource},
    sparse::store::{Record, Store},
    top_level::{Kind, Source},
};
use crate::document::indexed::{Reader, Snapshot};
use std::{
    io::Write,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
const NONE: u64 = u64::MAX;
const LIMIT: usize = 65_536;
#[derive(Clone)]
pub struct Hits {
    generation: u64,
    session: super::session::SessionId,
    data: Arc<Mutex<HitData>>,
    count: u64,
}
struct HitData {
    store: Store,
    directory: u64,
    next: u64,
    count: u64,
}
impl Hits {
    pub fn len(&self) -> u64 {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    fn target(&self, n: u64) -> Result<(u64, u64), String> {
        let d = self.data.lock().unwrap();
        if n >= d.count {
            return Err("Match out of range".into());
        }
        let at = d.store.directory_weight(d.directory, n, &|| false)?.0[2];
        let record = d.store.get(at, &|| false)?;
        Ok((record[1], record[2]))
    }
    #[cfg(test)]
    fn get(&self, n: u64) -> Result<u64, String> {
        self.target(n).map(|v| v.0)
    }
}
struct HitWriter {
    hits: Hits,
    replacements: u64,
    published: u64,
    last: Instant,
    started: Instant,
    publish: bool,
}
impl HitWriter {
    fn new(s: &Shared, cancel: &dyn Fn() -> bool, publish: bool) -> Result<Self, String> {
        let store = Store::memory(&s.budget, cancel)?;
        Ok(Self {
            hits: Hits {
                generation: s.search_epoch.load(Ordering::Relaxed),
                session: s.session,
                data: Arc::new(Mutex::new(HitData {
                    store,
                    directory: NONE,
                    next: 0,
                    count: 0,
                })),
                count: 0,
            },
            replacements: 0,
            published: 0,
            last: Instant::now(),
            started: Instant::now(),
            publish,
        })
    }
    fn notify(&mut self, s: &Shared, complete: bool, force: bool) {
        if !self.publish {
            return;
        }
        self.hits.count = self.hits.data.lock().unwrap().count;
        if !force
            && !(self.hits.count == 1 && self.published == 0)
            && self.hits.count.saturating_sub(self.published) < 256
            && self.last.elapsed() < Duration::from_millis(40)
        {
            return;
        }
        if self.hits.generation != s.search_epoch.load(Ordering::Acquire) {
            return;
        }
        let mut c = s.control.lock().unwrap();
        if self.hits.generation != s.search_epoch.load(Ordering::Acquire) {
            return;
        }
        c.search_update = Some(SearchUpdate {
            generation: self.hits.generation,
            result: Ok(self.hits.clone()),
            complete,
            elapsed: self.started.elapsed(),
        });
        self.published = self.hits.count;
        self.last = Instant::now();
        drop(c);
        s.ctx.request_repaint();
    }
    fn prune(&mut self, s: &Shared, cancel: &dyn Fn() -> bool) -> Result<(), String> {
        let revision = s.index.lock().unwrap().replacements;
        if revision == self.replacements {
            return Ok(());
        }
        let next = self.hits.data.lock().unwrap().next;
        for n in 0..next {
            if cancel() {
                return Err("Search cancelled".into());
            }
            let (edge, r) = {
                let d = self.hits.data.lock().unwrap();
                let edge = d
                    .store
                    .directory_find(d.directory, n, false, cancel)?
                    .ok_or("Missing hit")?;
                if edge[7] == 0 {
                    continue;
                }
                (edge, d.store.get(edge[2], cancel)?)
            };
            let live = {
                let i = s.index.lock().unwrap();
                i.node(r[1])?[2] == r[2] && i.live_ancestor(r[1])? == r[1]
            };
            if !live {
                let mut d = self.hits.data.lock().unwrap();
                let root = d.directory;
                d.directory = d.store.directory_put(root, [n, edge[2], 0, 0, 1], cancel)?;
                d.count -= 1;
            }
        }
        self.replacements = revision;
        self.notify(s, false, true);
        Ok(())
    }
    fn push(
        &mut self,
        s: &Shared,
        id: u64,
        start: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        self.prune(s, cancel)?;
        let i = s.index.lock().unwrap();
        if i.node(id)?[2] != start || i.live_ancestor(id)? != id {
            return Ok(());
        }
        drop(i);
        let mut d = self.hits.data.lock().unwrap();
        let record = d
            .store
            .append([8, id, start, 1, 0, 0, 0, 0, 0, 0, 0, 0], cancel)?;
        let root = d.directory;
        let next = d.next;
        d.directory = d
            .store
            .directory_put(root, [next, record, 1, 0, 1], cancel)?;
        d.next += 1;
        d.count += 1;
        drop(d);
        self.notify(s, false, false);
        Ok(())
    }
    fn finish(mut self) -> Hits {
        self.hits.count = self.hits.data.lock().unwrap().count;
        self.hits
    }
}
pub struct SearchUpdate {
    generation: u64,
    pub result: Result<Hits, String>,
    pub complete: bool,
    pub elapsed: Duration,
}
// Node tags: 6 = discovered/range known, 9 = recursively indexed.
// Node words: tag, kind, start, end, parent, key start/end, ordinal,
// children directory, discovered count, expanded, visible subtree weight.
struct Index {
    store: Store,
    addresses: u64,
    root: u64,
    expand_all: bool,
    replaced: bool,
    replacements: u64,
}
impl Index {
    fn node(&self, id: u64) -> Result<Record, String> {
        let r = self.store.get(id, &|| false)?;
        if !matches!(r[0], 6 | 9) {
            return Err("Invalid progressive node".into());
        }
        Ok(r)
    }
    fn child(&self, p: Record, n: u64) -> Result<Option<u64>, String> {
        Ok(self
            .store
            .directory_find(p[8], n, false, &|| false)?
            .filter(|r| r[1] == n)
            .map(|r| r[2]))
    }
    fn update(&mut self, mut id: u64) -> Result<(), String> {
        for _ in 0..129 {
            let mut r = self.node(id)?;
            r[11] = 1 + if r[10] != 0 {
                self.store.directory_sums(r[8], &|| false)?.0
            } else {
                0
            };
            self.store.put(id, r, &|| false)?;
            if r[4] == NONE {
                return Ok(());
            }
            let mut p = self.node(r[4])?;
            p[8] = self
                .store
                .directory_put(p[8], [r[7], id, r[11], 0, 1], &|| false)?;
            self.store.put(r[4], p, &|| false)?;
            id = r[4];
        }
        Err("Parent depth limit".into())
    }
    fn at(&self, mut rank: u64) -> Result<(u64, usize), String> {
        let mut id = self.root;
        for depth in 0..129 {
            let r = self.node(id)?;
            if rank == 0 {
                return Ok((id, depth));
            }
            if r[10] == 0 {
                return Err("Row unavailable".into());
            }
            let (edge, offset) = self.store.directory_weight(r[8], rank - 1, &|| false)?;
            id = edge[2];
            rank = offset;
        }
        Err("Tree depth limit".into())
    }
    fn live_ancestor(&self, mut id: u64) -> Result<u64, String> {
        for _ in 0..129 {
            let mut n = id;
            let mut orphan = None;
            for _ in 0..129 {
                let r = self.node(n)?;
                if r[4] == NONE {
                    break;
                }
                let parent = self.node(r[4])?;
                if self.child(parent, r[7])? != Some(n) {
                    orphan = Some(r[4]);
                    break;
                }
                n = r[4];
            }
            if let Some(parent) = orphan {
                id = parent;
            } else {
                return Ok(id);
            }
        }
        Err("Parent cycle".into())
    }
    fn rank(&self, mut id: u64) -> Result<u64, String> {
        let mut rank = 0;
        for _ in 0..129 {
            let r = self.node(id)?;
            if r[4] == NONE {
                return Ok(rank);
            }
            let p = self.node(r[4])?;
            rank += 1 + self.store.directory_prefix(p[8], r[7], &|| false)?.0;
            id = r[4];
        }
        Err("Parent cycle".into())
    }
    fn reveal(&mut self, mut id: u64) -> Result<(), String> {
        for _ in 0..129 {
            let r = self.node(id)?;
            if r[4] == NONE {
                return Ok(());
            }
            id = r[4];
            let mut p = self.node(id)?;
            p[10] = 1;
            self.store.put(id, p, &|| false)?;
            self.update(id)?;
        }
        Err("Parent cycle".into())
    }
}
#[derive(Clone)]
pub struct Row {
    pub id: u64,
    pub index: u64,
    pub depth: usize,
    pub label: String,
    pub path: String,
    pub kind: Kind,
    pub preview: String,
    pub expanded: bool,
    pub complete: bool,
}
pub struct Page {
    pub start: u64,
    pub selected: u64,
    pub rows: Vec<Row>,
    pub selection: Row,
    pub detail: String,
    pub count: u64,
    pub ready: bool,
    _lease: Reservation,
}
#[derive(Clone, serde::Serialize)]
pub struct Statistics {
    pub nodes: u64,
    pub rows: u64,
    pub scanned_bytes: u64,
    pub source_bytes: u64,
    pub ready: bool,
    pub first_field_us: Option<u64>,
}
#[derive(Clone)]
pub enum Action {
    Hit(Hits, u64),
    Toggle(u64),
    All(bool),
    Select(u64),
    Move(i64),
    Rank(u64),
    Parent,
    Child,
    Back,
    Forward,
}
#[derive(Clone)]
pub enum Operation {
    Path(u64),
    Search(super::search::Query),
    Query(String),
    Copy {
        ids: Vec<u64>,
        array: bool,
    },
    Export {
        ids: Vec<u64>,
        array: bool,
        path: std::path::PathBuf,
    },
}
pub enum Output {
    Search(Hits),
    Query { ids: Vec<u64>, preview: String },
    Copy(String),
    Export(std::path::PathBuf),
}
pub struct Completion {
    pub export: bool,
    pub result: Result<Output, String>,
    pub elapsed: Duration,
}
struct Control {
    search_operation: Option<(u64, super::search::Query)>,
    search_update: Option<SearchUpdate>,
    export_operation: Option<(u64, Operation)>,
    export_completion: Option<(u64, Completion)>,
    start: u64,
    selected: u64,
    page: Option<Arc<Page>>,
    revision: u64,
    delivered: u64,
    actions: std::collections::VecDeque<Action>,
    history: std::collections::VecDeque<u64>,
    history_at: usize,
    priority: Option<(u64, u64, u64)>,
    operation: Option<(u64, Operation)>,
    completion: Option<(u64, Completion)>,
    error: Option<String>,
}
struct Shared {
    session: super::session::SessionId,
    search_epoch: AtomicU64,
    export_epoch: AtomicU64,
    source: Arc<Source>,
    snapshot: Snapshot,
    index: Mutex<Index>,
    control: Mutex<Control>,
    changed: Condvar,
    budget: BudgetManager,
    stop: AtomicBool,
    cancel: Arc<AtomicBool>,
    ready: AtomicBool,
    nodes: AtomicU64,
    rows: AtomicU64,
    scanned: AtomicU64,
    first: AtomicU64,
    started: Instant,
    epoch: AtomicU64,
    priority_epoch: AtomicU64,
    ctx: eframe::egui::Context,
    _state: Reservation,
}
impl Shared {
    fn cancelled(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed)
    }
    fn changed(&self) {
        let mut c = self.control.lock().unwrap();
        c.revision = c.revision.wrapping_add(1);
        drop(c);
        self.changed.notify_all();
    }
    fn check(&self) -> Result<(), String> {
        if self.cancelled() {
            Err("Opening cancelled".into())
        } else {
            Ok(())
        }
    }
    fn fail(&self, e: String) {
        if !self.cancelled() {
            self.control.lock().unwrap().error = Some(e);
            self.changed();
            self.ctx.request_repaint();
        }
    }
    fn register(
        &self,
        kind: Kind,
        start: u64,
        parent: (u64, u64),
        key: (u64, u64, u64),
        ordinal: u64,
    ) -> Result<u64, String> {
        self.check()?;
        let (parent, parent_start) = parent;
        let mut i = self.index.lock().unwrap();
        if parent != NONE
            && (i.node(parent)?[2] != parent_start
                || (i.replaced && i.live_ancestor(parent)? != parent))
        {
            return Ok(NONE);
        }
        if let Some(a) = i
            .store
            .directory_find(i.addresses, start, false, &|| self.cancelled())?
            .filter(|r| r[1] == start)
        {
            return Ok(a[2]);
        }
        let mut canonical = None;
        let hash = key.2 ^ parent.wrapping_mul(0x9e3779b97f4a7c15);
        if key.0 != NONE {
            let mut link = i.store.head(hash, &|| self.cancelled())?;
            while link != NONE {
                self.check()?;
                let h = i.store.get(link, &|| self.cancelled())?;
                if h[1] == hash
                    && h[2] == parent
                    && equal_keys(&self.snapshot, key.0, h[3], &|| self.cancelled())?
                {
                    let old = i.node(h[5])?;
                    if i.child(i.node(parent)?, old[7])? == Some(h[5]) {
                        canonical = Some(h[5]);
                        break;
                    }
                }
                link = h[6];
            }
        }
        let id = if let Some(id) = canonical {
            let old = i.node(id)?;
            if start > old[2] {
                i.replaced = true;
                i.replacements = i.replacements.wrapping_add(1);
                let r = [
                    6,
                    kind as u64,
                    start,
                    NONE,
                    parent,
                    key.0,
                    key.1,
                    old[7],
                    NONE,
                    0,
                    old[10],
                    1,
                ];
                i.store.put(id, r, &|| self.cancelled())?;
                i.update(id)?;
            }
            id
        } else {
            let ordinal = if parent != NONE {
                i.node(parent)?[9]
            } else {
                ordinal
            };
            let r = [
                6,
                kind as u64,
                start,
                NONE,
                parent,
                key.0,
                key.1,
                ordinal,
                NONE,
                0,
                u64::from(parent == NONE || i.expand_all),
                1,
            ];
            let id = i.store.append(r, &|| self.cancelled())?;
            if parent == NONE {
                i.root = id
            } else {
                let mut p = i.node(parent)?;
                p[8] = i
                    .store
                    .directory_put(p[8], [ordinal, id, 1, 0, 1], &|| self.cancelled())?;
                p[9] += 1;
                i.store.put(parent, p, &|| self.cancelled())?;
                i.update(parent)?;
            }
            if key.0 != NONE {
                let head = i.store.head(hash, &|| self.cancelled())?;
                let h = i.store.append(
                    [7, hash, parent, key.0, key.1, id, head, 0, 0, 0, 0, 0],
                    &|| self.cancelled(),
                )?;
                i.store.set_head(hash, h, &|| self.cancelled())?;
            }
            id
        };
        let addresses = i.addresses;
        i.addresses = i
            .store
            .directory_put(addresses, [start, id, 1, 0, 1], &|| self.cancelled())?;
        self.nodes.fetch_add(1, Ordering::Relaxed);
        self.rows.store(i.node(i.root)?[11], Ordering::Release);
        drop(i);
        if parent != NONE {
            let _ = self.first.compare_exchange(
                NONE,
                self.started.elapsed().as_micros() as u64,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        self.changed();
        Ok(id)
    }
    fn finish(&self, id: u64, start: u64, end: u64, indexed: bool) -> Result<(), String> {
        let mut i = self.index.lock().unwrap();
        let mut r = i.node(id)?;
        if r[2] == start {
            r[3] = end;
            if indexed {
                r[0] = 9;
            }
            i.store.put(id, r, &|| self.cancelled())?;
        }
        drop(i);
        self.changed();
        Ok(())
    }
}
pub struct Session {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}
impl Session {
    pub fn open(
        path: &std::path::Path,
        cancel: Arc<AtomicBool>,
        ctx: eframe::egui::Context,
    ) -> Result<Self, String> {
        Self::start(Source::open(path)?, cancel, ctx)
    }
    pub fn start(
        source: Arc<Source>,
        cancel: Arc<AtomicBool>,
        ctx: eframe::egui::Context,
    ) -> Result<Self, String> {
        let budget = BudgetManager::new(Limits {
            // Transitional resident-tree ceiling; policy and user-facing limits follow in M4.
            // Blocks are charged on allocation, not preallocated up to this limit.
            memory: 512 * 1024 * 1024,
            disk: 64usize * 1024 * 1024 * 1024,
        });
        let snapshot = Snapshot::protected(source.duplicate_file()?, &budget)?;
        let store = Store::memory(&budget, &|| cancel.load(Ordering::Relaxed))?;
        let s = Arc::new(Shared {
            session: super::session::SessionId::fresh(),
            search_epoch: AtomicU64::new(0),
            source,
            snapshot,
            index: Mutex::new(Index {
                store,
                addresses: NONE,
                root: NONE,
                expand_all: false,
                replaced: false,
                replacements: 0,
            }),
            control: Mutex::new(Control {
                search_operation: None,
                search_update: None,
                export_operation: None,
                export_completion: None,
                start: 0,
                selected: NONE,
                page: None,
                revision: 1,
                delivered: 0,
                actions: Default::default(),
                history: Default::default(),
                history_at: 0,
                priority: None,
                operation: None,
                completion: None,
                error: None,
            }),
            changed: Condvar::new(),
            budget: budget.clone(),
            stop: AtomicBool::new(false),
            cancel,
            ready: AtomicBool::new(false),
            nodes: AtomicU64::new(0),
            rows: AtomicU64::new(0),
            scanned: AtomicU64::new(0),
            first: AtomicU64::new(NONE),
            started: Instant::now(),
            epoch: AtomicU64::new(0),
            export_epoch: AtomicU64::new(0),
            priority_epoch: AtomicU64::new(0),
            ctx,
            _state: budget.reserve(
                Resource::Memory,
                24 * 1024 * 1024,
                "progressive session and bounded queues",
            )?,
        });
        let scan = s.clone();
        let scanner = std::thread::spawn(move || match scan_all(&scan) {
            Ok(()) => {
                scan.ready.store(true, Ordering::Release);
                scan.changed();
                scan.ctx.request_repaint();
            }
            Err(e) => scan.fail(e),
        });
        let read = s.clone();
        let pager = std::thread::spawn(move || pager(&read));
        let fast = s.clone();
        let priority = std::thread::spawn(move || priority_worker(&fast));
        let op = s.clone();
        let operations = std::thread::spawn(move || operation_worker(&op, false));
        let search = s.clone();
        let searcher = std::thread::spawn(move || search_worker(&search));
        let output = s.clone();
        let exporter = std::thread::spawn(move || operation_worker(&output, true));
        Ok(Self {
            shared: s,
            threads: vec![scanner, pager, priority, operations, exporter, searcher],
        })
    }
    pub fn budget(&self) -> BudgetManager {
        self.shared.budget.clone()
    }
    pub fn statistics(&self) -> Statistics {
        let n = self.shared.first.load(Ordering::Relaxed);
        Statistics {
            nodes: self.shared.nodes.load(Ordering::Relaxed),
            rows: self.shared.rows.load(Ordering::Acquire),
            scanned_bytes: self.shared.scanned.load(Ordering::Relaxed),
            source_bytes: self.shared.source.bytes,
            ready: self.shared.ready.load(Ordering::Acquire),
            first_field_us: (n != NONE).then_some(n),
        }
    }
    pub fn poll(&self) -> Option<Arc<Page>> {
        self.shared.control.lock().unwrap().page.take()
    }
    pub fn error(&self) -> Option<String> {
        self.shared.control.lock().unwrap().error.take()
    }
    pub fn request(&self, start: u64) {
        let mut c = self.shared.control.lock().unwrap();
        if c.start != start {
            c.start = start;
            c.revision = c.revision.wrapping_add(1);
            c.page = None;
            drop(c);
            self.shared.changed.notify_all();
        }
    }
    pub fn action(&self, a: Action) {
        let mut c = self.shared.control.lock().unwrap();
        if c.actions.len() < 64 {
            c.actions.push_back(a);
            c.revision = c.revision.wrapping_add(1);
            c.page = None;
        }
        drop(c);
        self.shared.changed.notify_all();
    }
    pub fn submit(&self, op: Operation) {
        if let Operation::Search(q) = op {
            self.clear_search();
            let epoch = self.shared.search_epoch.load(Ordering::Acquire);
            self.shared.control.lock().unwrap().search_operation = Some((epoch, q));
            self.shared.changed.notify_all();
            return;
        }
        if matches!(op, Operation::Export { .. }) {
            let n = self.shared.export_epoch.fetch_add(1, Ordering::Relaxed) + 1;
            let mut c = self.shared.control.lock().unwrap();
            c.export_operation = Some((n, op));
            c.export_completion = None;
            drop(c);
            self.shared.changed.notify_all();
            return;
        }
        let n = self.shared.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        let mut c = self.shared.control.lock().unwrap();
        c.operation = Some((n, op));
        c.completion = None;
        drop(c);
        self.shared.changed.notify_all();
    }
    pub fn clear_search(&self) {
        self.shared.search_epoch.fetch_add(1, Ordering::AcqRel);
        let mut c = self.shared.control.lock().unwrap();
        c.search_operation = None;
        c.search_update = None;
    }
    pub fn search_update(&self) -> Option<SearchUpdate> {
        let mut c = self.shared.control.lock().unwrap();
        c.search_update
            .take()
            .filter(|u| u.generation == self.shared.search_epoch.load(Ordering::Acquire))
    }
    pub fn cancel_operation(&self) {
        self.shared.epoch.fetch_add(1, Ordering::Relaxed);
        let mut c = self.shared.control.lock().unwrap();
        c.operation = None;
        c.completion = None;
    }
    pub fn completion(&self) -> Option<Completion> {
        let mut c = self.shared.control.lock().unwrap();
        let regular = c
            .completion
            .take()
            .filter(|(n, _)| *n == self.shared.epoch.load(Ordering::Relaxed))
            .map(|(_, v)| v);
        regular.or_else(|| {
            c.export_completion
                .take()
                .filter(|(n, _)| *n == self.shared.export_epoch.load(Ordering::Relaxed))
                .map(|(_, v)| v)
        })
    }
    pub fn cancel_export(&self) {
        self.shared.export_epoch.fetch_add(1, Ordering::Relaxed);
        let mut c = self.shared.control.lock().unwrap();
        c.export_operation = None;
        c.export_completion = None;
    }
    pub fn retire(mut self) -> JoinHandle<()> {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.changed.notify_all();
        let ts = std::mem::take(&mut self.threads);
        std::thread::spawn(move || {
            for t in ts {
                let _ = t.join();
            }
        })
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.changed.notify_all();
    }
}
fn space(r: &mut Reader<'_>) -> Result<(), String> {
    while matches!(r.peek()?, Some(b' ' | b'\r' | b'\n' | b'\t')) {
        r.next()?;
    }
    Ok(())
}
fn string(r: &mut Reader<'_>) -> Result<u64, String> {
    r.expect(b'"')?;
    let mut h = 0xcbf29ce484222325u64;
    while let Some(c) = r.character()? {
        let mut b = [0; 4];
        for v in c.encode_utf8(&mut b).bytes() {
            h = (h ^ u64::from(v)).wrapping_mul(0x100000001b3);
        }
    }
    Ok(h)
}
fn equal_keys(
    source: &Snapshot,
    a: u64,
    b: u64,
    cancel: &dyn Fn() -> bool,
) -> Result<bool, String> {
    if a == b {
        return Ok(true);
    }
    let mut ar = Reader::new(source)?.with_cancellation(cancel);
    let mut br = Reader::new(source)?.with_cancellation(cancel);
    ar.pos = a;
    br.pos = b;
    ar.expect(b'"')?;
    br.expect(b'"')?;
    loop {
        let x = ar.character()?;
        let y = br.character()?;
        if x != y {
            return Ok(false);
        }
        if x.is_none() {
            return Ok(true);
        }
    }
}
fn digits(r: &mut Reader<'_>) -> Result<(), String> {
    while matches!(r.peek()?, Some(b'0'..=b'9')) {
        r.next()?;
    }
    Ok(())
}
fn scalar(r: &mut Reader<'_>, kind: Kind) -> Result<(), String> {
    match kind {
        Kind::String => {
            string(r)?;
        }
        Kind::Number => {
            if r.peek()? == Some(b'-') {
                r.next()?;
            }
            match r.next()? {
                b'0' => (),
                b'1'..=b'9' => digits(r)?,
                _ => return Err(r.error("invalid number")),
            }
            if r.peek()? == Some(b'.') {
                r.next()?;
                if !matches!(r.peek()?, Some(b'0'..=b'9')) {
                    return Err(r.error("missing fractional digits"));
                }
                digits(r)?;
            }
            if matches!(r.peek()?, Some(b'e' | b'E')) {
                r.next()?;
                if matches!(r.peek()?, Some(b'+' | b'-')) {
                    r.next()?;
                }
                if !matches!(r.peek()?, Some(b'0'..=b'9')) {
                    return Err(r.error("missing exponent digits"));
                }
                digits(r)?;
            }
        }
        Kind::Boolean | Kind::Null => {
            let word: &[u8] = match r.peek()? {
                Some(b't') => b"true",
                Some(b'f') => b"false",
                _ => b"null",
            };
            for b in word {
                r.expect(*b)?;
            }
        }
        _ => return Err("Expected scalar".into()),
    }
    Ok(())
}
fn kind(b: u8) -> Result<Kind, String> {
    match b {
        b'{' => Ok(Kind::Object),
        b'[' => Ok(Kind::Array),
        b'"' => Ok(Kind::String),
        b'-' | b'0'..=b'9' => Ok(Kind::Number),
        b't' | b'f' => Ok(Kind::Boolean),
        b'n' => Ok(Kind::Null),
        _ => Err("Invalid JSON: expected value".into()),
    }
}
struct Parser<'a> {
    s: &'a Shared,
    r: Reader<'a>,
    cancel: &'a dyn Fn() -> bool,
    local: Option<(u64, u64)>,
    search: Option<(&'a super::search::Matcher, &'a mut HitWriter)>,
}
impl Parser<'_> {
    fn value(
        &mut self,
        parent: (u64, u64),
        key: (u64, u64, u64),
        ordinal: u64,
        depth: usize,
        register: bool,
    ) -> Result<bool, String> {
        if depth > 128 {
            return Err(self.r.error("depth exceeds 128"));
        }
        if (self.cancel)() {
            return Err("Scan cancelled".into());
        }
        space(&mut self.r)?;
        let start = self.r.pos;
        let k = kind(self.r.peek()?.ok_or("Unexpected end of JSON")?)?;
        let id = if register {
            self.s.register(k, start, parent, key, ordinal)?
        } else {
            NONE
        };
        let register = register && id != NONE;
        let mut matched = false;
        let mut consumed_string = false;
        if let Some((matcher, output)) = self.search.as_mut() {
            output.prune(self.s, self.cancel)?;
            if register {
                let mut reader = Reader::new(&self.s.snapshot)?.with_cancellation(self.cancel);
                matched = matcher.keys()
                    && key.0 != NONE
                    && matcher.range(&mut reader, key.0, key.1, true, self.cancel)?;
                if matched {
                    output.push(self.s, id, start, self.cancel)?;
                }
                // Literals can be reported from a bounded decoded prefix of a huge string.
                if !matched && matcher.values() && k == Kind::String && !matcher.is_regex() {
                    matched = matcher.stream_string(
                        &mut reader,
                        start,
                        self.s.source.bytes,
                        self.cancel,
                        &mut || output.push(self.s, id, start, self.cancel),
                    )?;
                    self.r.pos = reader.pos;
                    consumed_string = true;
                }
            }
        }
        if k.container() {
            self.r.next()?;
            space(&mut self.r)?;
            let close = if k == Kind::Object { b'}' } else { b']' };
            let mut n = 0;
            if self.r.peek()? != Some(close) {
                loop {
                    space(&mut self.r)?;
                    let key = if k == Kind::Object {
                        let ks = self.r.pos;
                        let h = string(&mut self.r)?;
                        let ke = self.r.pos;
                        space(&mut self.r)?;
                        self.r.expect(b':')?;
                        (ks, ke, h)
                    } else {
                        (NONE, NONE, 0)
                    };
                    let current = !register || self.s.index.lock().unwrap().node(id)?[2] == start;
                    let child_register =
                        register && current && self.local.is_none_or(|(p, _)| start == p);
                    if !self.value((id, start), key, n, depth + 1, child_register)? {
                        return Ok(false);
                    }
                    n += 1;
                    if self
                        .local
                        .is_some_and(|(p, limit)| start == p && n >= limit)
                    {
                        return Ok(false);
                    }
                    space(&mut self.r)?;
                    match self.r.next()? {
                        b if b == close => break,
                        b',' => (),
                        _ => return Err(self.r.error("expected comma or closing delimiter")),
                    }
                }
            } else {
                self.r.next()?;
            }
        } else if !consumed_string {
            scalar(&mut self.r, k)?;
        }
        if register {
            self.s.finish(id, start, self.r.pos, self.local.is_none())?;
            if let Some((matcher, output)) = self.search.as_mut() {
                if !matched
                    && matcher.values()
                    && !k.container()
                    && (k != Kind::String || matcher.is_regex())
                {
                    let mut reader = Reader::new(&self.s.snapshot)?.with_cancellation(self.cancel);
                    if matcher.range(
                        &mut reader,
                        start,
                        self.r.pos,
                        k == Kind::String,
                        self.cancel,
                    )? {
                        output.push(self.s, id, start, self.cancel)?;
                    }
                }
                output.notify(self.s, false, false);
            }
        }
        if self.local.is_none() && self.search.is_none() {
            self.s.scanned.fetch_max(self.r.pos, Ordering::Relaxed);
        }
        Ok(true)
    }
}
fn scan_all(s: &Shared) -> Result<(), String> {
    let cancel = || s.cancelled();
    let mut r = Reader::new(&s.snapshot)?.with_cancellation(&cancel);
    if r.peek()? == Some(0xef) {
        for b in [0xef, 0xbb, 0xbf] {
            r.expect(b)?;
        }
    }
    let mut p = Parser {
        s,
        r,
        cancel: &cancel,
        local: None,
        search: None,
    };
    p.value((NONE, NONE), (NONE, NONE, 0), 0, 0, true)?;
    space(&mut p.r)?;
    if p.r.peek()?.is_some() {
        return Err(p.r.error("trailing characters"));
    }
    s.check()?;
    s.scanned.store(s.source.bytes, Ordering::Relaxed);
    Ok(())
}
fn priority_worker(s: &Shared) {
    loop {
        let task = {
            let mut c = s.control.lock().unwrap();
            while c.priority.is_none() && !s.cancelled() {
                c = s
                    .changed
                    .wait_timeout(c, Duration::from_millis(25))
                    .unwrap()
                    .0;
            }
            if s.cancelled() {
                return;
            }
            c.priority.take().unwrap()
        };
        let epoch = task.0;
        let cancel = || s.cancelled() || s.priority_epoch.load(Ordering::Relaxed) != epoch;
        let run = || -> Result<(), String> {
            let r = s.index.lock().unwrap().node(task.1)?;
            if r[0] == 9 || !kind_from(r[1])?.container() {
                return Ok(());
            }
            let mut reader = Reader::new(&s.snapshot)?.with_cancellation(&cancel);
            reader.pos = r[2];
            let hash = if r[5] != NONE {
                let mut k = Reader::new(&s.snapshot)?.with_cancellation(&cancel);
                k.pos = r[5];
                string(&mut k)?
            } else {
                0
            };
            let mut p = Parser {
                s,
                r: reader,
                cancel: &cancel,
                local: Some((r[2], task.2)),
                search: None,
            };
            let parent_start = if r[4] == NONE {
                NONE
            } else {
                s.index.lock().unwrap().node(r[4])?[2]
            };
            p.value((r[4], parent_start), (r[5], r[6], hash), r[7], 0, true)?;
            Ok(())
        };
        let _ = run();
        s.changed();
    }
}
fn kind_from(v: u64) -> Result<Kind, String> {
    match v {
        0 => Ok(Kind::Object),
        1 => Ok(Kind::Array),
        2 => Ok(Kind::String),
        3 => Ok(Kind::Number),
        4 => Ok(Kind::Boolean),
        5 => Ok(Kind::Null),
        _ => Err("Invalid progressive kind".into()),
    }
}
fn key_text(
    s: &Shared,
    r: Record,
    limit: usize,
    cancel: &dyn Fn() -> bool,
) -> Result<(String, bool), String> {
    if r[4] == NONE {
        return Ok(("$".into(), true));
    }
    if r[5] == NONE {
        return Ok((r[7].to_string(), true));
    }
    let mut reader = Reader::new(&s.snapshot)?.with_cancellation(cancel);
    reader.pos = r[5];
    reader.expect(b'"')?;
    let mut text = String::new();
    while let Some(c) = reader.character()? {
        if text.len() + c.len_utf8() > limit {
            return Ok((text, false));
        }
        text.push(c);
    }
    Ok((text, true))
}
fn path(s: &Shared, id: u64, limit: usize, cancel: &dyn Fn() -> bool) -> Result<String, String> {
    let mut segments = Vec::new();
    let mut n = id;
    let mut bytes = 0;
    for _ in 0..129 {
        let r = s.index.lock().unwrap().node(n)?;
        if r[4] == NONE {
            segments.reverse();
            return Ok(format!("${}", segments.concat()));
        }
        let (key, complete) = key_text(s, r, limit.saturating_sub(bytes), cancel)?;
        if !complete {
            return Err("Path exceeds output limit".into());
        }
        let part = if r[5] == NONE {
            format!("[{key}]")
        } else if crate::document::is_identifier(&key) {
            format!(".{key}")
        } else {
            format!(
                "[{}]",
                serde_json::to_string(&key).map_err(|e| e.to_string())?
            )
        };
        bytes += part.len();
        if bytes > limit {
            return Err("Path exceeds output limit".into());
        }
        segments.push(part);
        n = r[4];
    }
    Err("Parent cycle".into())
}
fn preview(
    s: &Shared,
    r: Record,
    limit: usize,
    cancel: &dyn Fn() -> bool,
) -> Result<String, String> {
    if r[3] == NONE && kind_from(r[1])?.container() {
        return Ok(if r[1] == 0 { "{…}" } else { "[…]" }.into());
    }
    let end = if r[3] == NONE { s.source.bytes } else { r[3] };
    if end < r[2] || end > s.source.bytes {
        return Err("Invalid source range".into());
    }
    let n = (end - r[2]).min(limit as u64) as usize;
    let mut raw = vec![0; n];
    s.snapshot
        .read_cancellable(r[2], &mut raw, cancel)
        .map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&raw).into_owned();
    if r[3] == NONE || end - r[2] > limit as u64 {
        text.push('…')
    }
    Ok(text)
}
fn row(s: &Shared, id: u64, rank: u64, depth: usize) -> Result<Row, String> {
    let r = s.index.lock().unwrap().node(id)?;
    let cancel = || s.cancelled();
    let (label, complete) = key_text(s, r, 640, &cancel)?;
    Ok(Row {
        id,
        index: rank,
        depth,
        label: format!(
            "{}{}",
            label.chars().take(160).collect::<String>(),
            if complete { "" } else { "…" }
        ),
        path: path(s, id, 16 * 1024, &cancel)
            .unwrap_or_else(|_| "Path exceeds display limit".into()),
        kind: kind_from(r[1])?,
        preview: preview(s, r, 128, &cancel)?,
        expanded: r[10] != 0,
        complete: r[3] != NONE,
    })
}
fn apply(s: &Shared, a: Action) -> Result<(), String> {
    let (selected, start) = {
        let c = s.control.lock().unwrap();
        (c.selected, c.start)
    };
    let hit = if let Action::Hit(h, n) = &a {
        h.target(*n).ok()
    } else {
        None
    };
    let mut i = s.index.lock().unwrap();
    if i.root == NONE {
        return Ok(());
    }
    let selected = i.live_ancestor(if selected == NONE { i.root } else { selected })?;
    let mut next = selected;
    let mut scroll = false;
    let history = matches!(a, Action::Back | Action::Forward);
    match a {
        Action::Hit(hits, n) => {
            if hits.session != s.session
                || hits.generation != s.search_epoch.load(Ordering::Relaxed)
            {
                return Ok(());
            }
            let _ = n;
            let Some((id, source_start)) = hit else {
                return Ok(());
            };
            if i.node(id)?[2] != source_start || i.live_ancestor(id)? != id {
                return Ok(());
            }
            next = id;
            i.reveal(next)?;
            scroll = true;
        }
        Action::Toggle(id) => {
            if i.live_ancestor(id)? != id {
                return Ok(());
            }
            let mut r = i.node(id)?;
            if kind_from(r[1])?.container() {
                r[10] ^= 1;
                i.store.put(id, r, &|| s.cancelled())?;
                i.update(id)?;
                if r[10] != 0 && r[0] != 9 {
                    let epoch = s.priority_epoch.fetch_add(1, Ordering::Relaxed) + 1;
                    s.control.lock().unwrap().priority = Some((epoch, id, 64));
                }
                if r[10] == 0 {
                    let mut n = selected;
                    for _ in 0..129 {
                        let r = i.node(n)?;
                        if n == id {
                            next = id;
                            break;
                        }
                        if r[4] == NONE {
                            break;
                        }
                        n = r[4];
                    }
                }
            }
        }
        Action::All(expanded) => {
            i.expand_all = expanded;
            fn walk(
                i: &mut Index,
                id: u64,
                on: bool,
                s: &Shared,
                depth: usize,
            ) -> Result<(), String> {
                s.check()?;
                if depth > 128 {
                    return Err("Depth limit".into());
                }
                let mut r = i.node(id)?;
                for n in 0..r[9] {
                    let child = i.child(r, n)?.ok_or("Missing child")?;
                    walk(i, child, on, s, depth + 1)?;
                    let cr = i.node(child)?;
                    r[8] = i
                        .store
                        .directory_put(r[8], [n, child, cr[11], 0, 1], &|| s.cancelled())?;
                }
                r[10] = u64::from(on);
                r[11] = 1 + if on {
                    i.store.directory_sums(r[8], &|| s.cancelled())?.0
                } else {
                    0
                };
                i.store.put(id, r, &|| s.cancelled())
            }
            let root = i.root;
            walk(&mut i, root, expanded, s, 0)?;
            next = root;
            scroll = true;
        }
        Action::Select(id) => {
            let id = i.live_ancestor(id)?;
            i.reveal(id)?;
            next = id;
            scroll = true;
        }
        Action::Move(delta) => {
            let rank = i
                .rank(selected)?
                .saturating_add_signed(delta)
                .min(i.node(i.root)?[11] - 1);
            next = i.at(rank)?.0;
            scroll = true;
        }
        Action::Rank(rank) => {
            next = i.at(rank.min(i.node(i.root)?[11] - 1))?.0;
            scroll = true;
        }
        Action::Parent => {
            let mut r = i.node(selected)?;
            if r[10] != 0 {
                r[10] = 0;
                i.store.put(selected, r, &|| false)?;
                i.update(selected)?;
            } else if r[4] != NONE {
                next = r[4];
            }
            scroll = true;
        }
        Action::Child => {
            let mut r = i.node(selected)?;
            if kind_from(r[1])?.container() {
                if r[10] == 0 {
                    r[10] = 1;
                    i.store.put(selected, r, &|| false)?;
                    i.update(selected)?;
                    if r[0] != 9 {
                        let epoch = s.priority_epoch.fetch_add(1, Ordering::Relaxed) + 1;
                        s.control.lock().unwrap().priority = Some((epoch, selected, 64));
                    }
                } else if let Some(id) = i.child(r, 0)? {
                    next = id;
                }
            }
            scroll = true;
        }
        Action::Back | Action::Forward => {
            let mut c = s.control.lock().unwrap();
            if matches!(a, Action::Back) {
                c.history_at = c.history_at.saturating_sub(1);
            } else {
                c.history_at = (c.history_at + 1).min(c.history.len().saturating_sub(1));
            }
            if let Some(id) = c.history.get(c.history_at) {
                next = *id;
            }
            drop(c);
            next = i.live_ancestor(next)?;
            i.reveal(next)?;
            scroll = true;
        }
    }
    s.rows.store(i.node(i.root)?[11], Ordering::Release);
    let rank = i.rank(next)?;
    drop(i);
    let mut c = s.control.lock().unwrap();
    c.selected = next;
    if next != selected && !history {
        let at = c.history_at;
        c.history.truncate(at + 1);
        c.history.push_back(next);
        if c.history.len() > 64 {
            c.history.pop_front();
        }
        c.history_at = c.history.len() - 1;
    }
    if scroll {
        c.start = rank.saturating_sub(8);
    } else {
        c.start = start;
    }
    Ok(())
}
fn pager(s: &Shared) {
    loop {
        let (start, selected, actions) = {
            let mut c = s.control.lock().unwrap();
            while c.revision == c.delivered && !s.cancelled() {
                c = s
                    .changed
                    .wait_timeout(c, Duration::from_millis(25))
                    .unwrap()
                    .0;
            }
            if s.cancelled() {
                return;
            }
            let actions = c.actions.drain(..).collect::<Vec<_>>();
            c.delivered = c.revision;
            (c.start, c.selected, actions)
        };
        let interacted = !actions.is_empty();
        for a in actions {
            if let Err(e) = apply(s, a) {
                s.fail(e)
            }
        }
        let (start, selected) = if interacted {
            let c = s.control.lock().unwrap();
            (c.start, c.selected)
        } else {
            (start, selected)
        };
        let build = || -> Result<Page, String> {
            let lease = s.budget.reserve(
                Resource::Memory,
                2 * 1024 * 1024,
                "progressive viewport and path workspace",
            )?;
            let (count, selected, rank, depth) = {
                let i = s.index.lock().unwrap();
                if i.root == NONE {
                    return Err("Root not discovered".into());
                }
                let mut selected = if selected == NONE { i.root } else { selected };
                // A duplicate object field can replace an entire old subtree.
                // Keep the canonical field selected, never an orphaned child.
                for _ in 0..129 {
                    let mut n = selected;
                    let mut orphan = None;
                    for _ in 0..129 {
                        let r = i.node(n)?;
                        if r[4] == NONE {
                            break;
                        }
                        let parent = i.node(r[4])?;
                        if i.child(parent, r[7])? != Some(n) {
                            orphan = Some(r[4]);
                            break;
                        }
                        n = r[4];
                    }
                    if let Some(parent) = orphan {
                        selected = parent;
                    } else {
                        break;
                    }
                }
                let rank = i.rank(selected)?;
                let (_, depth) = i.at(rank)?;
                (i.node(i.root)?[11], selected, rank, depth)
            };
            let start = start.min(count.saturating_sub(1));
            let mut rows = Vec::with_capacity(64);
            for n in start..(start + 64).min(count) {
                s.check()?;
                let (id, depth) = s.index.lock().unwrap().at(n)?;
                rows.push(row(s, id, n, depth)?);
            }
            let selection = row(s, selected, rank, depth)?;
            let r = s.index.lock().unwrap().node(selected)?;
            let detail = preview(s, r, 16 * 1024, &|| s.cancelled())?;
            Ok(Page {
                start,
                selected,
                rows,
                selection,
                detail,
                count,
                ready: s.ready.load(Ordering::Acquire),
                _lease: lease,
            })
        };
        match build() {
            Ok(p) => {
                let mut c = s.control.lock().unwrap();
                if c.actions.is_empty()
                    && c.start == start
                    && (c.selected == selected || selected == NONE)
                {
                    c.selected = p.selected;
                    c.start = p.start;
                    c.page = Some(Arc::new(p));
                    drop(c);
                    s.ctx.request_repaint();
                }
            }
            Err(e) => {
                if s.ready.load(Ordering::Acquire) {
                    s.fail(e)
                }
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}
fn search_worker(s: &Shared) {
    loop {
        let (epoch, q) = {
            let mut c = s.control.lock().unwrap();
            while c.search_operation.is_none() && !s.cancelled() {
                c = s
                    .changed
                    .wait_timeout(c, Duration::from_millis(25))
                    .unwrap()
                    .0;
            }
            if s.cancelled() {
                return;
            }
            c.search_operation.take().unwrap()
        };
        let cancel = || s.cancelled() || s.search_epoch.load(Ordering::Acquire) != epoch;
        let started = Instant::now();
        let result = (|| -> Result<(), String> {
            let _lease = s.budget.reserve(
                Resource::Memory,
                2 * 1024 * 1024,
                "streaming search workspace",
            )?;
            let matcher = super::search::Matcher::new(q, &s.budget)?;
            let mut output = HitWriter::new(s, &cancel, true)?;
            output.hits.generation = epoch;
            output.notify(s, false, true);
            let streaming = !s.ready.load(Ordering::Acquire);
            if !streaming {
                let root = s.index.lock().unwrap().root;
                search_node(s, root, &matcher, &mut output, &cancel)?;
            } else {
                let mut r = Reader::new(&s.snapshot)?.with_cancellation(&cancel);
                if r.peek()? == Some(0xef) {
                    for b in [0xef, 0xbb, 0xbf] {
                        r.expect(b)?;
                    }
                }
                let mut parser = Parser {
                    s,
                    r,
                    cancel: &cancel,
                    local: None,
                    search: Some((&matcher, &mut output)),
                };
                parser.value((NONE, NONE), (NONE, NONE, 0), 0, 0, true)?;
                space(&mut parser.r)?;
                if parser.r.peek()?.is_some() {
                    return Err(parser.r.error("trailing characters"));
                }
            }
            if cancel() {
                return Err("Search cancelled".into());
            }
            output.prune(s, &cancel)?;
            // Duplicates retain their first directory position and last value.
            // Rebuild only this case in canonical directory order after the stream validates.
            if streaming && s.index.lock().unwrap().replacements != 0 {
                let mut final_output = HitWriter::new(s, &cancel, false)?;
                final_output.hits.generation = epoch;
                let root = s.index.lock().unwrap().root;
                search_node(s, root, &matcher, &mut final_output, &cancel)?;
                final_output.publish = true;
                final_output.started = started;
                final_output.notify(s, true, true);
            } else {
                output.notify(s, true, true);
            }
            Ok(())
        })();
        if !cancel()
            && let Err(e) = result
        {
            s.control.lock().unwrap().search_update = Some(SearchUpdate {
                generation: epoch,
                result: Err(e),
                complete: true,
                elapsed: started.elapsed(),
            });
            s.ctx.request_repaint();
        }
    }
}
fn operation_worker(s: &Shared, export: bool) {
    loop {
        let (epoch, op) = {
            let mut c = s.control.lock().unwrap();
            while (if export {
                c.export_operation.is_none()
            } else {
                c.operation.is_none()
            }) && !s.cancelled()
            {
                c = s
                    .changed
                    .wait_timeout(c, Duration::from_millis(25))
                    .unwrap()
                    .0;
            }
            if s.cancelled() {
                return;
            }
            if export {
                c.export_operation.take().unwrap()
            } else {
                c.operation.take().unwrap()
            }
        };
        let cancel = || {
            s.cancelled()
                || (if export {
                    s.export_epoch.load(Ordering::Relaxed)
                } else {
                    s.epoch.load(Ordering::Relaxed)
                }) != epoch
        };
        let start = Instant::now();
        let result = operate(s, op, &cancel);
        if !cancel() {
            let completion = Some((
                epoch,
                Completion {
                    export,
                    result,
                    elapsed: start.elapsed(),
                },
            ));
            let mut c = s.control.lock().unwrap();
            if export {
                c.export_completion = completion;
            } else {
                c.completion = completion;
            }
            drop(c);
            s.ctx.request_repaint();
        }
    }
}

fn copy_range(
    s: &Shared,
    start: u64,
    end: u64,
    out: &mut dyn Write,
    cancel: &dyn Fn() -> bool,
) -> Result<(), String> {
    if start > end || end > s.source.bytes {
        return Err("Invalid source range".into());
    }
    let mut buf = vec![0; 65536];
    let mut pos = start;
    while pos < end {
        if cancel() {
            return Err("Operation cancelled".into());
        }
        let n = (end - pos).min(buf.len() as u64) as usize;
        s.snapshot
            .read_cancellable(pos, &mut buf[..n], cancel)
            .map_err(|e| e.to_string())?;
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        pos += n as u64;
    }
    Ok(())
}
fn write_node(
    s: &Shared,
    id: u64,
    out: &mut dyn Write,
    cancel: &dyn Fn() -> bool,
    depth: usize,
) -> Result<(), String> {
    if cancel() {
        return Err("Operation cancelled".into());
    }
    if depth > 128 {
        return Err("Depth limit".into());
    }
    let r = s.index.lock().unwrap().node(id)?;
    if r[3] == NONE {
        return Err("Selected value is still being indexed".into());
    }
    let k = kind_from(r[1])?;
    if k.container() {
        out.write_all(if k == Kind::Object { b"{" } else { b"[" })
            .map_err(|e| e.to_string())?;
        for n in 0..r[9] {
            if n != 0 {
                out.write_all(b",").map_err(|e| e.to_string())?;
            }
            let child = s
                .index
                .lock()
                .unwrap()
                .child(r, n)?
                .ok_or("Missing child")?;
            if k == Kind::Object {
                let cr = s.index.lock().unwrap().node(child)?;
                copy_range(s, cr[5], cr[6], out, cancel)?;
                out.write_all(b":").map_err(|e| e.to_string())?;
            }
            write_node(s, child, out, cancel, depth + 1)?;
        }
        out.write_all(if k == Kind::Object { b"}" } else { b"]" })
            .map_err(|e| e.to_string())?;
    } else {
        copy_range(s, r[2], r[3], out, cancel)?;
    }
    Ok(())
}
fn write_ids(
    s: &Shared,
    ids: &[u64],
    array: bool,
    out: &mut dyn Write,
    cancel: &dyn Fn() -> bool,
) -> Result<(), String> {
    if array {
        out.write_all(b"[").map_err(|e| e.to_string())?;
    }
    for (n, id) in ids.iter().enumerate() {
        if array && n != 0 {
            out.write_all(b",").map_err(|e| e.to_string())?;
        }
        write_node(s, *id, out, cancel, 0)?;
    }
    if array {
        out.write_all(b"]").map_err(|e| e.to_string())?;
    }
    Ok(())
}
struct Limited {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Limited {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(b.len()) > self.limit {
            return Err(std::io::Error::other(
                "JSON exceeds copy/preview limit; use Export",
            ));
        }
        self.bytes.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn operate(s: &Shared, op: Operation, cancel: &dyn Fn() -> bool) -> Result<Output, String> {
    let workspace = match &op {
        Operation::Export { .. } => 512 * 1024,
        Operation::Search(_) | Operation::Query(_) => 2 * 1024 * 1024,
        _ => 12 * 1024 * 1024,
    };
    let _lease = s.budget.reserve(
        Resource::Memory,
        workspace,
        "bounded file operation and result workspace",
    )?;
    if !s.ready.load(Ordering::Acquire) && !matches!(op, Operation::Path(_)) {
        return Err("File indexing has not completed; retry when ready".into());
    }
    match op {
        Operation::Path(id) => {
            if s.index.lock().unwrap().live_ancestor(id)? != id {
                return Err("Selected value was replaced; select it again".into());
            }
            Ok(Output::Copy(path(s, id, 8 * 1024 * 1024, cancel)?))
        }
        Operation::Search(q) => {
            let matcher = super::search::Matcher::new(q, &s.budget)?;
            let root = s.index.lock().unwrap().root;
            let mut ids = HitWriter::new(s, cancel, false)?;
            search_node(s, root, &matcher, &mut ids, cancel)?;
            Ok(Output::Search(ids.finish()))
        }
        Operation::Query(q) => {
            let ids = query(s, &q, cancel)?;
            let mut out = Limited {
                bytes: Vec::new(),
                limit: 16 * 1024,
            };
            let preview = match write_ids(s, &ids, true, &mut out, cancel) {
                Ok(()) => String::from_utf8(out.bytes).map_err(|e| e.to_string())?,
                Err(e) => format!("{}…\n{e}", String::from_utf8_lossy(&out.bytes)),
            };
            Ok(Output::Query { ids, preview })
        }
        Operation::Copy { ids, array } => {
            let mut out = Limited {
                bytes: Vec::new(),
                limit: 8 * 1024 * 1024,
            };
            write_ids(s, &ids, array, &mut out, cancel)?;
            Ok(Output::Copy(
                String::from_utf8(out.bytes).map_err(|e| e.to_string())?,
            ))
        }
        Operation::Export { ids, array, path } => {
            let flag = AtomicBool::new(false);
            crate::export::atomic_write_with_budget(&path, &flag, &s.budget, |out| {
                write_ids(s, &ids, array, out, cancel)
                    .map_err(|e| serde_json::Error::io(std::io::Error::other(e)))
            })?;
            Ok(Output::Export(path))
        }
    }
}
fn search_node(
    s: &Shared,
    id: u64,
    matcher: &super::search::Matcher,
    output: &mut HitWriter,
    cancel: &dyn Fn() -> bool,
) -> Result<(), String> {
    if cancel() {
        return Err("Search cancelled".into());
    }
    let r = s.index.lock().unwrap().node(id)?;
    let k = kind_from(r[1])?;
    let mut reader = Reader::new(&s.snapshot)?.with_cancellation(cancel);
    let hit =
        (matcher.keys() && r[5] != NONE && matcher.range(&mut reader, r[5], r[6], true, cancel)?)
            || (matcher.values()
                && !k.container()
                && matcher.range(&mut reader, r[2], r[3], k == Kind::String, cancel)?);
    drop(reader);
    if hit {
        output.push(s, id, r[2], cancel)?;
    }
    for n in 0..r[9] {
        let child = s
            .index
            .lock()
            .unwrap()
            .child(r, n)?
            .ok_or("Missing child")?;
        search_node(s, child, matcher, output, cancel)?;
    }
    Ok(())
}
fn append(out: &mut Vec<u64>, id: u64) -> Result<(), String> {
    if out.len() == LIMIT {
        return Err("Compatibility query is limited to 65536 results".into());
    }
    out.push(id);
    Ok(())
}
fn query(s: &Shared, q: &str, cancel: &dyn Fn() -> bool) -> Result<Vec<u64>, String> {
    use crate::document::indexed::{Selector, path_segments};
    if q.len() > 64 * 1024 {
        return Err("Query exceeds 64 KiB".into());
    }
    let q = q.trim();
    let normalized;
    let q = if q == "." {
        "$"
    } else if q.starts_with('.') || q.starts_with('[') {
        normalized = format!("${q}");
        &normalized
    } else {
        q
    };
    let root = s.index.lock().unwrap().root;
    let pointer = q.starts_with('/');
    if !pointer && !q.is_empty() {
        serde_json_path::JsonPath::parse(q).map_err(|e| format!("Invalid JSONPath: {e}"))?;
    }
    let groups = if let Some(pointer) = q.strip_prefix('/') {
        pointer
            .split('/')
            .map(|part| {
                let mut key = String::new();
                let mut chars = part.chars();
                while let Some(c) = chars.next() {
                    key.push(if c == '~' {
                        match chars.next() {
                            Some('0') => '~',
                            Some('1') => '/',
                            _ => return Err("Invalid JSON Pointer escape".to_string()),
                        }
                    } else {
                        c
                    });
                }
                Ok((false, vec![Selector::Name(key)]))
            })
            .collect::<Result<Vec<_>, String>>()?
    } else {
        path_segments(if q.is_empty() { "$" } else { q }).map_err(|_|"File session JSONPath supports fields, unions, wildcard, recursion and slices; filters/functions are unsupported. No automatic full-file parse fallback.".to_string())?
    };
    let mut ids = vec![root];
    for (recursive, selectors) in groups {
        let mut next = Vec::new();
        for id in ids {
            query_step(s, id, recursive, &selectors, &mut next, cancel, pointer)?;
        }
        ids = next;
    }
    Ok(ids)
}
fn query_step(
    s: &Shared,
    id: u64,
    recursive: bool,
    selectors: &[crate::document::indexed::Selector],
    out: &mut Vec<u64>,
    cancel: &dyn Fn() -> bool,
    pointer: bool,
) -> Result<(), String> {
    use crate::document::indexed::Selector;
    if cancel() {
        return Err("Query cancelled".into());
    }
    let r = s.index.lock().unwrap().node(id)?;
    if r[1] > 1 {
        return Ok(());
    }
    let child = |n: u64| {
        s.index
            .lock()
            .unwrap()
            .child(r, n)?
            .ok_or_else(|| "Missing child".to_string())
    };
    for selector in selectors {
        match selector {
            Selector::Name(key) => {
                if r[1] == 1 && pointer {
                    if let Ok(n) = key.parse::<u64>()
                        && n < r[9]
                        && n.to_string() == *key
                    {
                        append(out, child(n)?)?;
                    }
                } else if r[1] == 0 {
                    for n in 0..r[9] {
                        if cancel() {
                            return Err("Query cancelled".into());
                        }
                        let id = child(n)?;
                        let cr = s.index.lock().unwrap().node(id)?;
                        let (text, complete) = key_text(s, cr, key.len() + 1, cancel)?;
                        if complete && text == *key {
                            append(out, id)?;
                            break;
                        }
                    }
                }
            }
            Selector::Index(n) => {
                if r[1] == 1 {
                    let n = if *n < 0 { r[9] as i64 + *n } else { *n };
                    if n >= 0 && (n as u64) < r[9] {
                        append(out, child(n as u64)?)?;
                    }
                }
            }
            Selector::Wildcard => {
                for n in 0..r[9] {
                    append(out, child(n)?)?;
                }
            }
            Selector::Slice(start, end, step) => {
                if *step == 0 {
                    return Err("Slice step cannot be zero".into());
                }
                if r[1] == 1 {
                    let len = r[9] as i64;
                    let normalize = |v: i64| if v < 0 { len.saturating_add(v) } else { v };
                    let (mut n, end) = if *step > 0 {
                        (
                            start.map(normalize).unwrap_or(0).clamp(0, len),
                            end.map(normalize).unwrap_or(len).clamp(0, len),
                        )
                    } else {
                        (
                            start.map(normalize).unwrap_or(len - 1).clamp(-1, len - 1),
                            end.map(normalize).unwrap_or(-1).clamp(-1, len - 1),
                        )
                    };
                    while if *step > 0 { n < end } else { n > end } {
                        if cancel() {
                            return Err("Query cancelled".into());
                        }
                        append(out, child(n as u64)?)?;
                        let Some(next) = n.checked_add(*step) else {
                            break;
                        };
                        n = next;
                    }
                }
            }
        }
    }
    if recursive {
        for n in 0..r[9] {
            query_step(s, child(n)?, true, selectors, out, cancel, pointer)?;
        }
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    struct Input(std::path::PathBuf);
    impl Input {
        fn new(bytes: &[u8]) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "FerrisJson-recursive-{}-{}.tmp",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&p, bytes).unwrap();
            Self(p)
        }
    }
    impl Drop for Input {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn open(input: &Input) -> Session {
        Session::open(
            &input.0,
            Arc::new(AtomicBool::new(false)),
            Default::default(),
        )
        .unwrap()
    }
    fn ready(s: &Session) {
        let until = Instant::now() + Duration::from_secs(60);
        while !s.statistics().ready {
            assert!(s.error().is_none());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn page(s: &Session, predicate: impl Fn(&Page) -> bool) -> Arc<Page> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(p) = s.poll()
                && predicate(&p)
            {
                return p;
            }
            assert!(s.error().is_none());
            assert!(Instant::now() < until, "page timeout");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn retire(s: Session) {
        let b = s.budget();
        s.retire().join().unwrap();
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    fn value(s: &Session, id: u64) -> serde_json::Value {
        let mut out = Vec::new();
        write_node(&s.shared, id, &mut out, &|| false, 0).unwrap();
        serde_json::from_slice(&out).unwrap()
    }
    #[test]
    fn recursive_tree_duplicates_queries_and_semantic_output() {
        let text=br#"{"a/b":{"old":1},"rows":[{"id":1},{"id":2},{"id":3}],"a/b":{"new":true},"x\u0079":"\uD83D\uDE00","xy":"last"}"#;
        let input = Input::new(text);
        let s = open(&input);
        ready(&s);
        let root = s.shared.index.lock().unwrap().root;
        assert_eq!(
            value(&s, root),
            serde_json::from_slice::<serde_json::Value>(text).unwrap()
        );
        for (q, expected) in [
            ("/a~1b/new", serde_json::json!([true])),
            ("$.rows[*].id", serde_json::json!([1, 2, 3])),
            (" .rows[*].id ", serde_json::json!([1, 2, 3])),
            ("$..id", serde_json::json!([1, 2, 3])),
            ("$.rows[2,0,2].id", serde_json::json!([3, 1, 3])),
            ("$.rows[::-1].id", serde_json::json!([3, 2, 1])),
        ] {
            let ids = query(&s.shared, q, &|| false).unwrap();
            assert_eq!(
                ids.into_iter().map(|id| value(&s, id)).collect::<Vec<_>>(),
                expected.as_array().unwrap().clone()
            );
        }
        assert!(query(&s.shared, "$.rows[?(@.id)]", &|| false).is_err());
        assert!(query(&s.shared, "/a~2b", &|| false).is_err());
        s.action(Action::All(true));
        let p = page(&s, |p| p.count > 8);
        assert!(p.rows.iter().any(|r| r.depth == 3 && r.label == "id"));
        let id = query(&s.shared, "/rows/2/id", &|| false).unwrap()[0];
        drop(p);
        s.action(Action::Select(id));
        let p = page(&s, |p| p.selected == id);
        assert_eq!(p.selection.path, "$.rows[2].id");
        drop(p);
        s.action(Action::All(false));
        drop(page(&s, |p| p.count == 1));
        retire(s);
    }
    #[test]
    fn strict_stream_validation_and_cancel_release_resources() {
        for text in [
            b"[01]".as_slice(),
            br#"{"a":1,}"#,
            br#"["\uD800"]"#,
            br#"[true]x"#,
            br#"[1e+]"#,
            b"[\"\xff\"]",
        ] {
            let input = Input::new(text);
            let s = open(&input);
            let until = Instant::now() + Duration::from_secs(5);
            while s.error().is_none() {
                assert!(!s.statistics().ready);
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(1));
            }
            retire(s);
        }
        let input = Input::new(
            format!(
                "{{\"first\":\"{}\",\"tail\":1}}",
                "x".repeat(16 * 1024 * 1024)
            )
            .as_bytes(),
        );
        let s = open(&input);
        let p = page(&s, |p| p.rows.iter().any(|r| r.label == "first"));
        assert!(!p.ready);
        assert!(p.rows.iter().any(|r| r.label == "first" && !r.complete));
        drop(p);
        retire(s);
    }
    #[test]
    fn priority_and_sequential_scans_merge_without_duplicate_children() {
        let input = Input::new(
            br#"{"a":{"same":{"old":0},"x":1,"same":{"new":[1,2]}},"a":{"same":{"last":3}},"z":4}"#,
        );
        let s = open(&input);
        ready(&s);
        let a = query(&s.shared, "/a", &|| false).unwrap()[0];
        let r = s.shared.index.lock().unwrap().node(a).unwrap();
        let hash = {
            let mut reader = Reader::new(&s.shared.snapshot).unwrap();
            reader.pos = r[5];
            string(&mut reader).unwrap()
        };
        let cancel = || false;
        let mut reader = Reader::new(&s.shared.snapshot).unwrap();
        reader.pos = r[2];
        let mut p = Parser {
            s: &s.shared,
            r: reader,
            cancel: &cancel,
            local: Some((r[2], 64)),
            search: None,
        };
        let parent_start = s.shared.index.lock().unwrap().node(r[4]).unwrap()[2];
        p.value((r[4], parent_start), (r[5], r[6], hash), r[7], 0, true)
            .unwrap();
        assert_eq!(value(&s, a), serde_json::json!({"same":{"last":3}}));
        drop(p);
        retire(s);
    }
    #[test]
    fn long_keys_are_streamed_and_search_uses_decoded_values() {
        let key = "界".repeat(30_000);
        let input = Input::new(
            format!(
                "{{{}:{{\"value\":\"a\\u0062c\"}},\"n\":1E2}}",
                serde_json::to_string(&key).unwrap()
            )
            .as_bytes(),
        );
        let s = open(&input);
        ready(&s);
        let root = s.shared.index.lock().unwrap().root;
        let r = s.shared.index.lock().unwrap().node(root).unwrap();
        let ids = [s.shared.index.lock().unwrap().child(r, 0).unwrap().unwrap()];
        assert_eq!(ids.len(), 1);
        assert!(path(&s.shared, ids[0], 16 * 1024, &|| false).is_err());
        assert!(
            path(&s.shared, ids[0], 8 * 1024 * 1024, &|| false)
                .unwrap()
                .contains(&key)
        );
        let out = operate(
            &s.shared,
            Operation::Search(super::super::search::Query::literal("abc")),
            &|| false,
        )
        .unwrap();
        assert!(matches!(out,Output::Search(ids) if ids.len()==1));
        retire(s);
    }
    #[test]
    fn wide_index_evicts_records_and_high_hits_stay_on_disk() {
        let input = Input::new(
            format!(
                "[{}]",
                std::iter::repeat_n("1", 70_000)
                    .collect::<Vec<_>>()
                    .join(",")
            )
            .as_bytes(),
        );
        let s = open(&input);
        ready(&s);
        let root = s.shared.index.lock().unwrap().root;
        let r = s.shared.index.lock().unwrap().node(root).unwrap();
        assert_eq!(r[9], 70_000);
        for n in [0, 35_000, 69_999] {
            let id = s.shared.index.lock().unwrap().child(r, n).unwrap().unwrap();
            assert_eq!(value(&s, id), serde_json::json!(1));
        }
        let result = operate(
            &s.shared,
            Operation::Search(super::super::search::Query::literal("1")),
            &|| false,
        )
        .unwrap();
        let Output::Search(hits) = result else {
            panic!("Wrong output")
        };
        assert_eq!(hits.len(), 70_000);
        for n in [0, 35_000, 69_999] {
            assert_eq!(
                path(&s.shared, hits.get(n).unwrap(), 1024, &|| false).unwrap(),
                format!("$[{n}]")
            );
        }
        assert!(
            query(&s.shared, "$[*]", &|| false)
                .unwrap_err()
                .contains("65536")
        );
        assert!(query(&s.shared, "$['0']", &|| false).unwrap().is_empty());
        assert_eq!(query(&s.shared, "/0", &|| false).unwrap().len(), 1);
        drop(hits);
        retire(s);
    }
    #[test]
    fn nested_containers_expand_before_the_giant_descendant_finishes() {
        let input = Input::new(
            format!(
                "{{\"a\":{{\"child\":{{\"text\":\"{}\"}},\"next\":1}},\"tail\":0}}",
                "x".repeat(16 * 1024 * 1024)
            )
            .as_bytes(),
        );
        let s = open(&input);
        let p = page(&s, |p| p.rows.iter().any(|r| r.label == "a"));
        let a = p.rows.iter().find(|r| r.label == "a").unwrap().id;
        drop(p);
        s.action(Action::Toggle(a));
        let p = page(&s, |p| p.rows.iter().any(|r| r.label == "child"));
        assert!(!p.ready);
        let child = p.rows.iter().find(|r| r.label == "child").unwrap().id;
        drop(p);
        s.action(Action::Toggle(child));
        let p = page(&s, |p| p.rows.iter().any(|r| r.label == "text"));
        assert!(!p.ready);
        drop(p);
        s.submit(Operation::Query("$".into()));
        s.cancel_operation();
        s.submit(Operation::Path(child));
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(c) = s.completion() {
                assert!(matches!(c.result,Ok(Output::Copy(text)) if text=="$.a.child"));
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        retire(s);
    }
    #[test]
    fn export_has_its_own_lane_and_cancel_preserves_the_target() {
        let input = Input::new(
            format!(
                "{{\"data\":\"{}\",\"last\":3}}",
                "x".repeat(2 * 1024 * 1024)
            )
            .as_bytes(),
        );
        let target = Input::new(b"original");
        let s = open(&input);
        ready(&s);
        let root = s.shared.index.lock().unwrap().root;
        let calls = AtomicU64::new(0);
        let result = operate(
            &s.shared,
            Operation::Export {
                ids: vec![root],
                array: false,
                path: target.0.clone(),
            },
            &|| calls.fetch_add(1, Ordering::Relaxed) > 3,
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&target.0).unwrap(), b"original");
        s.submit(Operation::Export {
            ids: vec![root],
            array: false,
            path: target.0.clone(),
        });
        s.submit(Operation::Query("$.last".into()));
        let until = Instant::now() + Duration::from_secs(10);
        let (mut exported, mut queried) = (false, false);
        while !exported || !queried {
            if let Some(c) = s.completion() {
                match c.result.unwrap() {
                    Output::Export(_) => exported = true,
                    Output::Query { ids, .. } => {
                        queried = true;
                        assert_eq!(value(&s, ids[0]), serde_json::json!(3));
                    }
                    _ => panic!("Unexpected output"),
                }
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&target.0).unwrap())
                .unwrap(),
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&input.0).unwrap()).unwrap()
        );
        retire(s);
    }
    #[test]
    fn a_priority_pass_ahead_of_validation_cannot_restore_shadowed_subtrees() {
        let text = br#"{"a":{"old":{"nested":0}},"a":{"new":[1,2,3]},"z":false}"#;
        let input = Input::new(text);
        let cancel = Arc::new(AtomicBool::new(true));
        let mut s = Session::start(
            Source::open(&input.0).unwrap(),
            cancel.clone(),
            Default::default(),
        )
        .unwrap();
        for t in std::mem::take(&mut s.threads) {
            t.join().unwrap();
        }
        cancel.store(false, Ordering::Relaxed);
        let no_cancel = || false;
        let reader = Reader::new(&s.shared.snapshot).unwrap();
        let mut p = Parser {
            s: &s.shared,
            r: reader,
            cancel: &no_cancel,
            local: Some((0, 64)),
            search: None,
        };
        p.value((NONE, NONE), (NONE, NONE, 0), 0, 0, true).unwrap();
        drop(p);
        let root = s.shared.index.lock().unwrap().root;
        let r = s.shared.index.lock().unwrap().node(root).unwrap();
        let a = s.shared.index.lock().unwrap().child(r, 0).unwrap().unwrap();
        let a_record = s.shared.index.lock().unwrap().node(a).unwrap();
        assert_ne!(a_record[3], NONE);
        assert_eq!(a_record[0], 6);
        assert_eq!(a_record[9], 0);
        apply(&s.shared, Action::Toggle(a)).unwrap();
        assert_eq!(
            s.shared
                .control
                .lock()
                .unwrap()
                .priority
                .as_ref()
                .map(|t| t.1),
            Some(a)
        );
        scan_all(&s.shared).unwrap();
        assert_eq!(s.shared.index.lock().unwrap().node(a).unwrap()[0], 9);
        let root = s.shared.index.lock().unwrap().root;
        assert_eq!(
            value(&s, root),
            serde_json::from_slice::<serde_json::Value>(text).unwrap()
        );
        retire(s);
    }
    #[test]
    fn stale_child_writers_and_clicks_cannot_reattach_a_replaced_subtree() {
        let text = br#"{"a":{"old":{"nested":0}},"a":{"new":[1,2]},"z":false}"#;
        let input = Input::new(text);
        let cancel = Arc::new(AtomicBool::new(true));
        let mut s = Session::start(
            Source::open(&input.0).unwrap(),
            cancel.clone(),
            Default::default(),
        )
        .unwrap();
        for t in std::mem::take(&mut s.threads) {
            t.join().unwrap();
        }
        cancel.store(false, Ordering::Relaxed);
        let root = s
            .shared
            .register(Kind::Object, 0, (NONE, NONE), (NONE, NONE, 0), 0)
            .unwrap();
        let no_cancel = || false;
        let mut reader = Reader::new(&s.shared.snapshot).unwrap();
        reader.pos = 1;
        let hash = string(&mut reader).unwrap();
        reader.pos = 5;
        let mut p = Parser {
            s: &s.shared,
            r: reader,
            cancel: &no_cancel,
            local: None,
            search: None,
        };
        p.value((root, 0), (1, 4, hash), 0, 0, true).unwrap();
        drop(p);
        let root_record = s.shared.index.lock().unwrap().node(root).unwrap();
        let a = s
            .shared
            .index
            .lock()
            .unwrap()
            .child(root_record, 0)
            .unwrap()
            .unwrap();
        let a_record = s.shared.index.lock().unwrap().node(a).unwrap();
        let old = s
            .shared
            .index
            .lock()
            .unwrap()
            .child(a_record, 0)
            .unwrap()
            .unwrap();
        let old_start = s.shared.index.lock().unwrap().node(old).unwrap()[2];
        let reader = Reader::new(&s.shared.snapshot).unwrap();
        let mut p = Parser {
            s: &s.shared,
            r: reader,
            cancel: &no_cancel,
            local: Some((0, 64)),
            search: None,
        };
        p.value((NONE, NONE), (NONE, NONE, 0), 0, 0, true).unwrap();
        drop(p);
        let before = s.shared.index.lock().unwrap().node(a).unwrap();
        apply(&s.shared, Action::Toggle(old)).unwrap();
        assert_eq!(s.shared.index.lock().unwrap().node(a).unwrap(), before);
        assert_eq!(
            s.shared
                .register(
                    Kind::Null,
                    old_start + 1,
                    (old, old_start),
                    (NONE, NONE, 0),
                    0
                )
                .unwrap(),
            NONE
        );
        scan_all(&s.shared).unwrap();
        assert_eq!(
            value(&s, root),
            serde_json::from_slice::<serde_json::Value>(text).unwrap()
        );
        retire(s);
    }
    fn search_only(input: &Input) -> Session {
        let cancel = Arc::new(AtomicBool::new(true));
        let mut s = Session::start(
            Source::open(&input.0).unwrap(),
            cancel.clone(),
            Default::default(),
        )
        .unwrap();
        for t in std::mem::take(&mut s.threads) {
            t.join().unwrap();
        }
        cancel.store(false, Ordering::Relaxed);
        let shared = s.shared.clone();
        s.threads
            .push(std::thread::spawn(move || search_worker(&shared)));
        s
    }
    fn search_done(s: &Session) -> Hits {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(update) = s.search_update() {
                let hits = update.result.unwrap();
                if update.complete {
                    return hits;
                }
            }
            assert!(Instant::now() < until, "search timeout");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn search_publishes_and_reveals_before_value_end_without_global_indexer() {
        let input = Input::new(
            format!(
                "{{\"a\":{{\"text\":\"needle{}\"}},\"tail\":0}}",
                "x".repeat(16 * 1024 * 1024)
            )
            .as_bytes(),
        );
        let s = search_only(&input);
        s.submit(Operation::Search(super::super::search::Query::literal(
            "needle",
        )));
        let until = Instant::now() + Duration::from_secs(10);
        let first = loop {
            if let Some(update) = s.search_update() {
                let hits = update.result.unwrap();
                if !hits.is_empty() {
                    assert!(!update.complete);
                    break hits;
                }
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(!s.statistics().ready);
        let id = first.get(0).unwrap();
        assert_eq!(s.shared.index.lock().unwrap().node(id).unwrap()[3], NONE);
        apply(&s.shared, Action::Hit(first.clone(), 0)).unwrap();
        assert_eq!(s.shared.control.lock().unwrap().selected, id);
        assert_eq!(path(&s.shared, id, 1024, &|| false).unwrap(), "$.a.text");
        // Regular operations have an independent cancellation generation.
        s.submit(Operation::Path(id));
        s.cancel_operation();
        let hits = search_done(&s);
        assert_eq!(hits.len(), 1);
        assert!(!s.statistics().ready);
        drop(first);
        drop(hits);
        retire(s);
    }
    #[test]
    fn streaming_duplicate_hits_are_retracted_and_final_order_is_canonical() {
        let input =
            Input::new(br#"{"a":{"old":"hit"},"b":"hit","a":{"new":"hit"},"c":"hit","c":"gone"}"#);
        let s = search_only(&input);
        s.submit(Operation::Search(super::super::search::Query::literal(
            "hit",
        )));
        let hits = search_done(&s);
        assert_eq!(hits.len(), 2);
        assert_eq!(
            path(&s.shared, hits.get(0).unwrap(), 1024, &|| false).unwrap(),
            "$.a.new"
        );
        assert_eq!(
            path(&s.shared, hits.get(1).unwrap(), 1024, &|| false).unwrap(),
            "$.b"
        );
        assert_eq!(s.shared.index.lock().unwrap().replacements, 2);
        drop(hits);
        retire(s);
    }
    #[test]
    fn stale_hits_cannot_select_a_new_value_reusing_the_same_node_id() {
        let input = Input::new(br#"{"a":"hit","a":"gone"}"#);
        let s = search_only(&input);
        let root = s
            .shared
            .register(Kind::Object, 0, (NONE, NONE), (NONE, NONE, 0), 0)
            .unwrap();
        let mut r = Reader::new(&s.shared.snapshot).unwrap();
        r.pos = 1;
        let hash = string(&mut r).unwrap();
        drop(r);
        let id = s
            .shared
            .register(Kind::String, 5, (root, 0), (1, 4, hash), 0)
            .unwrap();
        let mut writer = HitWriter::new(&s.shared, &|| false, false).unwrap();
        writer.push(&s.shared, id, 5, &|| false).unwrap();
        let mut active = HitWriter::new(&s.shared, &|| false, false).unwrap();
        active.push(&s.shared, id, 5, &|| false).unwrap();
        let hits = writer.finish();
        s.shared
            .register(Kind::String, 15, (root, 0), (11, 14, hash), 1)
            .unwrap();
        apply(&s.shared, Action::Hit(hits.clone(), 0)).unwrap();
        assert_ne!(s.shared.control.lock().unwrap().selected, id);
        active.prune(&s.shared, &|| false).unwrap();
        let pruned = active.finish();
        assert_eq!(pruned.len(), 0);
        assert!(pruned.get(0).is_err());
        drop(pruned);
        drop(hits);
        retire(s);
    }
    #[test]
    fn streaming_search_cancellation_and_new_generation_discard_old_updates() {
        let input = Input::new(
            format!("[\"needle{}\",\"latest\"]", "x".repeat(4 * 1024 * 1024)).as_bytes(),
        );
        let s = search_only(&input);
        s.submit(Operation::Search(super::super::search::Query::literal(
            "needle",
        )));
        s.clear_search();
        s.submit(Operation::Search(super::super::search::Query::literal(
            "latest",
        )));
        let hits = search_done(&s);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            path(&s.shared, hits.get(0).unwrap(), 1024, &|| false).unwrap(),
            "$[1]"
        );
        s.clear_search();
        assert!(s.search_update().is_none());
        drop(hits);
        retire(s);
    }
    #[test]
    fn streaming_search_rejects_invalid_tail_and_oversized_regex() {
        for (text, regex, expected) in [
            (br#"["hit",0] trailing"#.to_vec(), false, "trailing"),
            (
                format!("[\"hit{}\"]", "x".repeat(300 * 1024)).into_bytes(),
                true,
                "256 KiB",
            ),
        ] {
            let input = Input::new(&text);
            let s = search_only(&input);
            let mut q = super::super::search::Query::literal("hit");
            q.regex = regex;
            s.submit(Operation::Search(q));
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(update) = s.search_update()
                    && update.complete
                {
                    assert!(update.result.err().unwrap().contains(expected));
                    break;
                }
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(1));
            }
            retire(s);
        }
    }
    #[test]
    fn stream_and_ready_search_agree_for_decoded_and_chunk_boundary_literals() {
        let text = format!(
            r#"{{"na\u006de":"{}\uD83D\uDE00AbC","n":1E2,"a":[true,null,"ABC"]}}"#,
            "x".repeat(65535)
        );
        let input = Input::new(text.as_bytes());
        let s = search_only(&input);
        for (term, expected) in [
            ("😀abc", 1),
            ("name", 1),
            ("e+2", 1),
            ("true", 1),
            ("missing", 0),
        ] {
            let q = super::super::search::Query::literal(term);
            s.submit(Operation::Search(q.clone()));
            let hits = search_done(&s);
            assert_eq!(hits.len(), expected, "{term}");
            // The streaming parser has built complete ranges, but global Ready stays false.
            s.shared.ready.store(true, Ordering::Release);
            let Output::Search(reference) =
                operate(&s.shared, Operation::Search(q), &|| false).unwrap()
            else {
                panic!()
            };
            assert_eq!(hits.len(), reference.len(), "{term}");
            for n in 0..hits.len() {
                assert_eq!(hits.get(n).unwrap(), reference.get(n).unwrap());
            }
            s.shared.ready.store(false, Ordering::Release);
            drop(hits);
            drop(reference);
        }
        retire(s);
    }
}
