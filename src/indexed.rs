//! Validated file-range backend. No memory mapping and no whole-document Value.
use super::paged::{Paged, Record};
use super::{Document, Kind, Node, NodeIds, SearchScope, Storage, compact, short};
use regex::RegexBuilder;
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
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
struct Snapshot {
    file: Mutex<Option<File>>,
    path: Option<PathBuf>,
    len: u64,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        self.file
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}
impl Snapshot {
    fn copy(
        path: &Path,
        cancelled: &impl Fn() -> bool,
        progress: &LoadProgress,
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
                        file: Mutex::new(Some(file)),
                        path: Some(path),
                        len: 0,
                    });
                    #[cfg(unix)]
                    if let Some(snapshot) = &mut result {
                        if fs::remove_file(snapshot.path.as_ref().unwrap()).is_ok() {
                            snapshot.path = None;
                        }
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
            let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            snapshot
                .file
                .get_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .write_all(&buffer[..count])
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
        if let Some(path) = &snapshot.path {
            if fs::remove_file(path).is_ok() {
                snapshot.path = None;
            }
        }
        Ok(snapshot)
    }
    fn read(&self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
        if offset
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err(io::Error::other("Snapshot range out of bounds"));
        }
        let mut guard = self
            .file
            .lock()
            .map_err(|_| io::Error::other("Snapshot lock poisoned"))?;
        let file = guard
            .as_mut()
            .ok_or_else(|| io::Error::other("Snapshot closed"))?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(bytes)
    }
}
struct Reader<'a> {
    source: &'a Snapshot,
    buffer: Vec<u8>,
    start: u64,
    count: usize,
    pos: u64,
    line: u64,
    column: u64,
}
impl<'a> Reader<'a> {
    fn new(source: &'a Snapshot) -> Self {
        Self {
            source,
            buffer: vec![0; BLOCK],
            start: 0,
            count: 0,
            pos: 0,
            line: 1,
            column: 0,
        }
    }
    fn error(&self, message: &str) -> String {
        format!(
            "Invalid JSON: {message} at line {} column {} (byte {})",
            self.line,
            self.column + 1,
            self.pos
        )
    }
    fn peek(&mut self) -> Result<Option<u8>, String> {
        if self.pos >= self.source.len {
            return Ok(None);
        }
        if self.pos < self.start || self.pos >= self.start + self.count as u64 {
            self.start = self.pos / BLOCK as u64 * BLOCK as u64;
            self.count = (self.source.len - self.start).min(BLOCK as u64) as usize;
            self.source
                .read(self.start, &mut self.buffer[..self.count])
                .map_err(|e| e.to_string())?;
        }
        Ok(Some(self.buffer[(self.pos - self.start) as usize]))
    }
    fn next(&mut self) -> Result<u8, String> {
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
    fn expect(&mut self, expected: u8) -> Result<(), String> {
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
    fn character(&mut self) -> Result<Option<char>, String> {
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
    fn scalar(&mut self, range: Range) -> Result<Value, String> {
        if range.end - range.start > SUBTREE_LIMIT as u64 {
            return Err(
                "Indexed scalar parsing exceeds 256 KiB; use in-memory mode for this search".into(),
            );
        }
        self.pos = range.start;
        self.peek()?;
        if range.end <= self.start + self.count as u64 {
            let start = (range.start - self.start) as usize;
            serde_json::from_slice(&self.buffer[start..start + (range.end - range.start) as usize])
                .map_err(|e| e.to_string())
        } else {
            let mut bytes = vec![0; (range.end - range.start) as usize];
            self.source
                .read(range.start, &mut bytes)
                .map_err(|e| e.to_string())?;
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())
        }
    }
}
#[derive(Clone, Copy)]
struct Range {
    start: u64,
    end: u64,
}
impl Record for Range {
    const WIDTH: usize = 16;
    fn encode(self, bytes: &mut [u8]) {
        bytes[..8].copy_from_slice(&self.start.to_le_bytes());
        bytes[8..].copy_from_slice(&self.end.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        Ok(Self {
            start: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            end: u64::from_le_bytes(bytes[8..].try_into().unwrap()),
        })
    }
}
#[derive(Default)]
struct Cache {
    entries: HashMap<usize, (Arc<Value>, usize)>,
    order: VecDeque<usize>,
    bytes: usize,
}
pub(super) struct Store {
    source: Snapshot,
    ranges: Paged<Range>,
    nodes: Paged<Node>,
    children: Paged<u32>,
    cache: Mutex<Cache>,
}
fn value_cost(value: &Value) -> usize {
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
    pub(super) fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub(super) fn node(&self, id: usize) -> Result<Node, String> {
        self.nodes.get(id)
    }
    pub(super) fn edge(&self, id: usize) -> Result<u32, String> {
        self.children.get(id)
    }
    pub(super) fn node_cache_bytes(&self) -> usize {
        self.nodes.cache_bytes()
    }
    pub(super) fn structure_cache_bytes(&self) -> usize {
        self.nodes.cache_bytes() + self.ranges.cache_bytes() + self.children.cache_bytes()
    }
    pub(super) fn disk_bytes(&self) -> u64 {
        self.nodes.disk_bytes() + self.ranges.disk_bytes() + self.children.disk_bytes()
    }
    pub(super) fn subtree(&self, doc: &Document, id: usize) -> Result<Arc<Value>, String> {
        let range = self.ranges.get(id)?;
        if range.end - range.start > SUBTREE_LIMIT as u64 || doc.node(id)?.end() - id > 16_384 {
            return Err("Subtree exceeds the bounded parse limit (256 KiB / 16,384 nodes); use range preview or export".into());
        }
        {
            let mut cache = self.cache.lock().map_err(|_| "Subtree cache poisoned")?;
            if let Some((value, _)) = cache.entries.get(&id) {
                let value = Arc::clone(value);
                cache.order.retain(|&old| old != id);
                cache.order.push_back(id);
                return Ok(value);
            }
        }
        let mut bytes = vec![0; (range.end - range.start) as usize];
        self.source
            .read(range.start, &mut bytes)
            .map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let cost = value_cost(&value);
        let value = Arc::new(value);
        let mut cache = self.cache.lock().map_err(|_| "Subtree cache poisoned")?;
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
        let node = doc.node(id)?;
        match node.kind {
            Kind::Object => Ok(format!("{{ {} keys }}", node.child_count)),
            Kind::Array => Ok(format!("[ {} items ]", node.child_count)),
            Kind::String => {
                let mut reader = Reader::new(&self.source);
                reader.pos = self.ranges.get(id)?.start;
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
                let range = self.ranges.get(id)?;
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
            let node = doc.node(id).map_err(io::Error::other)?;
            if node.container() {
                let object = matches!(node.kind, Kind::Object);
                output.write_all(if object { b"{" } else { b"[" })?;
                for (ordinal, child) in doc.direct_children(id).enumerate() {
                    let child = child.map_err(io::Error::other)?;
                    if ordinal != 0 {
                        output.write_all(b",")?;
                    }
                    output.write_all(b"\n")?;
                    indent(output, depth + 1)?;
                    if object {
                        serde_json::to_writer(
                            &mut *output,
                            doc.try_key(child).map_err(io::Error::other)?.as_ref(),
                        )
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
                let range = store.ranges.get(id).map_err(io::Error::other)?;
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
        let mut reader = Reader::new(&self.source);
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
    pub(super) fn search(
        &self,
        doc: &Document,
        term: &str,
        case: bool,
        regex: bool,
        scope: SearchScope,
        cancelled: impl Fn() -> bool,
    ) -> Result<NodeIds, String> {
        if term.len() > 1024 * 1024 {
            return Err("Indexed search term exceeds 1 MiB budget".into());
        }
        let matcher = RegexBuilder::new(&if regex {
            term.to_owned()
        } else {
            regex::escape(term)
        })
        .case_insensitive(!case)
        .build()
        .map_err(|e| format!("Invalid regular expression: {e}"))?;
        let mut keys = Vec::new();
        if scope != SearchScope::Values {
            keys.reserve(doc.keys.len());
            for (index, key) in doc.keys.iter().enumerate() {
                if index % 1024 == 0 && cancelled() {
                    return Err("Search cancelled".into());
                }
                keys.push(matcher.is_match(key));
            }
        }
        let array_keys = regex || term.bytes().all(|byte| byte.is_ascii_digit());
        let mut reader = Reader::new(&self.source);
        let mut ids = Vec::new();
        for id in 0..doc.node_count() {
            let node = doc.node(id)?;
            if id % 1024 == 0 && cancelled() {
                return Err("Search cancelled".into());
            }
            let key_match = scope != SearchScope::Values
                && match node.parent() {
                    None => matcher.is_match("$"),
                    Some(parent) if matches!(doc.node(parent)?.kind, Kind::Array) => {
                        array_keys && matcher.is_match(&node.segment.to_string())
                    }
                    _ => keys[node.segment as usize],
                };
            let value_match = if key_match || scope == SearchScope::Keys {
                false
            } else {
                match node.kind {
                    Kind::Object | Kind::Array => false,
                    Kind::Null => matcher.is_match("null"),
                    Kind::Boolean | Kind::Number => {
                        let value = reader.scalar(self.ranges.get(id)?)?;
                        match &value {
                            Value::Number(number) => matcher.is_match(number.as_str()),
                            _ => matcher.is_match(&value.to_string()),
                        }
                    }
                    Kind::String if regex => {
                        if self.ranges.get(id)?.end - self.ranges.get(id)?.start
                            > SUBTREE_LIMIT as u64
                        {
                            return Err("Indexed regex search requires strings <=256 KiB; use in-memory mode for larger values".into());
                        }
                        let value = reader.scalar(self.ranges.get(id)?)?;
                        matcher.is_match(value.as_str().unwrap())
                    }
                    Kind::String => {
                        reader.pos = self.ranges.get(id)?.start;
                        reader.expect(b'"')?;
                        let mut window = String::new();
                        let mut added = 0;
                        let overlap = term.chars().count() * 4;
                        let mut matched = false;
                        while let Some(character) = reader.character()? {
                            window.push(character);
                            added += character.len_utf8();
                            if added >= BLOCK {
                                if cancelled() {
                                    return Err("Search cancelled".into());
                                }
                                if matcher.is_match(&window) {
                                    matched = true;
                                    break;
                                }
                                let mut start = window.len().saturating_sub(overlap);
                                while !window.is_char_boundary(start) {
                                    start -= 1;
                                }
                                window.drain(..start);
                                added = 0;
                            }
                        }
                        matched || matcher.is_match(&window)
                    }
                }
            };
            if key_match || value_match {
                if ids.len() == ids.capacity() {
                    ids.reserve_exact(ids.capacity().max(1024).min(doc.node_count() - ids.len()));
                }
                ids.push(id as u32);
            }
        }
        Ok(ids)
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
}
fn grow<T>(values: &mut Vec<T>, allocated: &mut usize) -> Result<(), String> {
    if values.len() == values.capacity() {
        let extra = (values.capacity() / 2).clamp(1024, 1_048_576);
        let bytes = extra * std::mem::size_of::<T>();
        if allocated
            .checked_add(bytes)
            .is_none_or(|total| total > INDEX_BUDGET)
        {
            return Err("Indexed structural capacity exceeds 1.5 GiB budget".into());
        }
        values
            .try_reserve_exact(extra)
            .map_err(|e| format!("Cannot allocate index: {e}"))?;
        *allocated += bytes;
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
        grow(&mut self.nodes, &mut self.allocated)?;
        grow(&mut self.ranges, &mut self.allocated)?;
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
                    grow(&mut self.children, &mut self.allocated)?;
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

pub(super) fn open(
    path: &Path,
    cancelled: impl Fn() -> bool,
    progress: &LoadProgress,
) -> Result<Document, String> {
    let source = Snapshot::copy(path, &cancelled, progress)?;
    let bytes =
        usize::try_from(source.len).map_err(|_| "File size exceeds platform address range")?;
    progress.phase.store(1, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let mut parser = Parser {
        reader: Reader::new(&source),
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
        ..
    } = parser;
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
    let nodes = Paged::create(nodes, 8 * 1024 * 1024, &cancelled)?;
    let ranges = Paged::create(ranges, 4 * 1024 * 1024, &cancelled)?;
    let children = Paged::create(children, 4 * 1024 * 1024, &cancelled)?;
    if cancelled() {
        return Err("Loading cancelled".into());
    }
    Ok(Document {
        storage: Storage::Indexed(Box::new(Store {
            source,
            ranges,
            nodes,
            children,
            cache: Mutex::new(Cache::default()),
        })),
        nodes: Vec::new(),
        bytes,
        children: Vec::new(),
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
    const LIMIT: usize = 256 * 1024 * 1024 / 4;
    if result.len() >= LIMIT {
        return Err("Indexed query exceeds 256 MiB result-ID budget; narrow the expression".into());
    }
    if result.len() == result.capacity() {
        result.reserve_exact(result.capacity().max(1024).min(LIMIT - result.len()));
    }
    result.push(id as u32);
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
    let mut current = vec![0];
    for (recursive, selectors) in segments {
        let mut result = Vec::new();
        for &base in &current {
            let end = if recursive {
                doc.node(base as usize)?.end()
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
                            if let Some(id) = doc.child_name(id, name)? {
                                result_id(&mut result, id)?;
                            }
                        }
                        Selector::Wildcard => {
                            for child in doc.direct_children(id) {
                                let child = child?;
                                if cancelled() {
                                    return Err("Query cancelled".into());
                                }
                                result_id(&mut result, child)?;
                            }
                        }
                        Selector::Index(index) => {
                            let len = doc.node(id)?.child_count as i64;
                            let index = if *index < 0 {
                                len.saturating_add(*index)
                            } else {
                                *index
                            };
                            if index >= 0
                                && let Some(id) = doc.child_index(id, index as usize)?
                            {
                                result_id(&mut result, id)?;
                            }
                        }
                        Selector::Slice(start, end, step) => {
                            if !matches!(doc.node(id)?.kind, Kind::Array) || *step == 0 {
                                continue;
                            }
                            let len = doc.node(id)?.child_count as i64;
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
                                    doc.child_index(id, index as usize)?.unwrap(),
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
        assert_eq!(indexed.node_count(), memory.node_count());
        assert_eq!(json(&indexed, 0), *memory.value().unwrap());
        for id in 0..memory.node_count() {
            assert_eq!(indexed.path(id), memory.path(id));
            assert_eq!(
                indexed.node(id).unwrap().parent(),
                memory.node(id).unwrap().parent()
            );
            assert_eq!(
                indexed.node(id).unwrap().end(),
                memory.node(id).unwrap().end()
            );
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
        assert!(grow(&mut values, &mut budget).is_err());
        assert_eq!(values.capacity(), 0);
        let text = serde_json::to_string(&"x".repeat(SUBTREE_LIMIT + 1)).unwrap();
        let input = Input::new(text.as_bytes());
        let doc = input.open();
        assert!(doc.read_subtree(0).unwrap_err().contains("limit"));
        assert!(doc.value().is_none());
    }
    #[test]
    fn paged_tables_preserve_navigation_search_and_output_after_eviction() {
        let value = serde_json::json!({"rows": (0..12_000).map(|i| serde_json::json!({"k": i, "text": format!("item-{i}")})).collect::<Vec<_>>()});
        let text = value.to_string();
        let input = Input::new(text.as_bytes());
        let mut indexed = input.open();
        let memory = Document::parse(&text).unwrap();
        if let Storage::Indexed(store) = &mut indexed.storage {
            store.nodes.test_budget(BLOCK);
            store.ranges.test_budget(BLOCK);
            store.children.test_budget(BLOCK);
        }
        assert!(indexed.nodes.is_empty() && indexed.children.is_empty());
        assert_eq!(
            indexed.structure_disk_bytes(),
            indexed.node_count() as u64 * 48 - 4
        );
        for expression in ["$.rows[0,4096,11999,0].k", "$.rows[::-2000].text", "$..k"] {
            assert_eq!(
                indexed.query_path(expression).unwrap(),
                memory.query_path(expression).unwrap()
            );
        }
        for index in [11_999, 0, 4096, 8192, 0] {
            let path = format!("$.rows[{index}].text");
            let id = indexed.find_path(&path).unwrap();
            assert_eq!(indexed.try_path(id).unwrap(), path);
            assert_eq!(indexed.row_preview(id), memory.row_preview(id));
        }
        assert_eq!(
            indexed
                .search("item-11999", false, false, SearchScope::Values)
                .unwrap(),
            memory
                .search("item-11999", false, false, SearchScope::Values)
                .unwrap()
        );
        assert_eq!(json(&indexed, 0), value);
        let expanded = indexed.initial_expanded().unwrap();
        assert_eq!(expanded, memory.initial_expanded().unwrap());
        assert_eq!(
            indexed.visible(&expanded).unwrap(),
            memory.visible(&expanded).unwrap()
        );
        assert!(indexed.structure_capacity_bytes() <= BLOCK * 3);
        if let Storage::Indexed(store) = &mut indexed.storage {
            store.nodes.fail_reads();
        }
        assert!(
            indexed
                .find_path("/rows/0")
                .unwrap_err()
                .contains("Cannot read index page")
        );
        assert!(
            indexed
                .search("item", false, false, SearchScope::Values)
                .is_err()
        );
        assert!(indexed.write_node(Vec::new(), 0).is_err());
        assert!(indexed.try_path(1).is_err());
        let mut rows = vec![0];
        assert!(indexed.visible_into(&expanded, &mut rows).is_err());
        assert!(rows.is_empty());
    }
}
