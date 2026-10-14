use super::*;
use crate::engine::session::SessionStatus;
use std::path::PathBuf;

/// Stable snapshots only. Strict reuse compares every byte; no sampled reuse.
#[derive(Clone, Default)]
pub struct OpenOptions {
    pub cache_dir: Option<PathBuf>,
    /// Byte progress inside giant scalar values; it is not a committed frontier.
    pub progress: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// Publish independent immutable read views for the S5 interactive lane.
    pub live_views: bool,
}

pub struct CopiedValue {
    pub text: String,
    pub(super) _lease: Reservation,
}

pub struct PublishedPage {
    pub nodes: Vec<Node>,
    pub labels: Vec<String>,
    pub previews: Vec<String>,
    pub total: Option<u64>,
    pub next: Option<u64>,
    _lease: Reservation,
}
pub struct Event {
    pub session: SessionId,
    pub generation: u64,
    pub commit_sequence: u64,
    /// Durable source boundary; zero when persistence is disabled.
    pub committed_frontier: u64,
    pub status: SessionStatus,
    pub scanned: u64,
    pub total_bytes: u64,
    pub root_kind: Option<Kind>,
    pub page: Option<PublishedPage>,
    pub reused: bool,
    pub resumed: bool,
    pub message: Option<String>,
    pub view: Option<Box<SparseIndex>>,
}
pub(super) struct Checkpoint {
    pub source_len: u64,
    pub index_len: u64,
    pub position: u64,
    pub stack: Vec<Frame>,
    pub root: Option<Node>,
}
impl Checkpoint {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(64 * 1024);
        bytes.extend_from_slice(b"FJST0002");
        let mut word = |n: u64| bytes.extend_from_slice(&n.to_le_bytes());
        for n in [
            2,
            self.source_len,
            self.index_len,
            self.position,
            self.stack.len() as u64,
            u64::from(self.root.is_some()),
        ] {
            word(n);
        }
        if let Some(n) = self.root {
            for v in [n.start, n.end, n.kind.code(), n.container, n.count] {
                word(v);
            }
        }
        for f in &self.stack {
            for v in f.record {
                word(v);
            }
            for v in [
                f.offset,
                f.phase as u64,
                f.key[0],
                f.key[1],
                f.key[2],
                f.checkpoint_at,
                f.checkpoint_count,
            ] {
                word(v);
            }
        }
        bytes
    }
    pub(super) fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 56 || &bytes[..8] != b"FJST0002" {
            return Err("Invalid checkpoint format".into());
        }
        let mut pos = 8;
        let mut word = || -> Result<u64, String> {
            let b = bytes.get(pos..pos + 8).ok_or("Truncated checkpoint")?;
            pos += 8;
            Ok(u64::from_le_bytes(b.try_into().unwrap()))
        };
        if word()? != 2 {
            return Err("Unsupported scanner semantics".into());
        }
        let source_len = word()?;
        let index_len = word()?;
        let position = word()?;
        let count = word()?;
        let ready = word()?;
        if count > DEPTH as u64
            || ready > 1
            || position > source_len
            || (ready == 1 && count != 0)
            || (ready == 0 && count == 0)
        {
            return Err("Invalid checkpoint state".into());
        }
        let session = SessionId::fresh();
        let root = if ready == 1 {
            let n = Node {
                start: word()?,
                end: word()?,
                kind: Kind::decode(word()?)?,
                container: word()?,
                count: word()?,
                session,
            };
            if n.start >= n.end || n.end > source_len {
                return Err("Invalid checkpoint root range".into());
            }
            Some(n)
        } else {
            None
        };
        let mut stack = Vec::with_capacity(DEPTH);
        for _ in 0..count {
            let mut record = [0; 12];
            for v in &mut record {
                *v = word()?;
            }
            let offset = word()?;
            let phase = word()?;
            let key = [word()?, word()?, word()?];
            let checkpoint_at = word()?;
            let checkpoint_count = word()?;
            if record[0] != 1
                || !Kind::decode(record[1])?.container()
                || record[2] >= position
                || phase > 3
                || checkpoint_at > position
                || checkpoint_count > record[4]
                || offset >= index_len
            {
                return Err("Invalid checkpoint parser frame".into());
            }
            if stack
                .last()
                .is_some_and(|f: &Frame| f.record[2] >= record[2])
            {
                return Err("Invalid checkpoint parent stack".into());
            }
            stack.push(Frame {
                record,
                offset,
                phase: phase as u8,
                key,
                checkpoint_at,
                checkpoint_count,
            });
        }
        if pos != bytes.len() {
            return Err("Checkpoint trailing bytes".into());
        }
        Ok(Self {
            source_len,
            index_len,
            position,
            stack,
            root,
        })
    }
}

