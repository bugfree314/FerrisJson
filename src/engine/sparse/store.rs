//! Private ephemeral index: explicit little-endian records and checksums, no struct dumps.
use super::{NONE, hash_bytes};
use crate::engine::{
    budget::{BudgetManager, Reservation, Resource},
    file_source::with_io_slot,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) const HEADER: u64 = 80;
const BUCKETS: u64 = 65_536;
const RECORD: u64 = 104;
const BASE: u64 = HEADER + BUCKETS * 16;
pub(super) type Record = [u64; 12];

pub(super) struct Store {
    file: Option<File>,
    path: Option<PathBuf>,
    len: u64,
    lease: Reservation,
}
impl Drop for Store {
    fn drop(&mut self) {
        self.file.take();
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}
impl Store {
    /// Immutable checkpoint copy. The caller reserves persistent disk space
    /// before entering; the scratch copy and live index remain separately charged.
    pub(super) fn save(
        &mut self,
        path: &std::path::Path,
        budget: &BudgetManager,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
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
    pub(super) fn restore(
        &mut self,
        path: &std::path::Path,
        expected: u64,
        budget: &BudgetManager,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut input = File::open(path).map_err(|e| e.to_string())?;
        if input.metadata().map_err(|e| e.to_string())?.len() != expected
            || expected < BASE
            || !(expected - BASE).is_multiple_of(RECORD)
        {
            return Err("Invalid checkpoint index length".into());
        }
        self.lease
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
    pub(super) fn new(
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
        let mut store = Self {
            file: Some(file),
            path: Some(path),
            len: BASE,
            lease,
        };
        #[cfg(unix)]
        if fs::remove_file(store.path.as_ref().unwrap()).is_ok() {
            store.path = None;
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
        &mut self,
        pos: u64,
        bytes: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        if pos
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err("Sparse index range out of bounds".into());
        }
        with_io_slot(cancelled, || {
            let file = self.file.as_mut().unwrap();
            file.seek(SeekFrom::Start(pos))?;
            file.read_exact(bytes)
        })
        .map_err(|e| e.to_string())
    }
    fn write_at(
        &mut self,
        pos: u64,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        with_io_slot(cancelled, || {
            let file = self.file.as_mut().unwrap();
            file.seek(SeekFrom::Start(pos))?;
            file.write_all(bytes)
        })
        .map_err(|e| e.to_string())
    }
    pub(super) fn bytes(&self) -> u64 {
        self.len
    }
    pub(super) fn head(&mut self, hash: u64, cancelled: &dyn Fn() -> bool) -> Result<u64, String> {
        let mut bytes = [0; 16];
        self.read_at(HEADER + hash % BUCKETS * 16, &mut bytes, cancelled)?;
        if hash_bytes(&bytes[..8]) != u64::from_le_bytes(bytes[8..].try_into().unwrap()) {
            return Err("Sparse bucket checksum mismatch".into());
        }
        Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
    }
    pub(super) fn set_head(
        &mut self,
        hash: u64,
        head: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&head.to_le_bytes());
        let sum = hash_bytes(&bytes[..8]);
        bytes[8..].copy_from_slice(&sum.to_le_bytes());
        self.write_at(HEADER + hash % BUCKETS * 16, &bytes, cancelled)
    }
    pub(super) fn append(
        &mut self,
        record: Record,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        let offset = self.len;
        let next = offset
            .checked_add(RECORD)
            .ok_or("Sparse index size overflow")?;
        self.lease.resize(
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
    pub(super) fn get(
        &mut self,
        offset: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Record, String> {
        if !self.valid_offset(offset) {
            return Err("Invalid sparse record offset".into());
        }
        let mut bytes = [0; RECORD as usize];
        self.read_at(offset, &mut bytes, cancelled)?;
        if hash_bytes(&bytes[..96]) != u64::from_le_bytes(bytes[96..].try_into().unwrap()) {
            return Err("Sparse record checksum mismatch".into());
        }
        Ok(std::array::from_fn(|n| {
            u64::from_le_bytes(bytes[n * 8..n * 8 + 8].try_into().unwrap())
        }))
    }
    pub(super) fn put(
        &mut self,
        offset: u64,
        record: Record,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        if !self.valid_offset(offset) {
            return Err("Invalid sparse record offset".into());
        }
        let mut bytes = [0; RECORD as usize];
        for (n, value) in record.iter().enumerate() {
            bytes[n * 8..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        let sum = hash_bytes(&bytes[..96]);
        bytes[96..].copy_from_slice(&sum.to_le_bytes());
        self.write_at(offset, &bytes, cancelled)
    }
    pub(super) fn commit(
        &mut self,
        words: [u64; 8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut bytes = [0; HEADER as usize];
        bytes[..8].copy_from_slice(b"FJSPRS01");
        for (n, word) in words.iter().enumerate() {
            bytes[8 + n * 8..16 + n * 8].copy_from_slice(&word.to_le_bytes());
        }
        let sum = hash_bytes(&bytes[..72]);
        bytes[72..].copy_from_slice(&sum.to_le_bytes());
        self.write_at(0, &bytes, cancelled)
    }
    pub(super) fn verify(
        &mut self,
        words: [u64; 8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut bytes = [0; HEADER as usize];
        self.read_at(0, &mut bytes, cancelled)?;
        if &bytes[..8] != b"FJSPRS01"
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
    pub(super) fn corrupt(&mut self, offset: u64) {
        self.write_at(offset, &[0xaa], &|| false).unwrap();
    }
}
