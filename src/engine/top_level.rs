//! Early top-level discovery. Entries are provisional; the existing loader is
//! still responsible for full JSON validation and the final document.
use super::{
    budget::{BudgetManager, Limits, Reservation, Resource},
    file_source::FileSource,
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const BLOCK: usize = 64 * 1024;
pub const WINDOW: usize = 64;
const NAME: usize = 640;
const HEADER: usize = 32;
const UNKNOWN: u64 = u64::MAX;
static SERIAL: AtomicU64 = AtomicU64::new(0);

pub struct Source {
    file: FileSource,
    pub bytes: u64,
}
impl Source {
    pub fn duplicate_file(&self) -> Result<File, String> {
        self.file.try_clone().map_err(|e| e.to_string())
    }
    pub fn read_text(&self, cancel: &dyn Fn() -> bool) -> Result<String, String> {
        if cancel() {
            return Err("Loading cancelled".into());
        }
        let len = usize::try_from(self.bytes).map_err(|_| "Input size overflow")?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).map_err(|e| e.to_string())?;
        let mut block = vec![0; BLOCK];
        while bytes.len() < len {
            if cancel() {
                return Err("Loading cancelled".into());
            }
            let n = (len - bytes.len()).min(BLOCK);
            self.read(bytes.len() as u64, &mut block[..n], cancel)?;
            bytes.extend_from_slice(&block[..n]);
        }
        if cancel() {
            return Err("Loading cancelled".into());
        }
        String::from_utf8(bytes).map_err(|e| e.to_string())
    }
    pub fn open(path: &Path) -> Result<Arc<Self>, String> {
        #[cfg(not(windows))]
        {
            let _ = path;
            Err("Protected top-level discovery currently requires Windows".into())
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let file = OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(path)
                .map_err(|e| format!("Cannot open a stable read-only source: {e}"))?;
            let meta = file.metadata().map_err(|e| e.to_string())?;
            if !meta.is_file() {
                return Err("Select a regular file".into());
            }
            Ok(Arc::new(Self {
                file: FileSource::new(file),
                bytes: meta.len(),
            }))
        }
    }
    fn read(&self, offset: u64, data: &mut [u8], cancel: &dyn Fn() -> bool) -> Result<(), String> {
        if offset
            .checked_add(data.len() as u64)
            .is_none_or(|n| n > self.bytes)
        {
            return Err("Discovery read outside source".into());
        }
        self.file
            .read_exact_at_cancellable(offset, data, cancel)
            .map_err(|e| e.to_string())
    }
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
    pub fn container(self) -> bool {
        matches!(self, Self::Object | Self::Array)
    }
    fn from_byte(b: u8) -> Result<Self, String> {
        match b {
            b'{' => Ok(Self::Object),
            b'[' => Ok(Self::Array),
            b'"' => Ok(Self::String),
            b'-' | b'0'..=b'9' => Ok(Self::Number),
            b't' | b'f' => Ok(Self::Boolean),
            b'n' => Ok(Self::Null),
            _ => Err("Expected a JSON value".into()),
        }
    }
    fn decode(b: u8) -> Result<Self, String> {
        match b {
            0 => Ok(Self::Object),
            1 => Ok(Self::Array),
            2 => Ok(Self::String),
            3 => Ok(Self::Number),
            4 => Ok(Self::Boolean),
            5 => Ok(Self::Null),
            _ => Err("Invalid discovery kind".into()),
        }
    }
}
pub struct Row {
    pub index: u64,
    pub label: String,
    pub kind: Kind,
    pub pointer: Option<String>,
    pub path: String,
    pub preview: String,
}
pub struct Page {
    pub start: u64,
    pub selected: u64,
    pub rows: Vec<Row>,
    pub selection: Row,
    pub detail: String,
    _lease: Reservation,
}
struct Record {
    name: String,
    kind: Kind,
    start: u64,
    end: u64,
    ordinal: u64,
    complete_name: bool,
}
struct Files {
    offsets: File,
    data: File,
    end: u64,
    disk: Reservation,
}
struct Directory {
    files: Mutex<Files>,
    count: AtomicU64,
}
fn temporary() -> Result<File, String> {
    for _ in 0..100 {
        let path = std::env::temp_dir().join(format!(
            "FerrisJson-top-{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options
                .share_mode(0)
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
                #[cfg(unix)]
                std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("Cannot create top-level directory".into())
}
impl Directory {
    fn new(budget: &BudgetManager) -> Result<Self, String> {
        Ok(Self {
            files: Mutex::new(Files {
                offsets: temporary()?,
                data: temporary()?,
                end: 0,
                disk: budget.reserve(Resource::Disk, 0, "top-level disk directory")?,
            }),
            count: AtomicU64::new(0),
        })
    }
    fn append(&self, name: &str, kind: Kind, start: u64, ordinal: u64) -> Result<u64, String> {
        let mut n = name.len().min(NAME);
        while !name.is_char_boundary(n) {
            n -= 1;
        }
        let mut raw = [0u8; HEADER + NAME];
        raw[0] = kind as u8;
        raw[1] = u8::from(n == name.len());
        raw[2..4].copy_from_slice(&(n as u16).to_le_bytes());
        raw[4..12].copy_from_slice(&start.to_le_bytes());
        raw[12..20].copy_from_slice(&UNKNOWN.to_le_bytes());
        raw[20..28].copy_from_slice(&ordinal.to_le_bytes());
        raw[HEADER..HEADER + n].copy_from_slice(&name.as_bytes()[..n]);
        let mut f = self.files.lock().unwrap();
        let index = self.count.load(Ordering::Relaxed);
        let offset = f.end;
        let end = offset
            .checked_add((HEADER + n) as u64)
            .ok_or("Discovery directory overflow")?;
        let bytes = end
            .checked_add(
                (index + 1)
                    .checked_mul(8)
                    .ok_or("Discovery count overflow")?,
            )
            .ok_or("Discovery size overflow")?;
        f.disk
            .resize(usize::try_from(bytes).map_err(|_| "Discovery size overflow")?)?;
        f.data
            .seek(SeekFrom::Start(offset))
            .and_then(|_| f.data.write_all(&raw[..HEADER + n]))
            .map_err(|e| e.to_string())?;
        f.offsets
            .seek(SeekFrom::Start(index * 8))
            .and_then(|_| f.offsets.write_all(&offset.to_le_bytes()))
            .map_err(|e| e.to_string())?;
        f.end = end;
        self.count.store(index + 1, Ordering::Release);
        Ok(index)
    }
    fn finish(&self, index: u64, end: u64) -> Result<(), String> {
        let mut f = self.files.lock().unwrap();
        let mut offset = [0; 8];
        f.offsets
            .seek(SeekFrom::Start(index * 8))
            .and_then(|_| f.offsets.read_exact(&mut offset))
            .map_err(|e| e.to_string())?;
        f.data
            .seek(SeekFrom::Start(u64::from_le_bytes(offset) + 12))
            .and_then(|_| f.data.write_all(&end.to_le_bytes()))
            .map_err(|e| e.to_string())
    }
    fn record(&self, index: u64) -> Result<Record, String> {
        if index >= self.count.load(Ordering::Acquire) {
            return Err("Discovery row unavailable".into());
        }
        let mut f = self.files.lock().unwrap();
        let mut offset = [0; 8];
        let mut raw = [0; HEADER];
        f.offsets
            .seek(SeekFrom::Start(index * 8))
            .and_then(|_| f.offsets.read_exact(&mut offset))
            .map_err(|e| e.to_string())?;
        f.data
            .seek(SeekFrom::Start(u64::from_le_bytes(offset)))
            .and_then(|_| f.data.read_exact(&mut raw))
            .map_err(|e| e.to_string())?;
        let n = u16::from_le_bytes(raw[2..4].try_into().unwrap()) as usize;
        if n > NAME {
            return Err("Invalid directory name".into());
        }
        let mut name = vec![0; n];
        f.data.read_exact(&mut name).map_err(|e| e.to_string())?;
        Ok(Record {
            name: String::from_utf8(name).map_err(|e| e.to_string())?,
            kind: Kind::decode(raw[0])?,
            start: u64::from_le_bytes(raw[4..12].try_into().unwrap()),
            end: u64::from_le_bytes(raw[12..20].try_into().unwrap()),
            ordinal: u64::from_le_bytes(raw[20..28].try_into().unwrap()),
            complete_name: raw[1] == 1,
        })
    }
}

struct Control {
    start: u64,
    selected: u64,
    revision: u64,
    delivered: u64,
    page: Option<Arc<Page>>,
    scanned: bool,
}
struct Shared {
    source: Arc<Source>,
    directory: Directory,
    control: Mutex<Control>,
    changed: Condvar,
    stop: AtomicBool,
    load_cancel: Arc<AtomicBool>,
    ctx: eframe::egui::Context,
    budget: BudgetManager,
    started: Instant,
    first_field: AtomicU64,
    scanned_bytes: AtomicU64,
    first_field_bytes: AtomicU64,
    _state: Reservation,
}
impl Shared {
    fn cancelled(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.load_cancel.load(Ordering::Relaxed)
    }
    fn changed(&self) {
        let mut c = self.control.lock().unwrap();
        c.revision = c.revision.wrapping_add(1);
        drop(c);
        self.changed.notify_all();
    }
}
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Statistics {
    pub rows: u64,
    pub first_field_us: Option<u64>,
    pub scanned_bytes: u64,
    pub first_field_scanned_bytes: Option<u64>,
    pub source_bytes: u64,
    pub scanning_finished: bool,
}
pub struct Discovery {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}
impl Discovery {
    pub fn start(
        source: Arc<Source>,
        load_cancel: Arc<AtomicBool>,
        ctx: eframe::egui::Context,
    ) -> Result<Self, String> {
        let budget = BudgetManager::new(Limits {
            memory: 8 * 1024 * 1024,
            disk: 1024 * 1024 * 1024,
        });
        let shared = Arc::new(Shared {
            source,
            directory: Directory::new(&budget)?,
            control: Mutex::new(Control {
                start: 0,
                selected: 0,
                revision: 1,
                delivered: 0,
                page: None,
                scanned: false,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            load_cancel,
            ctx,
            budget: budget.clone(),
            started: Instant::now(),
            first_field: AtomicU64::new(UNKNOWN),
            scanned_bytes: AtomicU64::new(0),
            first_field_bytes: AtomicU64::new(UNKNOWN),
            _state: budget.reserve(Resource::Memory, 64 * 1024, "top-level session state")?,
        });
        let scan = shared.clone();
        let scanner = std::thread::spawn(move || {
            let _ = discover(&scan);
            {
                scan.control.lock().unwrap().scanned = true;
            }
            scan.changed();
        });
        let read = shared.clone();
        let pager = std::thread::spawn(move || {
            loop {
                let (start, selected, _revision) = {
                    let mut c = read.control.lock().unwrap();
                    while c.revision == c.delivered && !read.cancelled() {
                        c = read
                            .changed
                            .wait_timeout(c, Duration::from_millis(25))
                            .unwrap()
                            .0;
                    }
                    if read.cancelled() {
                        return;
                    }
                    c.delivered = c.revision;
                    (c.start, c.selected, c.revision)
                };
                if read.directory.count.load(Ordering::Acquire) == 0 {
                    continue;
                }
                if let Ok(page) = page(&read, start, selected) {
                    let mut c = read.control.lock().unwrap();
                    if (c.start, c.selected) == (start, selected) && !read.cancelled() {
                        c.page = Some(Arc::new(page));
                        drop(c);
                        read.ctx.request_repaint();
                    }
                }
                std::thread::sleep(Duration::from_millis(16));
            }
        });
        Ok(Self {
            shared,
            threads: vec![scanner, pager],
        })
    }
    pub fn request(&self, start: u64, selected: u64) {
        let mut c = self.shared.control.lock().unwrap();
        if (c.start, c.selected) != (start, selected) {
            c.start = start;
            c.selected = selected;
            c.revision = c.revision.wrapping_add(1);
            drop(c);
            self.shared.changed.notify_all();
        }
    }
    pub fn poll(&self) -> Option<Arc<Page>> {
        self.shared.control.lock().unwrap().page.take()
    }
    pub fn budget(&self) -> BudgetManager {
        self.shared.budget.clone()
    }
    pub fn statistics(&self) -> Statistics {
        let n = self.shared.first_field.load(Ordering::Acquire);
        Statistics {
            rows: self.shared.directory.count.load(Ordering::Acquire),
            first_field_us: (n != UNKNOWN).then_some(n),
            scanned_bytes: self.shared.scanned_bytes.load(Ordering::Relaxed),
            first_field_scanned_bytes: (n != UNKNOWN)
                .then(|| self.shared.first_field_bytes.load(Ordering::Relaxed)),
            source_bytes: self.shared.source.bytes,
            scanning_finished: self.shared.control.lock().unwrap().scanned,
        }
    }
    pub fn wait_first(&self, timeout: Duration) {
        let c = self.shared.control.lock().unwrap();
        let _ = self.shared.changed.wait_timeout_while(c, timeout, |c| {
            self.shared.first_field.load(Ordering::Relaxed) == UNKNOWN
                && !c.scanned
                && !self.shared.cancelled()
        });
    }
    pub fn retire(mut self) -> JoinHandle<()> {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.changed.notify_all();
        let threads = std::mem::take(&mut self.threads);
        std::thread::spawn(move || {
            for t in threads {
                let _ = t.join();
            }
        })
    }
}
impl Drop for Discovery {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.changed.notify_all();
    }
}

struct Reader<'a> {
    shared: &'a Shared,
    buffer: Vec<u8>,
    start: u64,
    count: usize,
    pos: u64,
}
impl Reader<'_> {
    fn peek(&mut self) -> Result<Option<u8>, String> {
        if self.shared.cancelled() {
            return Err("Discovery cancelled".into());
        }
        if self.pos >= self.shared.source.bytes {
            return Ok(None);
        }
        if self.pos < self.start || self.pos >= self.start + self.count as u64 {
            self.start = self.pos;
            self.count = (self.shared.source.bytes - self.start).min(BLOCK as u64) as usize;
            self.shared
                .source
                .read(self.start, &mut self.buffer[..self.count], &|| {
                    self.shared.cancelled()
                })?;
            self.shared
                .scanned_bytes
                .fetch_max(self.start + self.count as u64, Ordering::Relaxed);
        }
        Ok(Some(self.buffer[(self.pos - self.start) as usize]))
    }
    fn next(&mut self) -> Result<u8, String> {
        let b = self.peek()?.ok_or("Unexpected end of JSON")?;
        self.pos += 1;
        Ok(b)
    }
    fn whitespace(&mut self) -> Result<(), String> {
        while matches!(self.peek()?, Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
        Ok(())
    }
    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.next()? != b {
            return Err("Invalid JSON structure".into());
        }
        Ok(())
    }
    fn string(&mut self, capture: bool) -> Result<Option<String>, String> {
        self.expect(b'"')?;
        let mut raw = if capture { vec![b'"'] } else { Vec::new() };
        let mut escaped = false;
        loop {
            let b = self.next()?;
            if capture {
                if raw.len() == BLOCK {
                    return Err("Discovery key exceeds 64 KiB; full loader will handle it".into());
                }
                raw.push(b);
            }
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                break;
            }
        }
        if capture {
            Ok(Some(
                serde_json::from_slice(&raw).map_err(|e| e.to_string())?,
            ))
        } else {
            Ok(None)
        }
    }
    fn skip(&mut self) -> Result<(), String> {
        match self.peek()?.ok_or("Expected value")? {
            b'"' => {
                self.string(false)?;
            }
            b'{' | b'[' => {
                let first = self.next()?;
                let mut stack = vec![if first == b'{' { b'}' } else { b']' }];
                while let Some(&close) = stack.last() {
                    match self.peek()?.ok_or("Unclosed JSON container")? {
                        b'"' => {
                            self.string(false)?;
                        }
                        b'{' | b'[' => {
                            if stack.len() >= 128 {
                                return Err("Discovery depth limit".into());
                            }
                            let b = self.next()?;
                            stack.push(if b == b'{' { b'}' } else { b']' });
                        }
                        b'}' | b']' => {
                            if self.next()? != close {
                                return Err("Mismatched JSON delimiter".into());
                            }
                            stack.pop();
                        }
                        _ => {
                            self.next()?;
                        }
                    }
                }
            }
            _ => {
                while self.peek()?.is_some_and(|b| {
                    !matches!(b, b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t')
                }) {
                    self.pos += 1;
                }
            }
        }
        Ok(())
    }
}
fn discover(shared: &Shared) -> Result<(), String> {
    let _memory = shared.budget.reserve(
        Resource::Memory,
        256 * 1024,
        "top-level lexical buffer and bounded key",
    )?;
    let mut r = Reader {
        shared,
        buffer: vec![0; BLOCK],
        start: 0,
        count: 0,
        pos: 0,
    };
    if r.peek()? == Some(0xef) {
        r.expect(0xef)?;
        r.expect(0xbb)?;
        r.expect(0xbf)?;
    }
    r.whitespace()?;
    let kind = Kind::from_byte(r.peek()?.ok_or("Empty input")?)?;
    shared.directory.append("$", kind, r.pos, UNKNOWN)?;
    shared.changed();
    if !kind.container() {
        r.skip()?;
        shared.directory.finish(0, r.pos)?;
        shared.changed();
        return Ok(());
    }
    r.next()?;
    r.whitespace()?;
    let close = if kind == Kind::Object { b'}' } else { b']' };
    let mut ordinal = 0;
    if r.peek()? == Some(close) {
        r.next()?;
        shared.directory.finish(0, r.pos)?;
        shared.changed();
        return Ok(());
    }
    loop {
        r.whitespace()?;
        let name = if kind == Kind::Object {
            let key = r.string(true)?.unwrap();
            r.whitespace()?;
            r.expect(b':')?;
            key
        } else {
            ordinal.to_string()
        };
        r.whitespace()?;
        let value_kind = Kind::from_byte(r.peek()?.ok_or("Expected value")?)?;
        let row = shared.directory.append(
            &name,
            value_kind,
            r.pos,
            if kind == Kind::Array {
                ordinal
            } else {
                UNKNOWN
            },
        )?;
        let us = shared
            .started
            .elapsed()
            .as_micros()
            .min(u128::from(UNKNOWN - 1)) as u64;
        if shared.first_field.load(Ordering::Relaxed) == UNKNOWN {
            shared.first_field_bytes.store(
                shared.scanned_bytes.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            shared.first_field.store(us, Ordering::Release);
        }
        shared.changed();
        r.skip()?;
        shared.directory.finish(row, r.pos)?;
        shared.changed();
        r.whitespace()?;
        match r.next()? {
            b if b == close => break,
            b',' => (),
            _ => return Err("Expected top-level separator".into()),
        }
        ordinal += 1;
    }
    shared.directory.finish(0, r.pos)?;
    shared.changed();
    Ok(())
}
fn preview(
    source: &Source,
    r: &Record,
    limit: usize,
    cancel: &dyn Fn() -> bool,
) -> Result<String, String> {
    if r.start > source.bytes || (r.end != UNKNOWN && (r.end < r.start || r.end > source.bytes)) {
        return Err("Invalid discovered value range".into());
    }
    if r.end == UNKNOWN && r.kind.container() {
        return Ok(if r.kind == Kind::Object {
            "{…}"
        } else {
            "[…]"
        }
        .into());
    }
    let end = if r.end == UNKNOWN {
        source.bytes
    } else {
        r.end
    };
    let n = (end - r.start).min(limit as u64) as usize;
    let mut data = vec![0; n];
    source.read(r.start, &mut data, cancel)?;
    if r.end == UNKNOWN {
        if r.kind == Kind::String {
            let mut escaped = false;
            for i in 1..data.len() {
                let b = data[i];
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    data.truncate(i + 1);
                    break;
                }
            }
        } else if let Some(i) = data
            .iter()
            .position(|b| matches!(b, b',' | b']' | b'}' | b' ' | b'\r' | b'\n' | b'\t'))
        {
            data.truncate(i);
        }
    }
    let mut text = String::from_utf8_lossy(&data).into_owned();
    if r.end == UNKNOWN || end - r.start > limit as u64 {
        text.push('…');
    }
    Ok(text)
}
fn row(
    source: &Source,
    directory: &Directory,
    index: u64,
    cancel: &dyn Fn() -> bool,
) -> Result<Row, String> {
    let r = directory.record(index)?;
    let pointer = if index == 0 {
        Some(String::new())
    } else if r.ordinal != UNKNOWN {
        Some(format!("/{}", r.ordinal))
    } else if r.complete_name {
        Some(format!("/{}", r.name.replace('~', "~0").replace('/', "~1")))
    } else {
        None
    };
    Ok(Row {
        index,
        label: r.name.chars().take(160).collect(),
        kind: r.kind,
        pointer,
        path: if index == 0 {
            "$".into()
        } else if r.ordinal != UNKNOWN {
            format!("$[{}]", r.ordinal)
        } else if r.complete_name && crate::document::is_identifier(&r.name) {
            format!("$.{}", r.name)
        } else {
            format!(
                "$[{}]{}",
                serde_json::to_string(&r.name).map_err(|e| e.to_string())?,
                if r.complete_name { "" } else { "…" }
            )
        },
        preview: preview(source, &r, 128, cancel)?,
    })
}
fn page(shared: &Shared, start: u64, selected: u64) -> Result<Page, String> {
    let lease = shared.budget.reserve(
        Resource::Memory,
        512 * 1024,
        "top-level viewport and selected prefix",
    )?;
    let count = shared.directory.count.load(Ordering::Acquire);
    let selected = selected.min(count - 1);
    let mut rows = Vec::with_capacity(WINDOW);
    for index in start..start.saturating_add(WINDOW as u64).min(count) {
        if shared.cancelled() {
            return Err("Discovery cancelled".into());
        }
        rows.push(row(&shared.source, &shared.directory, index, &|| {
            shared.cancelled()
        })?);
    }
    let selection = row(&shared.source, &shared.directory, selected, &|| {
        shared.cancelled()
    })?;
    let record = shared.directory.record(selected)?;
    let detail = preview(&shared.source, &record, 16 * 1024, &|| shared.cancelled())?;
    Ok(Page {
        start,
        selected,
        rows,
        selection,
        detail,
        _lease: lease,
    })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn final_loaders_read_the_original_handle_even_when_path_is_rebound() {
        use std::os::windows::fs::OpenOptionsExt;
        let input = Input::new(br#"{"old":1}"#);
        let file = OpenOptions::new()
            .read(true)
            .share_mode(7)
            .open(&input.0)
            .unwrap();
        let source = Source {
            bytes: file.metadata().unwrap().len(),
            file: FileSource::new(file),
        };
        let moved = input.0.with_extension("moved");
        std::fs::rename(&input.0, &moved).unwrap();
        std::fs::write(&input.0, br#"{"new":2}"#).unwrap();
        // Cloned handles share a cursor, so final readers must use explicit offsets.
        let mut cursor = source.duplicate_file().unwrap();
        cursor.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(source.read_text(&|| false).unwrap(), r#"{"old":1}"#);
        let doc = crate::document::Document::open_indexed_file(
            source.duplicate_file().unwrap(),
            || false,
            &crate::document::LoadProgress::default(),
        )
        .unwrap();
        assert!(doc.find_path("/old").is_ok());
        assert!(doc.find_path("/new").is_err());
        drop(doc);
        drop(cursor);
        drop(source);
        std::fs::remove_file(moved).unwrap();
    }
    struct Input(std::path::PathBuf);
    impl Input {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "FerrisJson-top-test-{}-{}.tmp",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }
    impl Drop for Input {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn start(input: &Input) -> Discovery {
        Discovery::start(
            Source::open(&input.0).unwrap(),
            Arc::new(AtomicBool::new(false)),
            eframe::egui::Context::default(),
        )
        .unwrap()
    }
    fn wait(discovery: &Discovery, ready: impl Fn(&Page) -> bool) -> Arc<Page> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(page) = discovery.poll()
                && ready(&page)
            {
                return page;
            }
            assert!(
                Instant::now() < until,
                "discovery page timed out: {:?}",
                discovery.statistics()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn nested_delimiters_escapes_duplicates_and_unicode_are_not_top_level_fields() {
        let input = Input::new(
            r#"{"a/b":{"x":["[","\\\"",{"z":"}"}]},"\u0061":1,"a":2,"空~":null}"#.as_bytes(),
        );
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&input.0).unwrap()).unwrap();
        let discovery = start(&input);
        let page = wait(&discovery, |p| p.rows.len() == 5);
        assert_eq!(
            page.rows
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            vec!["$", "a/b", "a", "a", "空~"]
        );
        assert_eq!(page.rows[1].pointer.as_deref(), Some("/a~1b"));
        assert_eq!(page.rows[4].pointer.as_deref(), Some("/空~0"));
        drop(page);
        discovery.retire().join().unwrap();
    }
    #[test]
    fn giant_first_value_publishes_field_before_end_and_releases_source_on_cancel() {
        let mut text = String::from("{\"first\":\"");
        text.extend(std::iter::repeat_n('x', 8 * 1024 * 1024));
        text.push_str("\",\"last\":1}");
        let input = Input::new(text.as_bytes());
        let discovery = start(&input);
        let budget = discovery.shared.budget.clone();
        assert_eq!(
            discovery.shared.source.read_text(&|| true).unwrap_err(),
            "Loading cancelled"
        );
        discovery.wait_first(Duration::from_secs(2));
        let stats = discovery.statistics();
        assert!(stats.first_field_us.is_some());
        assert_eq!(stats.first_field_scanned_bytes, Some(BLOCK as u64));
        assert!(OpenOptions::new().write(true).open(&input.0).is_err());
        assert!(std::fs::rename(&input.0, input.0.with_extension("moved")).is_err());
        discovery.request(0, 1);
        let page = wait(&discovery, |p| p.selected == 1);
        assert_eq!(page.selection.label, "first");
        assert_eq!(page.selection.kind, Kind::String);
        assert!(page.detail.len() <= 16 * 1024 + 3);
        drop(page);
        discovery.retire().join().unwrap();
        assert!(OpenOptions::new().write(true).open(&input.0).is_ok());
        assert_eq!(budget.usage().memory, 0);
        assert_eq!(budget.usage().disk, 0);
    }
    #[test]
    fn wide_top_level_uses_disk_pages_and_latest_viewport_request() {
        let text = format!(
            "[{}]",
            (0..10000)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let input = Input::new(text.as_bytes());
        let discovery = start(&input);
        let budget = discovery.shared.budget.clone();
        let until = Instant::now() + Duration::from_secs(5);
        while !discovery.statistics().scanning_finished {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(discovery.statistics().rows, 10001);
        discovery.request(64, 64);
        discovery.request(9990, 10000);
        let page = wait(&discovery, |p| p.start == 9990 && p.selected == 10000);
        assert!(page.rows.len() <= WINDOW);
        assert_eq!(page.selection.label, "9999");
        assert_eq!(page.selection.pointer.as_deref(), Some("/9999"));
        assert_eq!(page.detail, "9999");
        drop(page);
        discovery.retire().join().unwrap();
        assert_eq!(budget.usage().memory, 0);
        assert_eq!(budget.usage().disk, 0);
    }
    #[test]
    fn writer_conflict_empty_containers_scalar_and_large_names_are_bounded() {
        let input = Input::new(b"{}");
        let writer = OpenOptions::new().write(true).open(&input.0).unwrap();
        assert!(Source::open(&input.0).is_err());
        drop(writer);
        for bytes in [b"{}".as_slice(), b"[]", b"123", b"\xef\xbb\xbf[true,null]"] {
            let input = Input::new(bytes);
            let d = start(&input);
            let page = wait(&d, |p| !p.rows.is_empty());
            assert_eq!(page.rows[0].label, "$");
            drop(page);
            d.retire().join().unwrap();
        }
        let key = "汉".repeat(1000);
        let text = format!("{{{}:1}}", serde_json::to_string(&key).unwrap());
        let input = Input::new(text.as_bytes());
        let d = start(&input);
        let page = wait(&d, |p| p.rows.len() == 2);
        assert!(page.rows[1].label.chars().count() <= 160);
        assert!(page.rows[1].pointer.is_none());
        drop(page);
        d.retire().join().unwrap();
    }
}