pub(super) struct Publisher<'a> {
    pub cache: Option<&'a mut cache::Cache>,
    pub budget: &'a BudgetManager,
    pub emit: &'a mut dyn FnMut(Event) -> Result<(), String>,
    pub session: SessionId,
    pub generation: u64,
    pub last_commit: u64,
    pub last_progress: u64,
    pub first: Vec<Node>,
    pub resumed: bool,
    pub reused: bool,
    pub source: &'a std::sync::Arc<Snapshot>,
    pub live: bool,
    pub pending_view: Option<Box<SparseIndex>>,
}
impl Publisher<'_> {
    pub(super) fn event(
        &mut self,
        snapshot: &Snapshot,
        status: SessionStatus,
        pos: u64,
        kind: Option<Kind>,
        page: Option<PublishedPage>,
        message: Option<String>,
    ) -> Result<(), String> {
        (self.emit)(Event {
            session: self.session,
            generation: self.generation,
            commit_sequence: self.cache.as_ref().map_or(0, |c| c.sequence()),
            committed_frontier: if self.cache.is_some() {
                self.last_commit
            } else {
                0
            },
            status,
            scanned: pos,
            total_bytes: snapshot.len,
            root_kind: kind,
            page,
            reused: self.reused,
            resumed: self.resumed,
            message,
            view: self.pending_view.take(),
        })
    }
    pub(super) fn safe(
        &mut self,
        scanner: &mut Scanner<'_>,
        node: Option<Node>,
        force: bool,
    ) -> Result<(), String> {
        let pos = scanner.reader.pos;
        let root_kind = scanner
            .stack
            .first()
            .map(|f| Kind::decode(f.record[1]))
            .transpose()?;
        let first_before = self.first.len();
        if let Some(n) = node
            && scanner.stack.len() == 1
            && root_kind == Some(Kind::Array)
            && self.first.len() < 64
            && !self.resumed
        {
            self.first.push(n);
        }
        let first_page = (first_before == 0 && self.first.len() == 1)
            || (first_before < 64 && self.first.len() == 64);
        // Copy-on-checkpoint is intentionally conservative. Frequency bounds
        // overhead; a single giant value may require rescanning after a crash.
        if force || first_page || pos.saturating_sub(self.last_commit) >= 16 * 1024 * 1024 {
            if let Some(cache) = &mut self.cache {
                let checkpoint = Checkpoint {
                    source_len: scanner.snapshot.len,
                    index_len: scanner.store.bytes(),
                    position: pos,
                    stack: scanner.stack.clone(),
                    root: None,
                };
                cache.commit(scanner.store, &checkpoint, scanner.cancelled)?;
            }
            self.last_commit = pos;
        }
        if first_page || force || pos.saturating_sub(self.last_progress) >= 1024 * 1024 {
            if self.live
                && root_kind == Some(Kind::Array)
                && (force || first_page || self.last_commit == pos)
            {
                self.pending_view = Some(Box::new(self.freeze(scanner)?));
            }
            let page = if first_page {
                Some(published(
                    scanner.snapshot,
                    &self.first,
                    None,
                    Some(self.first.len() as u64),
                    self.budget,
                    scanner.cancelled,
                )?)
            } else {
                None
            };
            self.event(
                scanner.snapshot,
                SessionStatus::Validating,
                pos,
                root_kind,
                page,
                None,
            )?;
            self.last_progress = pos;
        }
        Ok(())
    }
    fn freeze(&self, scanner: &Scanner<'_>) -> Result<SparseIndex, String> {
        let frame = scanner.stack.first().ok_or("No progressive root frame")?;
        let mut record = frame.record;
        record[3] = scanner.reader.pos;
        let mut store = scanner.store.fork(self.budget, scanner.cancelled)?;
        store.put(frame.offset, record, scanner.cancelled)?;
        let root = Node {
            session: self.session,
            start: record[2],
            end: record[3],
            kind: Kind::decode(record[1])?,
            container: frame.offset,
            count: record[4],
        };
        let header = [
            2,
            2,
            self.source.len,
            root.kind.code(),
            root.start,
            root.end,
            root.container,
            root.count,
        ];
        store.commit(header, scanner.cancelled)?;
        let state = self.budget.reserve(
            Resource::Memory,
            64 * 1024,
            "immutable interactive session metadata",
        )?;
        Ok(SparseIndex {
            snapshot: self.source.clone(),
            store,
            budget: self.budget.clone(),
            root,
            header,
            _state: state,
            _cache: None,
            complete: false,
        })
    }
    pub(super) fn restored_page(&mut self, scanner: &mut Scanner<'_>) -> Result<(), String> {
        let Some(frame) = scanner.stack.first() else {
            return Ok(());
        };
        if Kind::decode(frame.record[1])? != Kind::Array {
            return Ok(());
        }
        if self.live {
            self.pending_view = Some(Box::new(self.freeze(scanner)?));
        }
        if frame.record[4] == 0 {
            return Ok(());
        }
        let count = frame.record[4].min(64);
        let mut reader = Reader::new(scanner.snapshot)?.with_cancellation(scanner.cancelled);
        reader.pos = frame.record[2] + 1;
        let mut nodes = Vec::with_capacity(64);
        for ordinal in 0..count {
            whitespace(&mut reader, scanner.cancelled)?;
            let node = Scanner::new(
                scanner.snapshot,
                scanner.store,
                self.budget,
                self.session,
                scanner.cancelled,
                false,
            )?
            .run(reader.pos)?;
            reader.pos = node.end;
            nodes.push(node);
            if ordinal + 1 < count {
                whitespace(&mut reader, scanner.cancelled)?;
                reader.expect(b',')?;
            }
        }
        let page = published(
            scanner.snapshot,
            &nodes,
            None,
            Some(count),
            self.budget,
            scanner.cancelled,
        )?;
        self.event(
            scanner.snapshot,
            SessionStatus::Validating,
            scanner.reader.pos,
            Some(Kind::Array),
            Some(page),
            None,
        )
    }
}
pub(super) fn published(
    snapshot: &Snapshot,
    nodes: &[Node],
    total: Option<u64>,
    next: Option<u64>,
    budget: &BudgetManager,
    cancelled: &dyn Fn() -> bool,
) -> Result<PublishedPage, String> {
    if nodes.len() > 64 {
        return Err("Published page exceeds 64 nodes".into());
    }
    let lease = budget.reserve(
        Resource::Memory,
        64 * (std::mem::size_of::<Node>() + 2 * std::mem::size_of::<String>() + 2048),
        "published sparse page",
    )?;
    let mut previews = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut bytes = [0; 160];
        let len = (node.end - node.start).min(160) as usize;
        snapshot
            .read_cancellable(node.start, &mut bytes[..len], cancelled)
            .map_err(|e| e.to_string())?;
        let mut text = String::from_utf8_lossy(&bytes[..len]).into_owned();
        if node.end - node.start > len as u64 {
            text.push('…');
        }
        previews.push(text);
    }
    Ok(PublishedPage {
        nodes: nodes.to_vec(),
        labels: nodes.iter().map(|n| format!("@{}", n.start)).collect(),
        previews,
        total,
        next,
        _lease: lease,
    })
}
impl SparseIndex {
    pub fn copy_value(
        &mut self,
        node: Node,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<CopiedValue, String> {
        self.validate(node, cancelled)?;
        let len = usize::try_from(node.end - node.start).map_err(|_| "Copy size overflow")?;
        if len > crate::export::COPY_LIMIT {
            return Err("Value exceeds 8 MiB clipboard limit; use Export".into());
        }
        let lease = self
            .budget
            .reserve(Resource::Memory, len, "sparse clipboard value")?;
        let mut bytes = Vec::with_capacity(len);
        self.write_raw(node, &mut bytes, cancelled)?;
        Ok(CopiedValue {
            text: String::from_utf8(bytes).map_err(|e| e.to_string())?,
            _lease: lease,
        })
    }
    pub fn export_value(
        &mut self,
        node: Node,
        path: &Path,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<(), String> {
        self.validate(node, &|| {
            cancelled.load(std::sync::atomic::Ordering::Relaxed)
        })?;
        let budget = self.budget.clone();
        crate::export::atomic_write_with_budget(path, cancelled, &budget, |out| {
            self.write_raw(node, out, &|| {
                cancelled.load(std::sync::atomic::Ordering::Relaxed)
            })
            .map_err(|e| serde_json::Error::io(std::io::Error::other(e)))
        })
    }
    pub fn published_children(
        &mut self,
        parent: Node,
        start: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PublishedPage, String> {
        let page = self.children(parent, start, 64, cancelled)?;
        let mut published = published(
            &self.snapshot,
            page.nodes(),
            Some(page.total),
            page.next,
            &self.budget,
            cancelled,
        )?;
        if parent.kind == Kind::Array {
            for (i, label) in published.labels.iter_mut().enumerate() {
                *label = format!("[{}]", start + i as u64);
            }
        } else if parent.kind == Kind::Object {
            for (i, label) in published.labels.iter_mut().enumerate() {
                let (mut text, truncated) =
                    self.key_at(parent, start + i as u64, 640, cancelled)?;
                if truncated {
                    text.push('…');
                }
                *label = text;
            }
        }
        Ok(published)
    }
    pub fn published_node(
        &mut self,
        node: Node,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PublishedPage, String> {
        self.validate(node, cancelled)?;
        published(
            &self.snapshot,
            &[node],
            Some(node.count),
            None,
            &self.budget,
            cancelled,
        )
    }
}

/// Events are synchronous, so a bounded/cancellable consumer controls backpressure.
/// Ready is emitted only after trailing whitespace, persistent commit and validation.
pub fn open_progressive(
    path: &Path,
    budget: BudgetManager,
    options: OpenOptions,
    generation: u64,
    cancelled: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Event) -> Result<(), String>,
) -> Result<SparseIndex, String> {
    let session = SessionId::fresh();
    emit(Event {
        session,
        generation,
        commit_sequence: 0,
        committed_frontier: 0,
        status: SessionStatus::Preparing,
        scanned: 0,
        total_bytes: 0,
        root_kind: None,
        page: None,
        reused: false,
        resumed: false,
        message: None,
        view: None,
    })?;
    let result = build(path, budget, options, session, generation, cancelled, emit);
    if let Err(error) = &result {
        let _ = emit(Event {
            session,
            generation,
            commit_sequence: 0,
            committed_frontier: 0,
            status: if cancelled() {
                SessionStatus::Cancelled
            } else {
                SessionStatus::Failed
            },
            scanned: 0,
            total_bytes: 0,
            root_kind: None,
            page: None,
            reused: false,
            resumed: false,
            message: Some(error.clone()),
            view: None,
        });
    }
    result
}
fn build(
    path: &Path,
    budget: BudgetManager,
    options: OpenOptions,
    session: SessionId,
    generation: u64,
    cancelled: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Event) -> Result<(), String>,
) -> Result<SparseIndex, String> {
    let state = budget.reserve(
        Resource::Memory,
        64 * 1024,
        "sparse persistent session state",
    )?;
    let mut cache = options
        .cache_dir
        .as_ref()
        .map(|dir| cache::Cache::open(dir, &budget))
        .transpose()?;
    let mut checkpoint = None;
    let snapshot = if let Some(cache) = &mut cache {
        if cache.matches(path, cancelled)? {
            checkpoint = cache.load(cancelled)?;
        }
        if checkpoint.is_some() {
            Snapshot::cached_with_disk(&cache.source_path(), &budget, cache.detach_source()?)?
        } else {
            cache.reset()?;
            let temp = Snapshot::copy(path, &|| cancelled(), &LoadProgress::default(), &budget)?;
            cache.save_source(&temp, cancelled)?;
            drop(temp);
            Snapshot::cached_with_disk(&cache.source_path(), &budget, cache.detach_source()?)?
        }
    } else {
        Snapshot::copy(path, &|| cancelled(), &LoadProgress::default(), &budget)?
    };
    let snapshot = std::sync::Arc::new(snapshot);
    let mut store = Store::new(&budget, cancelled)?;
    if let Some(cp) = &checkpoint
        && (cp.source_len != snapshot.len
            || cache
                .as_ref()
                .unwrap()
                .restore(&mut store, cp, cancelled)
                .is_err())
    {
        check(cancelled)?;
        // Source remains immutable; only the unusable index state is discarded.
        checkpoint = None;
        store = Store::new(&budget, cancelled)?;
    }
    let reused = checkpoint.as_ref().is_some_and(|cp| cp.root.is_some());
    let resumed = checkpoint.as_ref().is_some_and(|cp| cp.root.is_none());
    let mut publisher = Publisher {
        cache: cache.as_mut(),
        budget: &budget,
        emit,
        session,
        generation,
        last_commit: 0,
        last_progress: 0,
        first: Vec::with_capacity(64),
        resumed,
        reused,
        source: &snapshot,
        live: options.live_views,
        pending_view: None,
    };
    let root = if let Some(cp) = checkpoint.as_mut().filter(|cp| cp.root.is_some()) {
        let mut root = cp.root.unwrap();
        root.session = session;
        root
    } else {
        let mut scanner = Scanner::new(&snapshot, &mut store, &budget, session, cancelled, true)?;
        scanner.progress = options.progress.as_deref();
        if let Some(cp) = checkpoint {
            scanner.stack = cp.stack;
            scanner.reader.pos = cp.position;
            publisher.last_commit = cp.position;
            publisher.last_progress = cp.position;
            publisher.event(
                &snapshot,
                SessionStatus::Validating,
                cp.position,
                scanner
                    .stack
                    .first()
                    .map(|f| Kind::decode(f.record[1]))
                    .transpose()?,
                None,
                None,
            )?;
            publisher.restored_page(&mut scanner)?;
            scanner.run_progressive(&mut publisher, true)?
        } else {
            scanner.run_progressive(&mut publisher, false)?
        }
    };
    let mut tail = Reader::new(&snapshot)?.with_cancellation(cancelled);
    tail.pos = root.end;
    whitespace(&mut tail, cancelled)?;
    if tail.peek()?.is_some() {
        return Err(tail.error("trailing characters"));
    }
    let header = [
        2,
        2,
        snapshot.len,
        root.kind.code(),
        root.start,
        root.end,
        root.container,
        root.count,
    ];
    if reused {
        store.verify(header, cancelled)?;
    } else {
        store.commit(header, cancelled)?;
        if let Some(cache) = publisher.cache.as_mut() {
            let index_len = store.bytes();
            cache.commit(
                &mut store,
                &Checkpoint {
                    source_len: snapshot.len,
                    index_len,
                    position: tail.pos,
                    stack: Vec::new(),
                    root: Some(root),
                },
                cancelled,
            )?;
        }
    }
    drop(tail);
    let mut index = SparseIndex {
        snapshot: snapshot.clone(),
        store,
        budget: budget.clone(),
        root,
        header,
        _state: state,
        _cache: None,
        complete: true,
    };
    let page = if root.kind.container() {
        index.published_children(root, 0, cancelled)?
    } else {
        index.published_node(root, cancelled)?
    };
    publisher.last_commit = index.snapshot.len;
    publisher.event(
        &index.snapshot,
        SessionStatus::Ready,
        index.snapshot.len,
        Some(root.kind),
        Some(page),
        None,
    )?;
    index._cache = cache;
    Ok(index)
}
