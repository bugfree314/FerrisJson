//! Private ephemeral index: explicit little-endian records and checksums, no struct dumps.
use super::{NONE, hash_bytes};
use crate::engine::{
    budget::{BudgetManager, Reservation, Resource},
    file_source::with_io_slot,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub(crate) const HEADER: u64 = 80;
const BUCKETS: u64 = 65_536;
pub(crate) const RECORD: u64 = 104;
const BASE: u64 = HEADER + BUCKETS * 16;
pub(crate) type Record = [u64; 12];

pub(crate) struct Store {
    memory: Option<super::memory_records::MemoryRecords>,
    write_cache: Option<Mutex<RecordCache>>,
    file: Option<File>,
    owner: Arc<Owner>,
    len: u64,
    readonly: bool,
    _directory_stack: Reservation,
    undo: Option<Undo>,
}
struct RecordCache {
    records: std::collections::HashMap<u64, (Record, bool)>,
    order: std::collections::VecDeque<u64>,
    _lease: Reservation,
}
struct Undo {
    initial_len: u64,
    records: Vec<(u64, Record)>,
    lease: Reservation,
}
struct Owner {
    path: Option<PathBuf>,
    disk: Mutex<Reservation>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        self.file.take();
    }
}
impl Store {
    /// Resident metadata for the progressive session; source content stays in its file.
    /// Virtual record offsets preserve IDs and the existing AVL/duplicate-key logic.
    pub(crate) fn memory(
        budget: &BudgetManager,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, String> {
        super::check(cancelled)?;
        Ok(Self {
            memory: Some(super::memory_records::MemoryRecords::new(
                budget,
                BUCKETS as usize,
            )?),
            write_cache: None,
            file: None,
            owner: Arc::new(Owner {
                path: None,
                disk: Mutex::new(budget.reserve(
                    Resource::Disk,
                    0,
                    "resident index uses no disk",
                )?),
            }),
            len: BASE,
            readonly: false,
            _directory_stack: budget.reserve(
                Resource::Memory,
                64 * 1024,
                "resident directory stack",
            )?,
            undo: None,
        })
    }

    fn require_file(&self) -> Result<(), String> {
        if self.memory.is_some() {
            Err(
                "Resident indexes do not support file views, persistence or rollback transactions"
                    .into(),
            )
        } else {
            Ok(())
        }
    }

    pub(crate) fn read_view(&self, budget: &BudgetManager) -> Result<Self, String> {
        self.require_file()?;
        if self.write_cache.is_some() {
            return Err("Live cached indexes cannot create external file views".into());
        }
        Ok(Self {
            memory: None,
            write_cache: None,
            file: Some(
                self.file
                    .as_ref()
                    .unwrap()
                    .try_clone()
                    .map_err(|e| e.to_string())?,
            ),
            owner: self.owner.clone(),
            len: self.len,
            readonly: true,
            _directory_stack: budget.reserve(
                Resource::Memory,
                64 * 1024,
                "independent index reader workspace",
            )?,
            undo: None,
        })
    }
    /// Immutable checkpoint copy. The caller reserves persistent disk space
    /// before entering; the scratch copy and live index remain separately charged.
    pub(crate) fn save(
        &mut self,
        path: &std::path::Path,
        budget: &BudgetManager,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        self.require_file()?;
        if self.write_cache.is_some() {
            return Err("Live cached indexes cannot be persisted".into());
        }
        let _lease = budget.reserve(Resource::Memory, 64 * 1024, "index checkpoint buffer")?;
        let mut options = OpenOptions::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        let mut bytes = vec![0; 64 * 1024];
        let mut pos = 0;
        while pos < self.len {
            super::check(cancelled)?;
            let n = bytes.len().min((self.len - pos) as usize);
            self.read_at(pos, &mut bytes[..n], cancelled)?;
            with_io_slot(cancelled, || output.write_all(&bytes[..n])).map_err(|e| e.to_string())?;
            pos += n as u64;
        }
        output.sync_all().map_err(|e| e.to_string())
    }
    pub(crate) fn restore(
        &mut self,
        path: &std::path::Path,
        expected: u64,
        budget: &BudgetManager,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        self.require_file()?;
        let mut input = File::open(path).map_err(|e| e.to_string())?;
        if input.metadata().map_err(|e| e.to_string())?.len() != expected
            || expected < BASE
            || !(expected - BASE).is_multiple_of(RECORD)
        {
            return Err("Invalid checkpoint index length".into());
        }
        self.owner
            .disk
            .lock()
            .unwrap()
            .resize(usize::try_from(expected).map_err(|_| "Index length overflow")?)?;
        let _lease = budget.reserve(Resource::Memory, 64 * 1024, "index restore buffer")?;
        let mut bytes = vec![0; 64 * 1024];
        self.len = expected;
        let mut pos = 0;
        while pos < expected {
            super::check(cancelled)?;
            let n = bytes.len().min((expected - pos) as usize);
            with_io_slot(cancelled, || input.read_exact(&mut bytes[..n]))
                .map_err(|e| e.to_string())?;
            self.write_at(pos, &bytes[..n], cancelled)?;
            pos += n as u64;
        }
        self.file
            .as_mut()
            .unwrap()
            .set_len(expected)
            .map_err(|e| e.to_string())?;
        // Check every bucket/record before trusting a restored generation.
        for bucket in 0..BUCKETS {
            self.head(bucket, cancelled)?;
        }
        let mut offset = BASE;
        while offset < expected {
            self.get(offset, cancelled)?;
            offset += RECORD;
        }
        Ok(())
    }
    pub(crate) fn new(
        budget: &BudgetManager,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, String> {
        let lease = budget.reserve(
            Resource::Disk,
            BASE as usize,
            "sparse index header and buckets",
        )?;
        let _buffer = budget.reserve(Resource::Memory, 64 * 1024, "sparse index initialization")?;
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let mut result = None;
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "FerrisJson-sparse-{}-{}.tmp",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options
                    .share_mode(0)
                    .access_mode(0xc0010000)
                    .custom_flags(0x04000000);
            }
            match options.open(&path) {
                Ok(file) => {
                    result = Some((file, path));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.to_string()),
            }
        }
        let (file, path) = result.ok_or("Cannot create sparse index")?;
        #[allow(unused_mut)] // Unix unlinks the owner path after opening.
        let mut store = Self {
            memory: None,
            write_cache: None,
            file: Some(file),
            owner: Arc::new(Owner {
                path: Some(path),
                disk: Mutex::new(lease),
            }),
            len: BASE,
            readonly: false,
            _directory_stack: budget.reserve(
                Resource::Memory,
                64 * 1024,
                "bounded directory operation stack",
            )?,
            undo: None,
        };
        #[cfg(unix)]
        if fs::remove_file(store.owner.path.as_ref().unwrap()).is_ok() {
            Arc::get_mut(&mut store.owner).unwrap().path = None;
        }
        store.write_at(0, &[0; HEADER as usize], cancelled)?;
        let mut buffer = vec![0; 64 * 1024];
        for chunk in buffer.as_chunks_mut::<16>().0 {
            chunk[..8].copy_from_slice(&NONE.to_le_bytes());
            let sum = hash_bytes(&chunk[..8]);
            chunk[8..].copy_from_slice(&sum.to_le_bytes());
        }
        let mut pos = HEADER;
        while pos < BASE {
            store.write_at(pos, &buffer, cancelled)?;
            pos += buffer.len() as u64;
        }
        Ok(store)
    }
    fn read_at(
        &self,
        pos: u64,
        bytes: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        self.require_file()?;
        if pos
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err("Sparse index range out of bounds".into());
        }
        let file = self.file.as_ref().unwrap();
        let mut offset = pos;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            super::check(cancelled)?;
            let n = with_io_slot(cancelled, || {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileExt;
                    file.seek_read(remaining, offset)
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    file.read_at(remaining, offset)
                }
                #[cfg(not(any(unix, windows)))]
                {
                    Err(std::io::Error::other("Positioned index reads unavailable"))
                }
            })
            .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("Index truncated".into());
            }
            offset += n as u64;
            remaining = &mut remaining[n..];
        }
        Ok(())
    }
    fn write_at(&self, pos: u64, bytes: &[u8], cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        self.require_file()?;
        if self.readonly {
            return Err("Read-only index view".into());
        }

        let file = self.file.as_ref().unwrap();
        let mut done = 0;
        while done < bytes.len() {
            let n = with_io_slot(cancelled, || {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileExt;
                    file.seek_write(&bytes[done..], pos + done as u64)
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    file.write_at(&bytes[done..], pos + done as u64)
                }
                #[cfg(not(any(windows, unix)))]
                {
                    Err(std::io::Error::other("Positioned index writes unavailable"))
                }
            })
            .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("Index write made no progress".into());
            }
            done += n;
        }
        Ok(())
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.len
    }
    pub(crate) fn head(&self, hash: u64, cancelled: &dyn Fn() -> bool) -> Result<u64, String> {
        super::check(cancelled)?;
        if let Some(memory) = &self.memory {
            return Ok(memory.head(hash));
        }
        let mut bytes = [0; 16];
        self.read_at(HEADER + hash % BUCKETS * 16, &mut bytes, cancelled)?;
        if hash_bytes(&bytes[..8]) != u64::from_le_bytes(bytes[8..].try_into().unwrap()) {
            return Err("Sparse bucket checksum mismatch".into());
        }
        Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
    }
    pub(crate) fn set_head(
        &mut self,
        hash: u64,
        head: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        super::check(cancelled)?;
        if let Some(memory) = &mut self.memory {
            memory.set_head(hash, head);
            return Ok(());
        }
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&head.to_le_bytes());
        let sum = hash_bytes(&bytes[..8]);
        bytes[8..].copy_from_slice(&sum.to_le_bytes());
        self.write_at(HEADER + hash % BUCKETS * 16, &bytes, cancelled)
    }
    pub(crate) fn append(
        &mut self,
        record: Record,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        super::check(cancelled)?;
        let offset = self.len;
        let next = offset
            .checked_add(RECORD)
            .ok_or("Sparse index size overflow")?;
        if let Some(memory) = &mut self.memory {
            memory.append(record)?;
            self.len = next;
            return Ok(offset);
        }
        self.owner.disk.lock().unwrap().resize(
            usize::try_from(next).map_err(|_| "Sparse index exceeds platform disk accounting")?,
        )?;
        self.len = next;
        self.put(offset, record, cancelled)?;
        Ok(offset)
    }
    fn valid_offset(&self, offset: u64) -> bool {
        offset >= BASE
            && (offset - BASE).is_multiple_of(RECORD)
            && offset
                .checked_add(RECORD)
                .is_some_and(|end| end <= self.len)
    }
    pub(crate) fn get(&self, offset: u64, cancelled: &dyn Fn() -> bool) -> Result<Record, String> {
        super::check(cancelled)?;
        if !self.valid_offset(offset) {
            return Err("Invalid sparse record offset".into());
        }
        if let Some(memory) = &self.memory {
            return memory.get((offset - BASE) / RECORD);
        }
        if let Some(cache) = &self.write_cache
            && let Some((r, _)) = cache.lock().unwrap().records.get(&offset)
        {
            super::check(cancelled)?;
            return Ok(*r);
        }
        let mut bytes = [0; RECORD as usize];
        self.read_at(offset, &mut bytes, cancelled)?;
        if hash_bytes(&bytes[..96]) != u64::from_le_bytes(bytes[96..].try_into().unwrap()) {
            return Err("Sparse record checksum mismatch".into());
        }
        let record = std::array::from_fn(|n| {
            u64::from_le_bytes(bytes[n * 8..n * 8 + 8].try_into().unwrap())
        });
        self.cache_record(offset, record, false, cancelled)?;
        Ok(record)
    }
    pub(crate) fn put(
        &mut self,
        offset: u64,
        record: Record,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        super::check(cancelled)?;
        if self.readonly {
            return Err("Read-only index view".into());
        }
        if !self.valid_offset(offset) {
            return Err("Invalid sparse record offset".into());
        }
        if let Some(memory) = &mut self.memory {
            return memory.put((offset - BASE) / RECORD, record);
        }
        if self
            .undo
            .as_ref()
            .is_some_and(|log| offset < log.initial_len)
        {
            let previous = self.get(offset, cancelled)?;
            let log = self.undo.as_mut().unwrap();
            if log.records.len() * std::mem::size_of::<(u64, Record)>() >= 8 * 1024 * 1024 {
                return Err("Tree state undo budget exceeded".into());
            }
            crate::engine::budget::reserve_vec(&mut log.records, 1, &mut log.lease)?;
            log.records.push((offset, previous));
        }
        if self.write_cache.is_some() {
            return self.cache_record(offset, record, true, cancelled);
        }
        self.write_record(offset, record, cancelled)
    }
    fn write_record(
        &self,
        offset: u64,
        record: Record,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut bytes = [0; RECORD as usize];
        for (n, value) in record.iter().enumerate() {
            bytes[n * 8..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        let sum = hash_bytes(&bytes[..96]);
        bytes[96..].copy_from_slice(&sum.to_le_bytes());
        self.write_at(offset, &bytes, cancelled)
    }
    /// Only for a single live ephemeral owner. No external file views or
    /// persistence are used by this owner; dirty entries may die with it.
    pub(crate) fn enable_live_cache(&mut self, budget: &BudgetManager) -> Result<(), String> {
        self.require_file()?;
        let lease = budget.reserve(
            Resource::Memory,
            4 * 1024 * 1024,
            "bounded live index record cache",
        )?;
        self.write_cache = Some(Mutex::new(RecordCache {
            records: std::collections::HashMap::with_capacity(8192),
            order: std::collections::VecDeque::with_capacity(8192),
            _lease: lease,
        }));
        Ok(())
    }
    fn cache_record(
        &self,
        offset: u64,
        record: Record,
        dirty: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let Some(cache) = &self.write_cache else {
            return Ok(());
        };
        super::check(cancelled)?;
        let mut c = cache.lock().unwrap();
        if let Some(old) = c.records.get_mut(&offset) {
            *old = (record, old.1 || dirty);
            return Ok(());
        }
        if c.records.len() == 8192 {
            let victim = *c.order.front().unwrap();
            let (r, modified) = c.records[&victim];
            if modified {
                self.write_record(victim, r, cancelled)?;
            }
            c.order.pop_front();
            c.records.remove(&victim);
        }
        c.order.push_back(offset);
        c.records.insert(offset, (record, dirty));
        Ok(())
    }
    pub(crate) fn in_transaction(&self) -> bool {
        self.undo.is_some()
    }
    pub(crate) fn transaction(&mut self, budget: &BudgetManager) -> Result<(), String> {
        self.require_file()?;
        if self.write_cache.is_some() {
            return Err("Live cached indexes do not use rollback transactions".into());
        }
        if self.undo.is_some() {
            return Err("Nested tree transaction".into());
        }
        self.undo = Some(Undo {
            initial_len: self.len,
            records: Vec::new(),
            lease: budget.reserve(Resource::Memory, 0, "tree state undo log")?,
        });
        Ok(())
    }
    pub(crate) fn finish_transaction(&mut self, commit: bool) -> Result<(), String> {
        let Some(log) = self.undo.take() else {
            return Err("No tree transaction".into());
        };
        if commit {
            return Ok(());
        }
        crate::engine::file_source::with_io_context(
            crate::engine::file_source::Priority::Interactive,
            crate::engine::file_source::Cancellation::Any(Vec::new()),
            || {
                for (id, record) in log.records.iter().rev() {
                    self.put(*id, *record, &|| false)?;
                }
                self.file
                    .as_ref()
                    .unwrap()
                    .set_len(log.initial_len)
                    .map_err(|e| e.to_string())?;
                self.len = log.initial_len;
                self.owner
                    .disk
                    .lock()
                    .unwrap()
                    .resize(log.initial_len as usize)
            },
        )
    }
    pub(crate) fn fork(
        &self,
        budget: &BudgetManager,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, String> {
        self.require_file()?;
        if self.write_cache.is_some() {
            return Err("Live cached indexes cannot create external file copies".into());
        }
        let mut copy = Self::new(budget, cancelled)?;
        copy.owner.disk.lock().unwrap().resize(self.len as usize)?;
        copy.len = self.len;
        let _memory = budget.reserve(Resource::Memory, 64 * 1024, "immutable index view copy")?;
        let mut bytes = vec![0; 64 * 1024];
        let mut pos = 0;
        while pos < self.len {
            super::super::file_source::check_cancelled().map_err(|e| e.to_string())?;
            let n = (self.len - pos).min(bytes.len() as u64) as usize;
            self.read_at(pos, &mut bytes[..n], cancelled)?;
            copy.write_at(pos, &bytes[..n], cancelled)?;
            pos += n as u64;
        }
        Ok(copy)
    }
    pub(crate) fn commit(
        &mut self,
        words: [u64; 8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut bytes = [0; HEADER as usize];
        bytes[..8].copy_from_slice(b"FJSPRS02");
        for (n, word) in words.iter().enumerate() {
            bytes[8 + n * 8..16 + n * 8].copy_from_slice(&word.to_le_bytes());
        }
        let sum = hash_bytes(&bytes[..72]);
        bytes[72..].copy_from_slice(&sum.to_le_bytes());
        self.write_at(0, &bytes, cancelled)
    }
    pub(crate) fn verify(
        &self,
        words: [u64; 8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut bytes = [0; HEADER as usize];
        self.read_at(0, &mut bytes, cancelled)?;
        if &bytes[..8] != b"FJSPRS02"
            || hash_bytes(&bytes[..72]) != u64::from_le_bytes(bytes[72..].try_into().unwrap())
        {
            return Err("Invalid sparse index header".into());
        }
        for (n, word) in words.iter().enumerate() {
            if u64::from_le_bytes(bytes[8 + n * 8..16 + n * 8].try_into().unwrap()) != *word {
                return Err("Sparse index state/version mismatch".into());
            }
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn corrupt(&mut self, offset: u64) {
        self.write_at(offset, &[0xaa], &|| false).unwrap();
    }
}
