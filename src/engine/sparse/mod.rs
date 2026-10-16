//! Experimental S3 scanner. Only containers/checkpoints/fields live on disk;
//! scalar array elements have no permanent record. No GUI/default-backend integration.
mod cache;
mod directory;
mod navigation;
mod search;
pub use navigation::RawSelection;
mod progressive;
pub(crate) mod store;
use crate::{
    document::{
        LoadProgress,
        indexed::{Reader, Snapshot},
    },
    engine::{
        budget::{BudgetManager, Reservation, Resource},
        file_source::with_io_slot,
        session::SessionId,
    },
};
pub use progressive::{CopiedValue, Event, OpenOptions, PublishedPage, open_progressive};
use std::{io::Write, path::Path};
use store::{Record, Store};
const NONE: u64 = u64::MAX;
const DEPTH: usize = 127;
const STRIDE: u64 = 1024;
const BYTE_STRIDE: u64 = 1024 * 1024;
const HASH_INIT: u64 = 0xcbf29ce484222325;
fn hash_update(hash: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(hash, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}
fn hash_bytes(bytes: &[u8]) -> u64 {
    hash_update(HASH_INIT, bytes)
}
fn directory_hash(parent: u64, hash: u64) -> u64 {
    hash_update(hash, &parent.to_le_bytes())
}
fn container_hash(start: u64) -> u64 {
    hash_update(HASH_INIT ^ 1, &start.to_le_bytes())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}
impl Kind {
    fn code(self) -> u64 {
        self as u64
    }
    fn decode(code: u64) -> Result<Self, String> {
        match code {
            0 => Ok(Self::Object),
            1 => Ok(Self::Array),
            2 => Ok(Self::String),
            3 => Ok(Self::Number),
            4 => Ok(Self::Boolean),
            5 => Ok(Self::Null),
            _ => Err("Invalid sparse kind".into()),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Null => "null",
        }
    }
    fn container(self) -> bool {
        matches!(self, Self::Object | Self::Array)
    }
}
/// References cannot be fabricated and are tied to the private immutable snapshot.
#[derive(Clone, Copy, Debug)]
pub struct Node {
    session: SessionId,
    start: u64,
    end: u64,
    kind: Kind,
    container: u64,
    count: u64,
}
impl Node {
    pub(crate) fn session(self) -> SessionId {
        self.session
    }
    pub(crate) fn words(self) -> [u64; 5] {
        [
            self.end,
            self.kind.code(),
            self.container,
            self.count,
            self.start,
        ]
    }
    pub(crate) fn restore(session: SessionId, w: [u64; 5]) -> Result<Self, String> {
        if w[4] > w[0] {
            return Err("Invalid saved node range".into());
        }
        Ok(Self {
            session,
            start: w[4],
            end: w[0],
            kind: Kind::decode(w[1])?,
            container: w[2],
            count: w[3],
        })
    }
    pub fn start(self) -> u64 {
        self.start
    }
    pub fn end(self) -> u64 {
        self.end
    }
    pub fn kind(self) -> Kind {
        self.kind
    }
    pub fn child_count(self) -> u64 {
        self.count
    }
}
/// Bounded page allocation stays charged while the caller owns it.
pub struct Page {
    nodes: Vec<Node>,
    pub next: Option<u64>,
    pub total: u64,
    _lease: Reservation,
}

impl Page {
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
}

pub struct SparseIndex {
    snapshot: std::sync::Arc<Snapshot>,
    store: Store,
    budget: BudgetManager,
    root: Node,
    header: [u64; 8],
    _state: Reservation,
    _cache: Option<cache::Cache>,
    complete: bool,
}
impl SparseIndex {
    pub fn open(
        path: &Path,
        budget: BudgetManager,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, String> {
        let state = budget.reserve(
            Resource::Memory,
            std::mem::size_of::<Self>() + 1024,
            "sparse session metadata",
        )?;
        let snapshot = std::sync::Arc::new(Snapshot::copy(
            path,
            &|| cancelled(),
            &LoadProgress::default(),
            &budget,
        )?);
        let mut store = Store::new(&budget, cancelled)?;
        let session = SessionId::fresh();
        let root =
            Scanner::new(&snapshot, &mut store, &budget, session, cancelled, true)?.run(0)?;
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
        store.commit(header, cancelled)?;
        Ok(Self {
            snapshot,
            store,
            budget,
            root,
            header,
            _state: state,
            _cache: None,
            complete: true,
        })
    }
    pub fn root(&self) -> Node {
        self.root
    }
    pub fn index_bytes(&self) -> u64 {
        self.store.bytes()
    }
    pub fn source_bytes(&self) -> u64 {
        self.snapshot.len
    }
    pub(crate) fn validate(&self, node: Node, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        if node.session != self.root.session {
            return Err("Node belongs to a different sparse session".into());
        }
        if node.start >= node.end || node.end > self.snapshot.len {
            return Err("Sparse source range out of bounds".into());
        }
        self.store.verify(self.header, cancelled)?;
        if node.kind.container() {
            let record = self.store.get(node.container, cancelled)?;
            if record[0] != 1
                || record[1] != node.kind.code()
                || record[2] != node.start
                || record[3] != node.end
                || record[4] != node.count
            {
                return Err("Sparse container identity mismatch".into());
            }
        }
        Ok(())
    }
    fn resolve(&mut self, mut node: Node, cancelled: &dyn Fn() -> bool) -> Result<Node, String> {
        if node.kind.container() {
            let mut cursor = self.store.head(container_hash(node.start), cancelled)?;
            let mut remaining = self.store.bytes() / 104;
            while cursor != NONE {
                if remaining == 0 {
                    return Err("Sparse directory cycle".into());
                }
                remaining -= 1;
                let r = self.store.get(cursor, cancelled)?;
                if r[0] == 1 && r[2] == node.start {
                    if r[3] != node.end || r[1] != node.kind.code() {
                        return Err("Sparse container range mismatch".into());
                    }
                    node.container = cursor;
                    node.count = r[4];
                    return Ok(node);
                }
                cursor = r[7];
            }
            return Err("Sparse container directory missing".into());
        }
        Ok(node)
    }
    fn field_node(&mut self, r: Record, cancelled: &dyn Fn() -> bool) -> Result<Node, String> {
        if r[0] != 3 {
            return Err("Invalid sparse field record".into());
        }
        self.resolve(
            Node {
                session: self.root.session,
                start: r[2],
                end: r[3],
                kind: Kind::decode(r[1])?,
                container: r[4],
                count: 0,
            },
            cancelled,
        )
    }
    /// Decoded UTF-8 key lookup; equivalent escaped keys share a disk entry.
    pub fn field(
        &mut self,
        parent: Node,
        key: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Node, String> {
        self.validate(parent, cancelled)?;
        if parent.kind != Kind::Object {
            return Err("Field lookup requires an object".into());
        }
        if key.len() > 16 * 1024 {
            return Err("Sparse lookup key exceeds 16 KiB command budget".into());
        }
        let hash = hash_bytes(key.as_bytes());
        let mut cursor = self
            .store
            .head(directory_hash(parent.container, hash), cancelled)?;
        let mut remaining = self.store.bytes() / 104;
        while cursor != NONE {
            if remaining == 0 {
                return Err("Sparse directory cycle".into());
            }
            remaining -= 1;
            let r = self.store.get(cursor, cancelled)?;
            if r[0] == 3
                && r[8] == parent.container
                && r[11] == hash
                && key_matches(&self.snapshot, r[9], r[10], key, cancelled)?
            {
                return self.field_node(r, cancelled);
            }
            cursor = r[7];
        }
        Err("Object key not found".into())
    }
    pub fn child(
        &mut self,
        parent: Node,
        ordinal: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Node, String> {
        let page = self.children(parent, ordinal, 1, cancelled)?;
        page.nodes
            .first()
            .copied()
            .ok_or_else(|| "Child index out of range".into())
    }
    /// Pages are limited to 256 entries; arrays resume at the nearest preceding checkpoint.
    pub fn children(
        &mut self,
        parent: Node,
        ordinal: u64,
        limit: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Page, String> {
        self.validate(parent, cancelled)?;
        if !parent.kind.container() {
            return Err("Children require a container".into());
        }
        if limit == 0 || limit > 256 {
            return Err("Sparse page limit must be 1..=256".into());
        }
        if ordinal > parent.count {
            return Err("Child cursor out of range".into());
        }
        let count = (parent.count - ordinal).min(limit as u64) as usize;
        let lease = self.budget.reserve(
            Resource::Memory,
            count * std::mem::size_of::<Node>(),
            "sparse child page",
        )?;
        let mut nodes = Vec::new();
        nodes.try_reserve_exact(count).map_err(|e| e.to_string())?;
        let mut lease = lease;
        lease.resize(nodes.capacity() * std::mem::size_of::<Node>())?;
        if count != 0 && parent.kind == Kind::Object {
            for index in ordinal..ordinal + count as u64 {
                let r = self.object_record(parent, index, cancelled)?;
                nodes.push(self.field_node(r, cancelled)?);
            }
        } else if count != 0 {
            let (mut position, mut index, _) =
                self.array_boundary(parent, ordinal, false, cancelled)?;
            let mut scan = Scanner::new(
                &self.snapshot,
                &mut self.store,
                &self.budget,
                self.root.session,
                cancelled,
                false,
            )?;
            while index < ordinal + count as u64 {
                let node = scan.indexed_node(position)?;
                if index >= ordinal {
                    nodes.push(node);
                }
                position = scan.next_array_boundary(node, parent, index)?;
                index += 1;
            }
        }
        let end = ordinal + count as u64;
        Ok(Page {
            nodes,
            next: (end < parent.count).then_some(end),
            total: parent.count,
            _lease: lease,
        })
    }
    /// Compatibility entry for the existing single-node Pointer subset.
    pub fn pointer(&mut self, pointer: &str, cancelled: &dyn Fn() -> bool) -> Result<Node, String> {
        crate::engine::query::QueryEngine::sparse_pointer(self, pointer, cancelled)
    }
    /// Streams original validated bytes, including duplicate keys in container ranges.
    /// It is not the legacy canonical/pretty export API.
    pub fn write_raw(
        &mut self,
        node: Node,
        output: &mut dyn Write,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        if !self.complete {
            return Err("Pending: complete document validation is required for output".into());
        }
        self.validate(node, cancelled)?;
        let _lease =
            self.budget
                .reserve(Resource::Memory, 64 * 1024, "sparse raw output buffer")?;
        let mut bytes = vec![0; 64 * 1024];
        let mut pos = node.start;
        while pos < node.end {
            if cancelled() {
                return Err("Sparse output cancelled".into());
            }
            let size = (node.end - pos).min(bytes.len() as u64) as usize;
            // Carry cancellation into the S2 positioned I/O admission wait as well.
            self.snapshot
                .read_cancellable(pos, &mut bytes[..size], cancelled)
                .map_err(|e| e.to_string())?;
            with_io_slot(cancelled, || output.write_all(&bytes[..size]))
                .map_err(|e| e.to_string())?;
            pos += size as u64;
        }
        Ok(())
    }
}

fn check(cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err("Sparse operation cancelled".into())
    } else {
        Ok(())
    }
}
fn whitespace(reader: &mut Reader<'_>, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    while matches!(reader.peek()?, Some(b' ' | b'\t' | b'\r' | b'\n')) {
        reader.next()?;
        if reader.pos.is_multiple_of(4096) {
            check(cancelled)?;
        }
    }
    Ok(())
}
fn key_matches(
    snapshot: &Snapshot,
    start: u64,
    end: u64,
    key: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool, String> {
    let mut reader = Reader::new(snapshot)?.with_cancellation(cancelled);
    reader.pos = start;
    reader.expect(b'"')?;
    for expected in key.chars() {
        check(cancelled)?;
        if reader.character()? != Some(expected) {
            return Ok(false);
        }
    }
    Ok(reader.character()?.is_none() && reader.pos == end)
}
fn keys_equal(
    snapshot: &Snapshot,
    left: u64,
    left_end: u64,
    right: u64,
    right_end: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool, String> {
    let mut a = Reader::new(snapshot)?.with_cancellation(cancelled);
    a.pos = left;
    a.expect(b'"')?;
    let mut b = Reader::new(snapshot)?.with_cancellation(cancelled);
    b.pos = right;
    b.expect(b'"')?;
    loop {
        check(cancelled)?;
        let ca = a.character()?;
        let cb = b.character()?;
        if ca != cb {
            return Ok(false);
        }
        if ca.is_none() {
            return Ok(a.pos == left_end && b.pos == right_end);
        }
    }
}
#[derive(Clone)]
struct Frame {
    record: Record,
    offset: u64,
    phase: u8,
    key: [u64; 3],
    checkpoint_at: u64,
    checkpoint_count: u64,
}
struct Scanner<'a> {
    reader: Reader<'a>,
    snapshot: &'a Snapshot,
    store: &'a mut Store,
    session: SessionId,
    cancelled: &'a dyn Fn() -> bool,
    index: bool,
    stack: Vec<Frame>,
    _stack: Reservation,
    progress: Option<&'a std::sync::atomic::AtomicU64>,
}
impl<'a> Scanner<'a> {
    fn new(
        snapshot: &'a Snapshot,
        store: &'a mut Store,
        budget: &BudgetManager,
        session: SessionId,
        cancelled: &'a dyn Fn() -> bool,
        index: bool,
    ) -> Result<Self, String> {
        let mut lease = budget.reserve(
            Resource::Memory,
            DEPTH * std::mem::size_of::<Frame>(),
            "sparse explicit parser stack",
        )?;
        let mut stack = Vec::new();
        stack.try_reserve_exact(DEPTH).map_err(|e| e.to_string())?;
        lease.resize(stack.capacity() * std::mem::size_of::<Frame>())?;
        Ok(Self {
            reader: Reader::new(snapshot)?.with_cancellation(cancelled),
            snapshot,
            store,
            session,
            cancelled,
            index,
            stack,
            _stack: lease,
            progress: None,
        })
    }
    fn string(&mut self) -> Result<u64, String> {
        self.reader.expect(b'"')?;
        let mut hash = HASH_INIT;
        let mut boundary = self.reader.pos / 4096;
        while let Some(c) = self.reader.character()? {
            let mut bytes = [0; 4];
            hash = hash_update(hash, c.encode_utf8(&mut bytes).as_bytes());
            if self.reader.pos / 4096 != boundary {
                boundary = self.reader.pos / 4096;
                if let Some(progress) = self.progress {
                    progress.store(self.reader.pos, std::sync::atomic::Ordering::Relaxed);
                }
                check(self.cancelled)?;
            }
        }
        Ok(hash)
    }
    fn digits(&mut self) -> Result<(), String> {
        while matches!(self.reader.peek()?, Some(b'0'..=b'9')) {
            self.reader.next()?;
            if self.reader.pos.is_multiple_of(4096) {
                if let Some(progress) = self.progress {
                    progress.store(self.reader.pos, std::sync::atomic::Ordering::Relaxed);
                }
                check(self.cancelled)?;
            }
        }
        Ok(())
    }
    fn number(&mut self) -> Result<(), String> {
        if self.reader.peek()? == Some(b'-') {
            self.reader.next()?;
        }
        match self.reader.next()? {
            b'0' => (),
            b'1'..=b'9' => self.digits()?,
            _ => return Err(self.reader.error("invalid number")),
        }
        if self.reader.peek()? == Some(b'.') {
            self.reader.next()?;
            if !matches!(self.reader.peek()?, Some(b'0'..=b'9')) {
                return Err(self.reader.error("missing fractional digits"));
            }
            self.digits()?;
        }
        if matches!(self.reader.peek()?, Some(b'e' | b'E')) {
            self.reader.next()?;
            if matches!(self.reader.peek()?, Some(b'+' | b'-')) {
                self.reader.next()?;
            }
            if !matches!(self.reader.peek()?, Some(b'0'..=b'9')) {
                return Err(self.reader.error("missing exponent digits"));
            }
            self.digits()?;
        }
        Ok(())
    }
    fn begin(&mut self) -> Result<Option<Node>, String> {
        check(self.cancelled)?;
        whitespace(&mut self.reader, self.cancelled)?;
        let start = self.reader.pos;
        let kind = match self.reader.peek()? {
            Some(b'{') => Kind::Object,
            Some(b'[') => Kind::Array,
            Some(b'"') => Kind::String,
            Some(b'-' | b'0'..=b'9') => Kind::Number,
            Some(b't' | b'f') => Kind::Boolean,
            Some(b'n') => Kind::Null,
            _ => return Err(self.reader.error("expected JSON value")),
        };
        if kind.container() {
            if self.stack.len() >= DEPTH {
                return Err("Sparse depth resource limit exceeded (127 containers)".into());
            }
            self.reader.next()?;
            let mut record = [0; 12];
            record[0] = 1;
            record[1] = kind.code();
            record[2] = start;
            record[3] = NONE;
            record[5] = NONE;
            record[6] = NONE;
            record[7] = NONE;
            record[8] = self.stack.last().map_or(NONE, |f| f.offset);
            record[9] = 1;
            record[10] = NONE;
            record[11] = self.stack.last().map_or(0, |f| f.record[4]);
            let offset = if self.index {
                let hash = container_hash(start);
                record[7] = self.store.head(hash, self.cancelled)?;
                let offset = self.store.append(record, self.cancelled)?;
                self.store.set_head(hash, offset, self.cancelled)?;
                offset
            } else {
                NONE
            };
            self.stack.push(Frame {
                record,
                offset,
                phase: 0,
                key: [0; 3],
                checkpoint_at: start,
                checkpoint_count: 0,
            });
            return Ok(None);
        }
        match kind {
            Kind::String => {
                self.string()?;
            }
            Kind::Number => self.number()?,
            Kind::Boolean | Kind::Null => {
                let expected: &[u8] = match self.reader.peek()? {
                    Some(b't') => b"true",
                    Some(b'f') => b"false",
                    _ => b"null",
                };
                for &byte in expected {
                    self.reader.expect(byte)?;
                }
            }
            _ => unreachable!(),
        }
        Ok(Some(Node {
            session: self.session,
            start,
            end: self.reader.pos,
            kind,
            container: NONE,
            count: 0,
        }))
    }
    fn attach(&mut self, node: Node) -> Result<(), String> {
        let weight = if self.index && node.kind.container() {
            self.store.get(node.container, self.cancelled)?[9]
        } else {
            1
        };
        let frame = self.stack.last_mut().unwrap();
        if frame.record[1] == Kind::Object.code() && self.index {
            let [key_start, key_end, hash] = frame.key;
            let bucket = directory_hash(frame.offset, hash);
            let head = self.store.head(bucket, self.cancelled)?;
            let mut cursor = head;
            let mut remaining = self.store.bytes() / 104;
            while cursor != NONE {
                if remaining == 0 {
                    return Err("Sparse directory cycle".into());
                }
                remaining -= 1;
                let mut r = self.store.get(cursor, self.cancelled)?;
                if r[0] == 3
                    && r[8] == frame.offset
                    && r[11] == hash
                    && keys_equal(
                        self.snapshot,
                        r[9],
                        r[10],
                        key_start,
                        key_end,
                        self.cancelled,
                    )?
                {
                    let previous = self
                        .store
                        .directory_find(frame.record[10], r[6], false, self.cancelled)?
                        .ok_or("Missing object ordinal")?;
                    if previous[1] != r[6] {
                        return Err("Object ordinal mismatch".into());
                    }
                    frame.record[9] = frame.record[9]
                        .checked_sub(previous[7])
                        .and_then(|n| n.checked_add(weight))
                        .ok_or("Subtree count overflow")?;
                    frame.record[10] = self.store.directory_put(
                        frame.record[10],
                        [r[6], cursor, weight, 0, 0],
                        self.cancelled,
                    )?;
                    if node.kind.container() {
                        let mut child = self.store.get(node.container, self.cancelled)?;
                        child[11] = r[6];
                        self.store.put(node.container, child, self.cancelled)?;
                    }
                    r[1] = node.kind.code();
                    r[2] = node.start;
                    r[3] = node.end;
                    r[4] = node.container;
                    self.store.put(cursor, r, self.cancelled)?;
                    frame.phase = 2;
                    return Ok(());
                }
                cursor = r[7];
            }
            let mut r = [0; 12];
            r[0] = 3;
            r[1] = node.kind.code();
            r[2] = node.start;
            r[3] = node.end;
            r[4] = node.container;
            r[5] = NONE;
            r[6] = frame.record[4];
            r[7] = head;
            r[8] = frame.offset;
            r[9] = key_start;
            r[10] = key_end;
            r[11] = hash;
            let offset = self.store.append(r, self.cancelled)?;
            self.store.set_head(bucket, offset, self.cancelled)?;
            if frame.record[6] == NONE {
                frame.record[5] = offset;
            } else {
                let mut last = self.store.get(frame.record[6], self.cancelled)?;
                last[5] = offset;
                self.store.put(frame.record[6], last, self.cancelled)?;
            }
            frame.record[6] = offset;
            frame.record[10] = self.store.directory_put(
                frame.record[10],
                [r[6], offset, weight, 0, 0],
                self.cancelled,
            )?;
        }
        frame.record[9] = frame.record[9]
            .checked_add(weight)
            .ok_or("Subtree count overflow")?;
        frame.record[4] = frame.record[4]
            .checked_add(1)
            .ok_or("Sparse child count overflow")?;
        frame.phase = 2;
        Ok(())
    }
    fn run(self, position: u64) -> Result<Node, String> {
        self.run_internal(position, None, false)
    }
    fn run_progressive(
        self,
        publisher: &mut progressive::Publisher<'_>,
        resumed: bool,
    ) -> Result<Node, String> {
        let position = self.reader.pos;
        self.run_internal(position, Some(publisher), resumed)
    }
    fn run_internal(
        mut self,
        position: u64,
        mut publisher: Option<&mut progressive::Publisher<'_>>,
        resumed: bool,
    ) -> Result<Node, String> {
        self.reader.pos = position;
        if !resumed {
            whitespace(&mut self.reader, self.cancelled)?;
            let kind = match self.reader.peek()? {
                Some(b'{') => Some(Kind::Object),
                Some(b'[') => Some(Kind::Array),
                Some(b'"') => Some(Kind::String),
                Some(b'-' | b'0'..=b'9') => Some(Kind::Number),
                Some(b't' | b'f') => Some(Kind::Boolean),
                Some(b'n') => Some(Kind::Null),
                _ => None,
            };
            if let Some(p) = publisher.as_mut() {
                p.event(
                    self.snapshot,
                    crate::engine::session::SessionStatus::Validating,
                    position,
                    kind,
                    None,
                    None,
                )?;
            }
        }
        let mut completed = if resumed { None } else { self.begin()? };
        if !resumed
            && completed.is_none()
            && let Some(p) = publisher.as_mut()
        {
            p.safe(&mut self, None, true)?;
        }
        loop {
            check(self.cancelled)?;
            if let Some(node) = completed.take() {
                if self.stack.is_empty() {
                    return Ok(node);
                }
                self.attach(node)?;
                if let Some(p) = publisher.as_mut() {
                    p.safe(&mut self, Some(node), false)?;
                }
            }
            whitespace(&mut self.reader, self.cancelled)?;
            let frame = self.stack.last().unwrap();
            let object = frame.record[1] == Kind::Object.code();
            let close = if object { b'}' } else { b']' };
            if self.reader.peek()? == Some(close) && frame.phase != 3 {
                self.reader.next()?;
                let mut frame = self.stack.pop().unwrap();
                frame.record[3] = self.reader.pos;
                if self.index {
                    self.store.put(frame.offset, frame.record, self.cancelled)?;
                }
                completed = Some(Node {
                    session: self.session,
                    start: frame.record[2],
                    end: frame.record[3],
                    kind: Kind::decode(frame.record[1])?,
                    container: frame.offset,
                    count: frame.record[4],
                });
                continue;
            }
            if frame.phase == 2 {
                self.reader.expect(b',')?;
                self.stack.last_mut().unwrap().phase = 3;
                continue;
            }
            if object {
                let start = self.reader.pos;
                let hash = self.string()?;
                let end = self.reader.pos;
                whitespace(&mut self.reader, self.cancelled)?;
                self.reader.expect(b':')?;
                self.stack.last_mut().unwrap().key = [start, end, hash];
            } else if self.index {
                let frame = self.stack.last_mut().unwrap();
                if frame.record[4] == 0
                    || frame.record[4] - frame.checkpoint_count >= STRIDE
                    || self.reader.pos - frame.checkpoint_at >= BYTE_STRIDE
                {
                    let mut r = [0; 12];
                    r[0] = 2;
                    r[1] = frame.record[9] - 1;
                    r[2] = self.reader.pos;
                    r[4] = frame.record[4];
                    r[5] = NONE;
                    r[8] = frame.offset;
                    let offset = self.store.append(r, self.cancelled)?;
                    frame.record[10] = self.store.directory_put(
                        frame.record[10],
                        [r[4], offset, 0, r[1], 0],
                        self.cancelled,
                    )?;
                    if frame.record[6] == NONE {
                        frame.record[5] = offset;
                    } else {
                        let mut last = self.store.get(frame.record[6], self.cancelled)?;
                        last[5] = offset;
                        self.store.put(frame.record[6], last, self.cancelled)?;
                    }
                    frame.record[6] = offset;
                    frame.checkpoint_at = self.reader.pos;
                    frame.checkpoint_count = frame.record[4];
                }
            }
            self.stack.last_mut().unwrap().phase = 1;
            completed = self.begin()?;
        }
    }
}

#[cfg(test)]
mod progressive_tests;
#[cfg(test)]
mod tests;
