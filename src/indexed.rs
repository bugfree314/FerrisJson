//! Validated file-range backend. No memory mapping and no whole-document Value.
use crate::engine::{
    budget::{BudgetManager, Reservation, Resource, reserve_vec},
    file_source::{FileSource, lock_cancellable, with_io_slot},
    session::SessionId,
};

use super::{Document, Kind, Node, NodeIds, Storage, compact, short};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
};

const BLOCK: usize = 64 * 1024;
const INDEX_BUDGET: usize = 1536 * 1024 * 1024;
const KEY_BUDGET: usize = 64 * 1024 * 1024;
const SUBTREE_LIMIT: usize = 256 * 1024;
const CACHE_BUDGET: usize = 8 * 1024 * 1024;
static SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
pub struct LoadProgress {
    done: AtomicU64,
    total: AtomicU64,
    phase: AtomicU8,
}
impl LoadProgress {
    pub fn status(&self) -> String {
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return "Opening indexed file...".into();
        }
        let phase = match self.phase.load(Ordering::Relaxed) {
            0 => "Snapshot",
            1 => "Validating/indexing",
            _ => "Finishing index",
        };
        if self.phase.load(Ordering::Relaxed) >= 2 {
            return "Finishing index...".into();
        }
        format!(
            "{phase}: {:.0}%",
            self.done.load(Ordering::Relaxed) as f64 * 100.0 / total as f64
        )
    }
}
pub(crate) struct Snapshot {
    file: FileSource,
    path: Option<PathBuf>,
    pub(crate) len: u64,
    _disk: Reservation,
    budget: BudgetManager,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        self.file.close();
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}
impl Snapshot {
    pub(crate) fn cached_with_disk(
        path: &Path,
        budget: &BudgetManager,
        disk: Reservation,
    ) -> Result<Self, String> {
        let mut snapshot = Self::cached(path, budget)?;
        snapshot._disk = disk;
        Ok(snapshot)
    }
    /// A cache snapshot is immutable while this handle is open. Its disk space
    /// is charged by the cache owner, independently of this reader's lifetime.
    pub(crate) fn cached(path: &Path, budget: &BudgetManager) -> Result<Self, String> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1);
        }
        let file = options.open(path).map_err(|e| e.to_string())?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        Ok(Self {
            file: FileSource::new(file),
            path: None,
            len,
            _disk: budget.reserve(Resource::Disk, 0, "cache snapshot reader")?,
            budget: budget.clone(),
        })
    }
    pub(crate) fn copy(
        path: &Path,
        cancelled: &impl Fn() -> bool,
        progress: &LoadProgress,
        budget: &BudgetManager,
    ) -> Result<Self, String> {
        let mut input_options = OpenOptions::new();
        input_options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            input_options.share_mode(1);
        }
        let mut input = input_options
            .open(path)
            .map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
        let before = input.metadata().map_err(|e| e.to_string())?;
        usize::try_from(before.len()).map_err(|_| "File size exceeds platform address range")?;
        progress.total.store(before.len(), Ordering::Relaxed);
        progress.done.store(0, Ordering::Relaxed);
        progress.phase.store(0, Ordering::Relaxed);
        let disk = budget.reserve(
            Resource::Disk,
            usize::try_from(before.len()).map_err(|_| "Snapshot size overflow")?,
            "file snapshot",
        )?;
        let mut disk = Some(disk);
        let _buffer = budget.reserve(Resource::Memory, BLOCK, "snapshot copy buffer")?;
        let mut result = None;
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "FerrisJson-snapshot-{}-{}.tmp",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(0);
                // DELETE_ON_CLOSE also cleans the snapshot if the process exits abruptly.
                options
                    .access_mode(0x8000_0000 | 0x4000_0000 | 0x0001_0000)
                    .custom_flags(0x0400_0000);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    result = Some(Self {
                        file: FileSource::new(file),
                        path: Some(path),
                        len: 0,
                        _disk: disk.take().unwrap(),
                        budget: budget.clone(),
                    });
                    #[cfg(unix)]
                    if let Some(snapshot) = &mut result
                        && fs::remove_file(snapshot.path.as_ref().unwrap()).is_ok()
                    {
                        snapshot.path = None;
                    }
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(format!("Cannot create snapshot: {e}")),
            }
        }
        let mut snapshot = result.ok_or("Cannot create snapshot")?;
        let mut buffer = vec![0; BLOCK];
        loop {
            if cancelled() {
                return Err("Loading cancelled".into());
            }
            let count =
                with_io_slot(cancelled, || input.read(&mut buffer)).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            snapshot._disk.resize(
                snapshot._disk.bytes().max(
                    usize::try_from(snapshot.len)
                        .map_err(|_| "Snapshot size overflow")?
                        .checked_add(count)
                        .ok_or("Snapshot size overflow")?,
                ),
            )?;
            with_io_slot(cancelled, || {
                snapshot.file.file_mut().write_all(&buffer[..count])
            })
            .map_err(|e| e.to_string())?;
            snapshot.len += count as u64;
            progress.done.store(snapshot.len, Ordering::Relaxed);
        }
        let after = input.metadata().map_err(|e| e.to_string())?;
        if before.len() != after.len()
            || snapshot.len != before.len()
            || before.modified().ok() != after.modified().ok()
        {
            return Err("Source changed while copying; reopen the file".into());
        }
        #[cfg(unix)]
        if let Some(path) = &snapshot.path
            && fs::remove_file(path).is_ok()
        {
            snapshot.path = None;
        }
        Ok(snapshot)
    }
    pub(crate) fn read(&self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
        self.check_range(offset, bytes.len())?;
        self.file.read_exact_at(offset, bytes)
    }
    pub(crate) fn read_cancellable(
        &self,
        offset: u64,
        bytes: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        self.check_range(offset, bytes.len())?;
        self.file
            .read_exact_at_cancellable(offset, bytes, cancelled)
    }
    fn check_range(&self, offset: u64, length: usize) -> io::Result<()> {
        if offset
            .checked_add(length as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err(io::Error::other("Snapshot range out of bounds"));
        }
        Ok(())
    }
}
pub(crate) struct Reader<'a> {
    cancelled: Option<&'a dyn Fn() -> bool>,
    source: &'a Snapshot,
    buffer: Vec<u8>,
    _buffer_reservation: Reservation,
    start: u64,
    count: usize,
    pub(crate) pos: u64,
    line: u64,
    column: u64,
}
impl<'a> Reader<'a> {
    pub(crate) fn new(source: &'a Snapshot) -> Result<Self, String> {
        let lease = source
            .budget
            .reserve(Resource::Memory, BLOCK, "reader buffer")?;
        Ok(Self {
            cancelled: None,
            source,
            _buffer_reservation: lease,
            buffer: vec![0; BLOCK],
            start: 0,
            count: 0,
            pos: 0,
            line: 1,
            column: 0,
        })
    }
    pub(crate) fn with_cancellation(mut self, cancelled: &'a dyn Fn() -> bool) -> Self {
        self.cancelled = Some(cancelled);
        self
    }
    pub(crate) fn error(&self, message: &str) -> String {
        format!(
            "Invalid JSON: {message} at line {} column {} (byte {})",
            self.line,
            self.column + 1,
            self.pos
        )
    }
    pub(crate) fn peek(&mut self) -> Result<Option<u8>, String> {
        if self.pos >= self.source.len {
            return Ok(None);
        }
        if self.pos < self.start || self.pos >= self.start + self.count as u64 {
            self.start = self.pos / BLOCK as u64 * BLOCK as u64;
            self.count = (self.source.len - self.start).min(BLOCK as u64) as usize;
            self.source
                .file
                .read_exact_at_cancellable(
                    self.start,
                    &mut self.buffer[..self.count],
                    self.cancelled.unwrap_or(&|| false),
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(Some(self.buffer[(self.pos - self.start) as usize]))
    }
    pub(crate) fn next(&mut self) -> Result<u8, String> {
        let byte = self
            .peek()?
            .ok_or_else(|| self.error("unexpected end of input"))?;
        self.pos += 1;
        if byte == b'\n' {
            self.line += 1;
            self.column = 0;
        } else {
            self.column += 1;
        }
        Ok(byte)
    }
    pub(crate) fn expect(&mut self, expected: u8) -> Result<(), String> {
        if self.peek()? != Some(expected) {
            return Err(self.error("unexpected token"));
        }
        self.next()?;
        Ok(())
    }
    fn hex(&mut self) -> Result<u16, String> {
        let mut value = 0;
        for _ in 0..4 {
            let byte = self.next()?;
            let digit = (byte as char)
                .to_digit(16)
                .ok_or_else(|| self.error("invalid Unicode escape"))?;
            value = value * 16 + digit as u16;
        }
        Ok(value)
    }
    // Reads one decoded character after an opening quote. None consumes the closing quote.
    pub(crate) fn character(&mut self) -> Result<Option<char>, String> {
        let byte = self.next()?;
        let character = match byte {
            b'"' => return Ok(None),
            0..=31 => return Err(self.error("unescaped control character")),
            b'\\' => match self.next()? {
                b'"' => '"',
                b'\\' => '\\',
                b'/' => '/',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'u' => {
                    let first = self.hex()?;
                    let code = if (0xd800..=0xdbff).contains(&first) {
                        self.expect(b'\\')?;
                        self.expect(b'u')?;
                        let second = self.hex()?;
                        if !(0xdc00..=0xdfff).contains(&second) {
                            return Err(self.error("invalid surrogate pair"));
                        }
                        0x10000 + ((first as u32 - 0xd800) << 10) + second as u32 - 0xdc00
                    } else {
                        first as u32
                    };
                    char::from_u32(code).ok_or_else(|| self.error("unpaired Unicode surrogate"))?
                }
                _ => return Err(self.error("invalid string escape")),
            },
            32..=127 => byte as char,
            _ => {
                let count = match byte {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Err(self.error("invalid UTF-8")),
                };
                let mut bytes = [0; 4];
                bytes[0] = byte;
                for byte in bytes.iter_mut().take(count).skip(1) {
                    *byte = self.next()?;
                }
                std::str::from_utf8(&bytes[..count])
                    .map_err(|_| self.error("invalid UTF-8"))?
                    .chars()
                    .next()
                    .unwrap()
            }
        };
        Ok(Some(character))
    }
}
#[derive(Clone, Copy)]
struct Range {
    start: u64,
    end: u64,
}
#[derive(Default)]
struct Cache {
    entries: HashMap<usize, (Arc<Subtree>, usize)>,
    order: VecDeque<usize>,
    bytes: usize,
}
pub struct Subtree {
    value: Value,
    _reservation: Reservation,
}
impl std::ops::Deref for Subtree {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.value
    }
}
impl std::fmt::Debug for Subtree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}
impl serde::Serialize for Subtree {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

pub(super) struct Store {
    source: Snapshot,
    ranges: Vec<Range>,
    cache: Mutex<Cache>,
}
pub(super) fn value_cost(value: &Value) -> usize {
    std::mem::size_of::<Value>()
        + match value {
            Value::String(text) => text.capacity(),
            Value::Number(number) => number.as_str().len(),
            Value::Array(values) => {
                values.capacity() * std::mem::size_of::<Value>()
                    + values.iter().map(value_cost).sum::<usize>()
            }
            Value::Object(values) => {
                values.len() * 2 * (std::mem::size_of::<(String, Value)>() + 32)
                    + values
                        .iter()
                        .map(|(key, value)| key.capacity() + value_cost(value))
                        .sum::<usize>()
            }
            _ => 0,
        }
}
impl Store {
    pub(super) fn range_bytes(&self) -> usize {
        self.ranges.capacity() * std::mem::size_of::<Range>()
    }
    pub(super) fn subtree(&self, doc: &Document, id: usize) -> Result<Arc<Subtree>, String> {
        let range = self.ranges.get(id).ok_or("Invalid node ID")?;
        if range.end - range.start > SUBTREE_LIMIT as u64 || doc.nodes[id].end() - id > 16_384 {
            return Err("Subtree exceeds the bounded parse limit (256 KiB / 16,384 nodes); use range preview or export".into());
        }
        {
            let mut cache = lock_cancellable(&self.cache).map_err(|e| e.to_string())?;
            if let Some((value, _)) = cache.entries.get(&id) {
                let value = Arc::clone(value);
                cache.order.retain(|&old| old != id);
                cache.order.push_back(id);
                return Ok(value);
            }
        }
        // Bound transient decode/serde work before reading. Retained payload is charged
        // separately and survives eviction while callers retain an Arc.
        let _workspace = doc.budget.reserve(
            Resource::Memory,
            SUBTREE_LIMIT * 32,
            "subtree decode workspace",
        )?;
        let mut bytes = vec![0; (range.end - range.start) as usize];
        self.source
            .read(range.start, &mut bytes)
            .map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let cost = value_cost(&value);
        let reservation = doc.budget.reserve(
            Resource::Memory,
            cost + 128,
            "subtree payload and cache metadata estimate",
        )?;
        let value = Arc::new(Subtree {
            value,
            _reservation: reservation,
        });
        let mut cache = lock_cancellable(&self.cache).map_err(|e| e.to_string())?;
        if let Some((old, _)) = cache.entries.get(&id) {
            return Ok(Arc::clone(old));
        }
        if cost <= CACHE_BUDGET {
            while cache.bytes + cost > CACHE_BUDGET || cache.entries.len() >= 512 {
                let old = cache.order.pop_front().unwrap();
                let (_, cost) = cache.entries.remove(&old).unwrap();
                cache.bytes -= cost;
            }
            cache.bytes += cost;
            cache.order.push_back(id);
            cache.entries.insert(id, (Arc::clone(&value), cost));
        }
        Ok(value)
    }
    pub(super) fn row_preview(&self, doc: &Document, id: usize) -> Result<String, String> {
        let node = &doc.nodes[id];
        match node.kind {
            Kind::Object => Ok(format!("{{ {} keys }}", node.child_count)),
            Kind::Array => Ok(format!("[ {} items ]", node.child_count)),
            Kind::String => {
                let mut reader = Reader::new(&self.source)?;
                reader.pos = self.ranges[id].start;
                reader.expect(b'"')?;
                let mut text = String::new();
                for _ in 0..160 {
                    match reader.character()? {
                        Some(character) => text.push(character),
                        None => return Ok(serde_json::to_string(&text).unwrap()),
                    }
                }
                if reader.character()?.is_some() {
                    text.push('…');
                }
                Ok(serde_json::to_string(&text).unwrap())
            }
            Kind::Number => {
                let range = self.ranges[id];
                if range.end - range.start <= SUBTREE_LIMIT as u64 {
                    return Ok(short(
                        self.subtree(doc, id)?.as_number().unwrap().as_str(),
                        160,
                    ));
                }
                let mut bytes = vec![0; 160];
                self.source
                    .read(range.start, &mut bytes)
                    .map_err(|e| e.to_string())?;
                Ok(format!("{}…", String::from_utf8(bytes).unwrap()))
            }
            Kind::Boolean | Kind::Null => Ok(self.subtree(doc, id)?.to_string()),
        }
    }
    pub(super) fn write(
        &self,
        doc: &Document,
        output: &mut dyn Write,
        ids: &[u32],
        array: bool,
    ) -> io::Result<()> {
        fn indent(output: &mut dyn Write, depth: usize) -> io::Result<()> {
            output.write_all(&[b' '; 512][..depth * 2])
        }
        fn value(
            store: &Store,
            doc: &Document,
            reader: &mut Reader<'_>,
            output: &mut dyn Write,
            id: usize,
            depth: usize,
        ) -> io::Result<()> {
            let node = &doc.nodes[id];
            if node.container() {
                let object = matches!(node.kind, Kind::Object);
                output.write_all(if object { b"{" } else { b"[" })?;
                for (ordinal, child) in doc.direct_children(id).enumerate() {
                    if ordinal != 0 {
                        output.write_all(b",")?;
                    }
                    output.write_all(b"\n")?;
                    indent(output, depth + 1)?;
                    if object {
                        serde_json::to_writer(&mut *output, doc.key(child).as_ref())
                            .map_err(io::Error::other)?;
                        output.write_all(b": ")?;
                    }
                    value(store, doc, reader, output, child, depth + 1)?;
                }
                if node.child_count != 0 {
                    output.write_all(b"\n")?;
                    indent(output, depth)?;
                }
                output.write_all(if object { b"}" } else { b"]" })
            } else {
                let range = store.ranges[id];
                reader.pos = range.start;
                while reader.pos < range.end {
                    reader.peek().map_err(io::Error::other)?;
                    let available =
                        ((reader.start + reader.count as u64).min(range.end) - reader.pos) as usize;
                    let start = (reader.pos - reader.start) as usize;
                    output.write_all(&reader.buffer[start..start + available])?;
                    reader.pos += available as u64;
                }
                Ok(())
            }
        }
        let mut reader = Reader::new(&self.source).map_err(io::Error::other)?;
        if array {
            output.write_all(b"[")?;
        }
        for (ordinal, &id) in ids.iter().enumerate() {
            if array {
                if ordinal != 0 {
                    output.write_all(b",")?;
                }
                output.write_all(b"\n  ")?;
            }
            value(
                self,
                doc,
                &mut reader,
                output,
                id as usize,
                usize::from(array),
            )?;
        }
        if array {
            if !ids.is_empty() {
                output.write_all(b"\n")?;
            }
            output.write_all(b"]")?;
        }
        Ok(())
    }
    pub(super) fn search_visit(
        &self,
        doc: &Document,
        matcher: &crate::engine::search::Matcher,
        cancel: &dyn Fn() -> bool,
        visit: &mut dyn FnMut(usize, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut reader = Reader::new(&self.source)?.with_cancellation(cancel);
        for (id, node) in doc.nodes.iter().enumerate() {
            if cancel() {
                return Err("Search cancelled".into());
            }
            let key = matcher.keys() && matcher.text(&doc.key(id));
            let value = if key || !matcher.values() {
                false
            } else {
                match node.kind {
                    Kind::Object | Kind::Array => false,
                    Kind::Boolean | Kind::Null | Kind::Number | Kind::String => matcher.range(
                        &mut reader,
                        self.ranges[id].start,
                        self.ranges[id].end,
                        matches!(node.kind, Kind::String),
                        cancel,
                    )?,
                }
            };
            visit(id, key || value)?;
        }
        Ok(())
    }
}

struct Parser<'a, F> {
    reader: Reader<'a>,
    cancelled: F,
    progress: &'a LoadProgress,
    nodes: Vec<Node>,
    ranges: Vec<Range>,
    children: Vec<u32>,
    keys: Vec<Arc<str>>,
    key_ids: HashMap<Arc<str>, u32>,
    key_buffer: String,
    key_bytes: usize,
    allocated: usize,
    duplicates: bool,
    budget: BudgetManager,
    resources: Vec<Reservation>,
    key_reservation: Reservation,
}
fn grow<T>(
    values: &mut Vec<T>,
    allocated: &mut usize,
    budget: &BudgetManager,
    resources: &mut Vec<Reservation>,
) -> Result<(), String> {
    if values.len() == values.capacity() {
        let extra = (values.capacity() / 2).clamp(1024, 1_048_576);
        let bytes = extra * std::mem::size_of::<T>();
        if allocated
            .checked_add(bytes)
            .is_none_or(|total| total > INDEX_BUDGET)
        {
            return Err("Indexed structural capacity exceeds 1.5 GiB budget".into());
        }
        let mut lease = budget.reserve(Resource::Memory, bytes, "indexed structure growth")?;
        let _overlap = budget.reserve(
            Resource::Memory,
            values.capacity() * std::mem::size_of::<T>(),
            "index realloc overlap",
        )?;
        let previous = values.capacity();
        values
            .try_reserve_exact(extra)
            .map_err(|e| format!("Cannot allocate index: {e}"))?;
        let actual = (values.capacity() - previous) * std::mem::size_of::<T>();
        lease.resize(actual)?;
        resources.push(lease);
        *allocated += actual;
    }
    Ok(())
}
impl<F: Fn() -> bool> Parser<'_, F> {
    fn check(&self) -> Result<(), String> {
        if (self.cancelled)() {
            Err("Loading cancelled".into())
        } else {
            Ok(())
        }
    }
    fn space(&mut self) -> Result<(), String> {
        while matches!(self.reader.peek()?, Some(b' ' | b'\r' | b'\n' | b'\t')) {
            self.reader.next()?;
            if self.reader.pos & 4095 == 0 {
                self.check()?;
                self.progress.done.store(self.reader.pos, Ordering::Relaxed);
            }
        }
        Ok(())
    }
    fn string(&mut self, key: bool) -> Result<(), String> {
        self.reader.expect(b'"')?;
        if key {
            self.key_buffer.clear();
        }
        while let Some(character) = self.reader.character()? {
            if self.reader.pos & 4095 < 4 {
                self.check()?;
                self.progress.done.store(self.reader.pos, Ordering::Relaxed);
            }
            if key {
                self.key_buffer.push(character);
                if self.key_buffer.len() > 1024 * 1024 {
                    return Err("Object key exceeds indexed 1 MiB key limit".into());
                }
            }
        }
        Ok(())
    }
    fn key(&mut self) -> Result<u32, String> {
        self.string(true)?;
        if let Some(&id) = self.key_ids.get(self.key_buffer.as_str()) {
            return Ok(id);
        }
        if self.keys.len() >= 1_000_000 || self.key_bytes + self.key_buffer.len() > KEY_BUDGET {
            return Err(
                "Indexed key table exceeds 1,000,000 keys / 64 MiB decoded-key budget".into(),
            );
        }
        let id = compact(self.keys.len())?;
        let extra = self.key_buffer.len() + 2 * std::mem::size_of::<Arc<str>>() + 192;
        self.key_reservation.resize(
            self.key_reservation
                .bytes()
                .checked_add(extra)
                .ok_or("Key size overflow")?,
        )?;
        let key: Arc<str> = Arc::from(self.key_buffer.as_str());
        self.key_bytes += key.len();
        self.key_ids.insert(Arc::clone(&key), id);
        self.keys.push(key);
        Ok(id)
    }
    fn digits(&mut self) -> Result<(), String> {
        while matches!(self.reader.peek()?, Some(b'0'..=b'9')) {
            self.reader.next()?;
            if self.reader.pos & 4095 == 0 {
                self.check()?;
                self.progress.done.store(self.reader.pos, Ordering::Relaxed);
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
    fn value(&mut self, parent: u32, segment: u32, depth: u32) -> Result<u32, String> {
        self.check()?;
        self.space()?;
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
        let id = compact(self.nodes.len())?;
        compact(self.nodes.len() + 1)?;
        grow(
            &mut self.nodes,
            &mut self.allocated,
            &self.budget,
            &mut self.resources,
        )?;
        grow(
            &mut self.ranges,
            &mut self.allocated,
            &self.budget,
            &mut self.resources,
        )?;
        self.nodes.push(Node {
            parent,
            depth,
            end: id + 1,
            segment,
            child_start: 0,
            child_count: 0,
            kind,
        });
        self.ranges.push(Range { start, end: start });
        match kind {
            Kind::String => self.string(false)?,
            Kind::Number => self.number()?,
            Kind::Null | Kind::Boolean => {
                let literal: &[u8] = match self.reader.peek()? {
                    Some(b't') => b"true",
                    Some(b'f') => b"false",
                    _ => b"null",
                };
                for &byte in literal {
                    self.reader.expect(byte)?;
                }
            }
            Kind::Array | Kind::Object => {
                if depth >= 127 {
                    return Err(self.reader.error("recursion limit exceeded"));
                }
                let object = matches!(kind, Kind::Object);
                let close = if object { b'}' } else { b']' };
                self.reader.next()?;
                self.space()?;
                let mut direct: Vec<u32> = Vec::new();
                let mut direct_lease =
                    self.budget
                        .reserve(Resource::Memory, 0, "container temporary children")?;
                let mut positions_lease =
                    self.budget
                        .reserve(Resource::Memory, 0, "duplicate-key map estimate")?;
                let mut positions: Option<HashMap<u32, usize>> = None;
                if self.reader.peek()? != Some(close) {
                    loop {
                        self.space()?;
                        let child_segment = if object {
                            let key = self.key()?;
                            self.space()?;
                            self.reader.expect(b':')?;
                            key
                        } else {
                            compact(direct.len())?
                        };
                        let child = self.value(id, child_segment, depth + 1)?;
                        let existing = if object {
                            if let Some(positions) = &positions {
                                positions.get(&child_segment).copied()
                            } else {
                                direct.iter().position(|&child| {
                                    self.nodes[child as usize].segment == child_segment
                                })
                            }
                        } else {
                            None
                        };
                        if let Some(position) = existing {
                            direct[position] = child;
                            self.duplicates = true;
                        } else {
                            if let Some(positions) = &mut positions {
                                positions.insert(child_segment, direct.len());
                            }
                            if direct.len() == direct.capacity() {
                                let extra = direct.capacity().max(64);
                                reserve_vec(&mut direct, extra, &mut direct_lease)?;
                            }
                            if object {
                                positions_lease.resize((direct.len() + 1) * 64)?;
                            }
                            direct.push(child);
                            if object && direct.len() == 64 {
                                positions = Some(
                                    direct
                                        .iter()
                                        .enumerate()
                                        .map(|(i, &child)| (self.nodes[child as usize].segment, i))
                                        .collect(),
                                );
                            }
                        }
                        self.space()?;
                        if self.reader.peek()? == Some(close) {
                            break;
                        }
                        self.reader.expect(b',')?;
                    }
                }
                self.reader.expect(close)?;
                self.nodes[id as usize].child_start = compact(self.children.len())?;
                self.nodes[id as usize].child_count = compact(direct.len())?;
                for child in direct {
                    grow(
                        &mut self.children,
                        &mut self.allocated,
                        &self.budget,
                        &mut self.resources,
                    )?;
                    self.children.push(child);
                }
            }
        }
        self.nodes[id as usize].end = compact(self.nodes.len())?;
        self.ranges[id as usize].end = self.reader.pos;
        self.progress.done.store(self.reader.pos, Ordering::Relaxed);
        Ok(id)
    }
}

pub(super) fn open_with_budget(
    path: &Path,
    cancelled: impl Fn() -> bool,
    progress: &LoadProgress,
    budget: BudgetManager,
) -> Result<Document, String> {
    let source = Snapshot::copy(path, &cancelled, progress, &budget)?;
    let bytes =
        usize::try_from(source.len).map_err(|_| "File size exceeds platform address range")?;
    progress.phase.store(1, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let _workspace = budget.reserve(
        Resource::Memory,
        BLOCK + 2 * 1024 * 1024,
        "scanner workspace",
    )?;
    let mut parser = Parser {
        reader: Reader::new(&source)?.with_cancellation(&cancelled),
        cancelled: &cancelled,
        progress,
        nodes: Vec::new(),
        ranges: Vec::new(),
        children: Vec::new(),
        keys: Vec::new(),
        key_ids: HashMap::new(),
        key_buffer: String::new(),
        key_bytes: 0,
        allocated: 0,
        duplicates: false,
        budget: budget.clone(),
        resources: Vec::new(),
        key_reservation: budget.reserve(Resource::Memory, 0, "shared key estimate")?,
    };
    while parser.reader.peek()? == Some(0xef) {
        parser.reader.expect(0xef)?;
        parser.reader.expect(0xbb)?;
        parser.reader.expect(0xbf)?;
    }
    parser.value(u32::MAX, 0, 0)?;
    parser.space()?;
    if parser.reader.peek()?.is_some() {
        return Err(parser.reader.error("trailing characters"));
    }
    parser.check()?;
    progress.phase.store(2, Ordering::Relaxed);
    let Parser {
        mut nodes,
        mut ranges,
        mut children,
        keys,
        key_ids,
        duplicates,
        allocated,
        mut resources,
        key_reservation,
        ..
    } = parser;
    resources.push(key_reservation);
    if duplicates {
        let extra = nodes.len() * 48;
        if allocated
            .checked_add(extra)
            .is_none_or(|bytes| bytes > INDEX_BUDGET)
        {
            return Err("Duplicate-key compaction exceeds index budget".into());
        }
        fn copy(
            id: usize,
            parent: u32,
            previous: (&[Node], &[Range], &[u32]),
            output: (&mut Vec<Node>, &mut Vec<Range>, &mut Vec<u32>),
            cancelled: &impl Fn() -> bool,
        ) -> Result<u32, String> {
            let (old, ranges, edges) = previous;
            let (nodes, spans, children) = output;
            if nodes.len() % 1024 == 0 && cancelled() {
                return Err("Loading cancelled".into());
            }
            let node = &old[id];
            let new_id = compact(nodes.len())?;
            let start = children.len();
            children.resize(start + node.child_count as usize, 0);
            nodes.push(Node {
                parent,
                depth: node.depth,
                end: new_id + 1,
                segment: node.segment,
                child_start: compact(start)?,
                child_count: node.child_count,
                kind: node.kind,
            });
            spans.push(ranges[id]);
            for (ordinal, &child) in edges
                [node.child_start as usize..node.child_start as usize + node.child_count as usize]
                .iter()
                .enumerate()
            {
                children[start + ordinal] = copy(
                    child as usize,
                    new_id,
                    (old, ranges, edges),
                    (&mut *nodes, &mut *spans, &mut *children),
                    cancelled,
                )?;
            }
            nodes[new_id as usize].end = compact(nodes.len())?;
            Ok(new_id)
        }
        resources.push(budget.reserve(
            Resource::Memory,
            nodes.len() * std::mem::size_of::<Node>()
                + ranges.len() * std::mem::size_of::<Range>()
                + children.len() * 4,
            "duplicate key compaction",
        )?);
        let mut canonical = Vec::with_capacity(nodes.len());
        let mut spans = Vec::with_capacity(nodes.len());
        let mut edges = Vec::with_capacity(children.len());
        copy(
            0,
            u32::MAX,
            (&nodes, &ranges, &children),
            (&mut canonical, &mut spans, &mut edges),
            &cancelled,
        )?;
        nodes = canonical;
        ranges = spans;
        children = edges;
        resources.retain(|lease| lease.label() != "indexed structure growth");
    }
    for (id, node) in nodes.iter().enumerate() {
        if id % 1024 == 0 && cancelled() {
            return Err("Loading cancelled".into());
        }
        if matches!(node.kind, Kind::Object) {
            children
                [node.child_start as usize..node.child_start as usize + node.child_count as usize]
                .sort_unstable_by_key(|&child| nodes[child as usize].segment);
        }
    }
    progress.done.store(source.len, Ordering::Relaxed);
    if cancelled() {
        return Err("Loading cancelled".into());
    }
    Ok(Document {
        session: SessionId::fresh(),
        budget,
        _resources: resources,
        storage: Storage::Indexed(Store {
            source,
            ranges,
            cache: Mutex::new(Cache::default()),
        }),
        nodes,
        bytes,
        children,
        keys,
        key_ids,
    })
}

#[derive(Clone)]
enum Selector {
    Name(String),
    Index(i64),
    Wildcard,
    Slice(Option<i64>, Option<i64>, i64),
}
fn result_id(result: &mut NodeIds, id: usize) -> Result<(), String> {
    if result.len() == result.capacity() {
        result.reserve_exact(result.capacity().max(1024))?;
    }
    result.push(id as u32)?;
    Ok(())
}
fn unsupported() -> String {
    "Indexed JSONPath supports fields, unions, wildcard, recursion and slices; filters/functions are unsupported. Disable Indexed files and reopen for full JSONPath (no automatic full-document fallback).".into()
}
fn selectors(text: &str) -> Result<Vec<Selector>, String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escape = false;
    for (i, ch) in text.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if let Some(delimiter) = quote {
            if ch == '\\' {
                escape = true;
            } else if ch == delimiter {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == ',' {
            parts.push(&text[start..i]);
            start = i + 1;
        }
    }
    parts.push(&text[start..]);
    parts
        .into_iter()
        .map(|part| {
            let part = part.trim();
            if part == "*" {
                return Ok(Selector::Wildcard);
            }
            if part.starts_with('"') {
                return serde_json::from_str(part)
                    .map(Selector::Name)
                    .map_err(|_| unsupported());
            }
            if part.starts_with('\'') && part.ends_with('\'') {
                let mut encoded = String::from("\"");
                let mut chars = part[1..part.len() - 1].chars();
                while let Some(ch) = chars.next() {
                    match ch {
                        '\\' => {
                            let next = chars.next().ok_or_else(unsupported)?;
                            if next == '\'' {
                                encoded.push('\'');
                            } else {
                                encoded.push('\\');
                                encoded.push(next);
                            }
                        }
                        '"' => encoded.push_str("\\\""),
                        _ => encoded.push(ch),
                    }
                }
                encoded.push('"');
                return serde_json::from_str(&encoded)
                    .map(Selector::Name)
                    .map_err(|_| unsupported());
            }
            if part.contains(':') {
                let parts: Vec<_> = part.split(':').collect();
                if !(2..=3).contains(&parts.len()) {
                    return Err(unsupported());
                }
                let number = |text: &str| {
                    if text.trim().is_empty() {
                        Ok(None)
                    } else {
                        text.trim()
                            .parse::<i64>()
                            .map(Some)
                            .map_err(|_| unsupported())
                    }
                };
                return Ok(Selector::Slice(
                    number(parts[0])?,
                    number(parts[1])?,
                    if parts.len() == 3 {
                        number(parts[2])?.unwrap_or(1)
                    } else {
                        1
                    },
                ));
            }
            part.parse().map(Selector::Index).map_err(|_| unsupported())
        })
        .collect()
}
pub(super) fn query(
    doc: &Document,
    expression: &str,
    cancelled: impl Fn() -> bool,
) -> Result<NodeIds, String> {
    let mut segments = Vec::new();
    let bytes = expression.as_bytes();
    let mut position = 1;
    while position < bytes.len() {
        let mut recursive = false;
        if bytes[position] == b'.' {
            position += 1;
            if position < bytes.len() && bytes[position] == b'.' {
                recursive = true;
                position += 1;
            }
            if position >= bytes.len() {
                return Err(unsupported());
            }
            if bytes[position] != b'[' {
                let start = position;
                while position < bytes.len() && bytes[position] != b'.' && bytes[position] != b'[' {
                    position += 1;
                }
                let name = &expression[start..position];
                segments.push((
                    recursive,
                    vec![if name == "*" {
                        Selector::Wildcard
                    } else {
                        Selector::Name(name.into())
                    }],
                ));
                continue;
            }
        }
        if bytes[position] != b'[' {
            return Err(unsupported());
        }
        position += 1;
        let start = position;
        let mut quote = None;
        let mut escape = false;
        while position < bytes.len() {
            let byte = bytes[position];
            if escape {
                escape = false;
            } else if let Some(delimiter) = quote {
                if byte == b'\\' {
                    escape = true;
                } else if byte == delimiter {
                    quote = None;
                }
            } else if byte == b'\'' || byte == b'"' {
                quote = Some(byte);
            } else if byte == b']' {
                break;
            } else if byte == b'?' || byte == b'(' {
                return Err(unsupported());
            }
            position += 1;
        }
        if position == bytes.len() {
            return Err(unsupported());
        }
        segments.push((recursive, selectors(&expression[start..position])?));
        position += 1;
    }
    let mut current = NodeIds::new(&doc.budget);
    current.push(0)?;
    for (recursive, selectors) in segments {
        let mut result = NodeIds::new(&doc.budget);
        for &base in &current {
            let end = if recursive {
                doc.nodes[base as usize].end()
            } else {
                base as usize + 1
            };
            for id in base as usize..end {
                if cancelled() {
                    return Err("Query cancelled".into());
                }
                for selector in &selectors {
                    match selector {
                        Selector::Name(name) => {
                            if let Some(id) = doc.child_name(id, name) {
                                result_id(&mut result, id)?;
                            }
                        }
                        Selector::Wildcard => {
                            for child in doc.direct_children(id) {
                                if cancelled() {
                                    return Err("Query cancelled".into());
                                }
                                result_id(&mut result, child)?;
                            }
                        }
                        Selector::Index(index) => {
                            let len = doc.nodes[id].child_count as i64;
                            let index = if *index < 0 {
                                len.saturating_add(*index)
                            } else {
                                *index
                            };
                            if index >= 0
                                && let Some(id) = doc.child_index(id, index as usize)
                            {
                                result_id(&mut result, id)?;
                            }
                        }
                        Selector::Slice(start, end, step) => {
                            if !matches!(doc.nodes[id].kind, Kind::Array) || *step == 0 {
                                continue;
                            }
                            let len = doc.nodes[id].child_count as i64;
                            let normalize = |value: Option<i64>, default| {
                                value.map_or(default, |value| {
                                    if value < 0 {
                                        len.saturating_add(value)
                                    } else {
                                        value
                                    }
                                })
                            };
                            let (mut index, end) = if *step > 0 {
                                (
                                    normalize(*start, 0).clamp(0, len),
                                    normalize(*end, len).clamp(0, len),
                                )
                            } else {
                                (
                                    normalize(*start, len - 1).clamp(-1, len - 1),
                                    normalize(*end, -1).clamp(-1, len - 1),
                                )
                            };
                            while if *step > 0 { index < end } else { index > end } {
                                if cancelled() {
                                    return Err("Query cancelled".into());
                                }
                                result_id(
                                    &mut result,
                                    doc.child_index(id, index as usize).unwrap(),
                                )?;
                                let Some(next) = index.checked_add(*step) else {
                                    break;
                                };
                                index = next;
                            }
                        }
                    }
                }
            }
        }
        current = result;
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::SearchScope;
    struct Input(PathBuf);
    impl Input {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "FerrisJson-index-test-{}-{}.json",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.write_all(bytes).unwrap();
            Self(path)
        }
        fn open(&self) -> Document {
            Document::open_indexed(&self.0, || false, &LoadProgress::default()).unwrap()
        }
    }
    impl Drop for Input {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    fn json(doc: &Document, id: usize) -> Value {
        let mut bytes = Vec::new();
        doc.write_node(&mut bytes, id).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    #[test]
    fn indexed_tree_matches_value_backend_with_duplicates_and_special_keys() {
        let text = "\u{feff}{\"z\":{\"old\":0},\"a/b\":{\"~x\":[1,{\"a.b\":\"\\u4e2d文\"}]},\"z\":{\"last\":123456789012345678901234567890},\"\":false}";
        let input = Input::new(text.as_bytes());
        let indexed = input.open();
        let memory = Document::parse(text).unwrap();
        assert!(indexed.value().is_none());
        assert_eq!(indexed.nodes.len(), memory.nodes.len());
        assert_eq!(json(&indexed, 0), *memory.value().unwrap());
        for id in 0..memory.nodes.len() {
            assert_eq!(indexed.path(id), memory.path(id));
            assert_eq!(indexed.nodes[id].parent(), memory.nodes[id].parent());
            assert_eq!(indexed.nodes[id].end(), memory.nodes[id].end());
            assert_eq!(json(&indexed, id), *memory.at(id));
            assert_eq!(indexed.find_path(&memory.path(id)).unwrap(), id);
            assert_eq!(
                indexed.query_path(&memory.pointer(id)).ok(),
                memory.query_path(&memory.pointer(id)).ok()
            );
        }
    }
    #[test]
    fn supported_queries_preserve_order_slices_recursion_and_duplicate_results() {
        let text = r#"{"rows":[{"n":0},{"n":1},{"n":2},{"n":3}],"a/b":{"x.y":true},"quote'\"":42,"empty":[]}"#;
        let input = Input::new(text.as_bytes());
        let indexed = input.open();
        let memory = Document::parse(text).unwrap();
        for expression in [
            "$",
            ".",
            "$.rows[*].n",
            "$.rows[3,0,3].n",
            "$..n",
            "$..*",
            "$.rows[::-1].n",
            "$.rows[-3:-1].n",
            "$.rows[3:0:-2].n",
            "$.rows[::0]",
            "$['a/b']['x.y']",
            "$['rows','empty']",
            "$.empty[::-1]",
            "$.rows[-99]",
            "$.rows[99]",
            "$.missing",
        ] {
            let expected = memory.query_path(expression).unwrap();
            assert_eq!(
                indexed.query_path(expression).unwrap(),
                expected,
                "{expression}"
            );
            let mut actual = Vec::new();
            indexed.write_results(&mut actual, &expected).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&actual).unwrap(),
                serde_json::from_str::<Value>(&memory.results_json(&expected)).unwrap()
            );
        }
        assert!(
            indexed
                .query_path("$.rows[?(@.n > 0)]")
                .unwrap_err()
                .contains("unsupported")
        );
        assert!(indexed.query_path_cancellable("$..*", || true).is_err());
    }
    #[test]
    fn strict_validation_matches_serde_and_reports_locations() {
        let valid = [
            "null",
            "true",
            "-0",
            "1.2300e+02",
            "1e999",
            "[]",
            "{}",
            r#""\uD83E\uDD80""#,
            "[127,-123456789012345678901234567890]",
        ];
        for text in valid {
            let input = Input::new(text.as_bytes());
            assert!(serde_json::from_str::<Value>(text).is_ok());
            input.open();
        }
        let invalid = [
            "",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "-01",
            "+1",
            "1.",
            "1e+",
            "false true",
            "{\n\"a\": tru}",
            r#""\uD800""#,
            r#""\uDC00""#,
            r#""\x00""#,
            "\"line\nbreak\"",
            "/*comment*/null",
        ];
        for text in invalid {
            let input = Input::new(text.as_bytes());
            assert!(serde_json::from_str::<Value>(text).is_err());
            let error = Document::open_indexed(&input.0, || false, &LoadProgress::default())
                .err()
                .unwrap();
            assert!(
                error.contains("line") && error.contains("column"),
                "{text}: {error}"
            );
        }
        let input = Input::new(b"{\"s\":\"\xf0\x80\x80\x80\"}");
        assert!(Document::open_indexed(&input.0, || false, &LoadProgress::default()).is_err());
        for depth in [126, 127, 128] {
            let text = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            let input = Input::new(text.as_bytes());
            assert_eq!(
                Document::open_indexed(&input.0, || false, &LoadProgress::default()).is_ok(),
                serde_json::from_str::<Value>(&text).is_ok()
            );
        }
    }
    #[test]
    fn decoded_search_handles_chunk_boundaries_unicode_and_number_spellings() {
        let long = format!("{}K한🦀END{}", "x".repeat(BLOCK - 3), "y".repeat(300_000));
        let text = format!(
            r#"{{"key\u0041":"\u4e2d文\nquoted\"","long":{},"numbers":[-0,1e2,1.2300e+02],"b":true,"n":null}}"#,
            serde_json::to_string(&long).unwrap()
        );
        let input = Input::new(text.as_bytes());
        let indexed = input.open();
        let memory = Document::parse(&text).unwrap();
        for (term, case, scope) in [
            ("中文", true, SearchScope::Values),
            ("k한🦀END", false, SearchScope::Values),
            ("keya", false, SearchScope::Keys),
            ("0", true, SearchScope::Values),
            ("e+2", false, SearchScope::Values),
            ("true", true, SearchScope::All),
            ("null", true, SearchScope::Values),
            ("quoted\"", true, SearchScope::Values),
        ] {
            assert_eq!(
                indexed.search(term, case, false, scope).unwrap(),
                memory.search(term, case, false, scope).unwrap(),
                "{term}"
            );
        }
        assert!(
            indexed
                .search("^x.*END", true, true, SearchScope::Values)
                .unwrap_err()
                .contains("256 KiB")
        );
        assert!(
            indexed
                .search_cancellable("absent", true, false, SearchScope::Values, || true)
                .is_err()
        );
        let preview = indexed.preview(indexed.find_path("$.long").unwrap());
        assert!(preview.contains("Preview truncated"));
        assert!(!preview.contains('\u{fffd}'));
    }
    #[test]
    fn snapshot_survives_source_changes_and_bounded_subtrees_are_cached() {
        let input = Input::new(br#"{"record":{"name":"one","n":7},"other":8}"#);
        let indexed = input.open();
        fs::write(&input.0, b"null").unwrap();
        let id = indexed.find_path("$.record").unwrap();
        let first = indexed.read_subtree(id).unwrap();
        let again = indexed.read_subtree(id).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(first["n"], 7);
        assert_eq!(json(&indexed, 0)["record"]["name"], "one");
        if let Storage::Indexed(store) = &indexed.storage {
            assert!(store.cache.lock().unwrap().bytes <= CACHE_BUDGET);
            let path = store.source.path.clone();
            drop(first);
            drop(again);
            drop(indexed);
            if let Some(path) = path {
                assert!(!path.exists());
            }
        }
    }
    #[test]
    fn cancellation_and_capacity_budget_are_enforced_without_full_value() {
        let input = Input::new(b"[]");
        assert!(
            Document::open_indexed(&input.0, || true, &LoadProgress::default())
                .err()
                .unwrap()
                .contains("cancelled")
        );
        let text = serde_json::to_string(&"x".repeat(BLOCK * 3)).unwrap();
        let scanning = Input::new(text.as_bytes());
        let progress = LoadProgress::default();
        let error = Document::open_indexed(
            &scanning.0,
            || {
                progress.phase.load(Ordering::Relaxed) == 1
                    && progress.done.load(Ordering::Relaxed) > 0
            },
            &progress,
        )
        .err()
        .unwrap();
        assert!(error.contains("cancelled"));
        assert!(progress.done.load(Ordering::Relaxed) < text.len() as u64);
        let mut values: Vec<Node> = Vec::new();
        let mut budget = INDEX_BUDGET;
        assert!(
            grow(
                &mut values,
                &mut budget,
                &BudgetManager::default(),
                &mut Vec::new()
            )
            .is_err()
        );
        assert_eq!(values.capacity(), 0);
        let text = serde_json::to_string(&"x".repeat(SUBTREE_LIMIT + 1)).unwrap();
        let input = Input::new(text.as_bytes());
        let doc = input.open();
        assert!(doc.read_subtree(0).unwrap_err().contains("limit"));
        assert!(doc.value().is_none());
    }
}
