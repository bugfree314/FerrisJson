//! Two immutable generations. A torn manifest slot never destroys the other
//! commit. Index mutations are confined to the private working Store.
use super::{check, hash_bytes, progressive::Checkpoint};
use crate::document::indexed::Snapshot;
use crate::engine::{
    budget::{BudgetManager, Reservation, Resource},
    file_source::with_io_slot,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // The binding is mutable on Unix; keep the same return path on Windows.
    let _ = &mut options;
    options
}

const FILES: [&str; 9] = [
    "source.bin",
    "source.tmp",
    "manifest.bin",
    "index-0.bin",
    "index-1.bin",
    "state-0.bin",
    "state-1.bin",
    "index.tmp",
    "state.tmp",
];
#[cfg(test)]
pub(super) static FAULT: std::thread::LocalKey<std::cell::Cell<u8>> = {
    std::thread_local! { static VALUE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }
    VALUE
};
fn fault(stage: u8) -> Result<(), String> {
    #[cfg(test)]
    if FAULT.with(|f| f.get() == stage) {
        return Err(format!("Injected checkpoint write failure {stage}"));
    }
    let _ = stage;
    Ok(())
}
pub(super) struct Cache {
    dir: PathBuf,
    _lock: File,
    disk: Reservation,
    budget: BudgetManager,
    sequence: u64,
    slot: usize,
}
impl Cache {
    pub(super) fn open(dir: &Path, budget: &BudgetManager) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        for name in FILES.into_iter().chain(["writer.lock", "owner.bin"]) {
            if fs::symlink_metadata(dir.join(name))
                .is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
            {
                return Err("Cache contains a non-regular managed file".into());
            }
        }
        let lock = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("writer.lock"))
            .map_err(|e| e.to_string())?;
        lock.try_lock()
            .map_err(|_| "Cache is already in use by another session")?;
        let owner = dir.join("owner.bin");
        if owner.exists() {
            let mut marker = File::open(&owner).map_err(|e| e.to_string())?;
            let mut bytes = [0; 17];
            marker
                .read_exact(&mut bytes)
                .map_err(|_| "Invalid cache ownership marker")?;
            if &bytes != b"FerrisJsonS4Cache" {
                return Err("Not a FerrisJson S4 cache directory".into());
            }
        } else if FILES.iter().any(|name| dir.join(name).exists()) {
            return Err("Cache directory has managed filenames without an ownership marker; choose an empty directory".into());
        }
        let mut bytes = 17usize;
        for name in FILES {
            if let Ok(m) = fs::metadata(dir.join(name)) {
                bytes = bytes
                    .checked_add(usize::try_from(m.len()).map_err(|_| "Cache size overflow")?)
                    .ok_or("Cache size overflow")?;
            }
        }
        let disk = budget.reserve(Resource::Disk, bytes, "persistent cache files")?;
        if !owner.exists() {
            let mut marker = private_options()
                .write(true)
                .create_new(true)
                .open(owner)
                .map_err(|e| e.to_string())?;
            marker
                .write_all(b"FerrisJsonS4Cache")
                .and_then(|_| marker.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            dir: dir.into(),
            _lock: lock,
            disk,
            budget: budget.clone(),
            sequence: 0,
            slot: 1,
        })
    }
    fn remove(&mut self, name: &str) -> Result<(), String> {
        let path = self.dir.join(name);
        match fs::metadata(&path) {
            Ok(m) => {
                fs::remove_file(path).map_err(|e| e.to_string())?;
                self.disk
                    .resize(self.disk.bytes().saturating_sub(m.len() as usize))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
        Ok(())
    }
    pub(super) fn reset(&mut self) -> Result<(), String> {
        for name in FILES {
            self.remove(name)?;
        }
        self.sequence = 0;
        self.slot = 1;
        Ok(())
    }
    pub(super) fn source_path(&self) -> PathBuf {
        self.dir.join("source.bin")
    }
    pub(super) fn detach_source(&mut self) -> Result<Reservation, String> {
        let bytes = fs::metadata(self.source_path())
            .map_err(|e| e.to_string())?
            .len() as usize;
        let mut lease =
            self.budget
                .reserve(Resource::Disk, 0, "shared persistent source snapshot")?;
        self.disk.resize(
            self.disk
                .bytes()
                .checked_sub(bytes)
                .ok_or("Cache source accounting mismatch")?,
        )?;
        lease.resize(bytes)?;
        Ok(lease)
    }
    /// Byte-for-byte equality is stronger than metadata or sampling, and costs
    /// one full source/cache read. The cache snapshot stays open for the session.
    pub(super) fn matches(
        &self,
        source: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool, String> {
        let _memory =
            self.budget
                .reserve(Resource::Memory, 128 * 1024, "strict cache comparison")?;
        let mut options = private_options();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1);
        }
        let mut input = options.open(source).map_err(|e| e.to_string())?;
        let before = input.metadata().map_err(|e| e.to_string())?;
        let mut cached = match File::open(self.source_path()) {
            Ok(f) => f,
            Err(_) => return Ok(false),
        };
        if cached.metadata().map_err(|e| e.to_string())?.len() != before.len() {
            return Ok(false);
        }
        let mut a = vec![0; 64 * 1024];
        let mut b = vec![0; 64 * 1024];
        let mut left = before.len();
        while left != 0 {
            check(cancelled)?;
            let n = left.min(a.len() as u64) as usize;
            with_io_slot(cancelled, || {
                input.read_exact(&mut a[..n])?;
                cached.read_exact(&mut b[..n])
            })
            .map_err(|e| e.to_string())?;
            if a[..n] != b[..n] {
                return Ok(false);
            }
            left -= n as u64;
        }
        let after = input.metadata().map_err(|e| e.to_string())?;
        Ok(before.len() == after.len() && before.modified().ok() == after.modified().ok())
    }
    pub(super) fn save_source(
        &mut self,
        source: &Snapshot,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        self.remove("source.tmp")?;
        self.disk.resize(
            self.disk
                .bytes()
                .checked_add(usize::try_from(source.len).map_err(|_| "Cache source overflow")?)
                .ok_or("Cache disk overflow")?,
        )?;
        let _memory = self
            .budget
            .reserve(Resource::Memory, 64 * 1024, "cache snapshot copy")?;
        let mut out = private_options()
            .write(true)
            .create_new(true)
            .open(self.dir.join("source.tmp"))
            .map_err(|e| e.to_string())?;
        let mut bytes = vec![0; 64 * 1024];
        let mut pos = 0;
        while pos < source.len {
            check(cancelled)?;
            let n = (source.len - pos).min(bytes.len() as u64) as usize;
            source
                .read_cancellable(pos, &mut bytes[..n], cancelled)
                .map_err(|e| e.to_string())?;
            with_io_slot(cancelled, || out.write_all(&bytes[..n])).map_err(|e| e.to_string())?;
            pos += n as u64;
        }
        out.sync_all().map_err(|e| e.to_string())?;
        drop(out);
        fs::rename(self.dir.join("source.tmp"), self.source_path()).map_err(|e| e.to_string())?;
        self.sync_dir()
    }
    fn sync_dir(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            File::open(&self.dir)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    pub(super) fn load(
        &mut self,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<Checkpoint>, String> {
        check(cancelled)?;
        let mut manifest = match File::open(self.dir.join("manifest.bin")) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        let mut bytes = [0; 64];
        if manifest.read_exact(&mut bytes).is_err() {
            return Ok(None);
        }
        let mut entries = [(0u64, 0usize, 0u64); 2];
        for (slot, entry) in entries.iter_mut().enumerate() {
            let b = &bytes[slot * 32..slot * 32 + 32];
            if &b[..8] == b"FJCM0002"
                && hash_bytes(&b[..24]) == u64::from_le_bytes(b[24..].try_into().unwrap())
            {
                *entry = (
                    u64::from_le_bytes(b[8..16].try_into().unwrap()),
                    slot,
                    u64::from_le_bytes(b[16..24].try_into().unwrap()),
                );
            }
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.0));
        for (seq, slot, sum) in entries {
            if seq == 0 {
                continue;
            }
            check(cancelled)?;
            let path = self.dir.join(format!("state-{slot}.bin"));
            if !fs::metadata(&path).is_ok_and(|m| m.len() <= 64 * 1024) {
                continue;
            }
            let _lease =
                self.budget
                    .reserve(Resource::Memory, 64 * 1024, "cache checkpoint decode")?;
            let state = match fs::read(path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if hash_bytes(&state) != sum {
                continue;
            }
            if let Ok(checkpoint) = Checkpoint::decode(&state) {
                self.sequence = seq;
                self.slot = slot;
                return Ok(Some(checkpoint));
            }
        }
        Ok(None)
    }
    pub(super) fn restore(
        &self,
        store: &mut super::Store,
        state: &Checkpoint,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        store.restore(
            &self.dir.join(format!("index-{}.bin", self.slot)),
            state.index_len,
            &self.budget,
            cancelled,
        )
    }
    pub(super) fn commit(
        &mut self,
        store: &mut super::Store,
        state: &Checkpoint,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        check(cancelled)?;
        let slot = 1 - self.slot;
        let index_name = format!("index-{slot}.bin");
        let state_name = format!("state-{slot}.bin");
        self.remove(&index_name)?;
        self.remove(&state_name)?;
        self.remove("index.tmp")?;
        self.remove("state.tmp")?;
        let _memory =
            self.budget
                .reserve(Resource::Memory, 64 * 1024, "cache checkpoint encode")?;
        let bytes = state.encode();
        let added = usize::try_from(store.bytes())
            .map_err(|_| "Index cache size overflow")?
            .checked_add(bytes.len())
            .ok_or("Cache disk overflow")?;
        self.disk.resize(
            self.disk
                .bytes()
                .checked_add(added)
                .ok_or("Cache disk overflow")?,
        )?;
        store.save(&self.dir.join("index.tmp"), &self.budget, cancelled)?;
        fault(1)?;
        let mut output = private_options()
            .write(true)
            .create_new(true)
            .open(self.dir.join("state.tmp"))
            .map_err(|e| e.to_string())?;
        output
            .write_all(&bytes)
            .and_then(|_| output.sync_all())
            .map_err(|e| e.to_string())?;
        drop(output);
        fault(2)?;
        fs::rename(self.dir.join("index.tmp"), self.dir.join(index_name))
            .map_err(|e| e.to_string())?;
        fault(3)?;
        fs::rename(self.dir.join("state.tmp"), self.dir.join(state_name))
            .map_err(|e| e.to_string())?;
        fault(4)?;
        self.sync_dir()?;
        let seq = self
            .sequence
            .checked_add(1)
            .ok_or("Cache commit sequence overflow")?;
        let mut entry = [0u8; 32];
        entry[..8].copy_from_slice(b"FJCM0002");
        entry[8..16].copy_from_slice(&seq.to_le_bytes());
        entry[16..24].copy_from_slice(&hash_bytes(&bytes).to_le_bytes());
        let sum = hash_bytes(&entry[..24]);
        entry[24..].copy_from_slice(&sum.to_le_bytes());
        let path = self.dir.join("manifest.bin");
        let existed = path.exists();
        if !existed {
            self.disk.resize(
                self.disk
                    .bytes()
                    .checked_add(64)
                    .ok_or("Cache disk overflow")?,
            )?;
        }
        let mut manifest = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| e.to_string())?;
        if !existed {
            manifest.write_all(&[0; 64]).map_err(|e| e.to_string())?;
        }
        manifest
            .seek(SeekFrom::Start((slot * 32) as u64))
            .map_err(|e| e.to_string())?;
        manifest
            .write_all(&entry[..16])
            .map_err(|e| e.to_string())?;
        fault(5)?;
        manifest
            .write_all(&entry[16..])
            .and_then(|_| manifest.sync_all())
            .map_err(|e| e.to_string())?;
        self.sync_dir()?;
        self.sequence = seq;
        self.slot = slot;
        Ok(())
    }
    pub(super) fn sequence(&self) -> u64 {
        self.sequence
    }
}
