//! Immutable disk tables with bounded decoded-page caches. No memory mapping.
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

const PAGE_BYTES: usize = 64 * 1024;
static SERIAL: AtomicU64 = AtomicU64::new(0);

pub(super) trait Record: Copy {
    const WIDTH: usize;
    fn encode(self, bytes: &mut [u8]);
    fn decode(bytes: &[u8]) -> Result<Self, String>;
}
impl Record for u32 {
    const WIDTH: usize = 4;
    fn encode(self, bytes: &mut [u8]) {
        bytes.copy_from_slice(&self.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        Ok(u32::from_le_bytes(
            bytes.try_into().map_err(|_| "Invalid edge record")?,
        ))
    }
}
struct State<T> {
    file: Option<File>,
    pages: HashMap<usize, (Vec<T>, u64)>,
    clock: u64,
    bytes: usize,
}
pub(super) struct Paged<T> {
    state: Mutex<State<T>>,
    path: Option<PathBuf>,
    len: usize,
    budget: usize,
}
impl<T> Drop for Paged<T> {
    fn drop(&mut self) {
        self.state
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .file
            .take();
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}
impl<T: Record> Paged<T> {
    pub(super) fn create(
        values: Vec<T>,
        budget: usize,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Self, String> {
        let mut result = None;
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "FerrisJson-index-{}-{}.tmp",
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
                    .access_mode(0x80000000 | 0x40000000 | 0x00010000)
                    .custom_flags(0x04000000);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    result = Some(Self {
                        state: Mutex::new(State {
                            file: Some(file),
                            pages: HashMap::new(),
                            clock: 0,
                            bytes: 0,
                        }),
                        path: Some(path),
                        len: values.len(),
                        budget,
                    });
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("Cannot create index table: {e}")),
            }
        }
        let mut table = result.ok_or("Cannot create index table")?;
        #[cfg(unix)]
        if fs::remove_file(table.path.as_ref().unwrap()).is_ok() {
            table.path = None;
        }
        let state = table
            .state
            .get_mut()
            .map_err(|_| "Index table lock failed")?;
        let file = state.file.as_mut().unwrap();
        let per_page = PAGE_BYTES / T::WIDTH;
        let mut buffer = vec![0; per_page * T::WIDTH];
        for chunk in values.chunks(per_page) {
            if cancelled() {
                return Err("Loading cancelled".into());
            }
            for (&value, bytes) in chunk.iter().zip(buffer.chunks_exact_mut(T::WIDTH)) {
                value.encode(bytes);
            }
            file.write_all(&buffer[..chunk.len() * T::WIDTH])
                .map_err(|e| format!("Cannot write index table: {e}"))?;
        }
        file.flush()
            .map_err(|e| format!("Cannot flush index table: {e}"))?;
        // Ownership of the original full array ends here, before the next table is written.
        drop(values);
        Ok(table)
    }
    pub(super) fn len(&self) -> usize {
        self.len
    }
    pub(super) fn disk_bytes(&self) -> u64 {
        self.len as u64 * T::WIDTH as u64
    }
    pub(super) fn cache_bytes(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).bytes
    }
    #[cfg(test)]
    pub(super) fn test_budget(&mut self, bytes: usize) {
        self.budget = bytes;
        let state = self.state.get_mut().unwrap();
        state.pages.clear();
        state.bytes = 0;
    }
    #[cfg(test)]
    pub(super) fn fail_reads(&mut self) {
        let state = self.state.get_mut().unwrap();
        state.pages.clear();
        state.bytes = 0;
        state.file.as_mut().unwrap().set_len(0).unwrap();
    }
    pub(super) fn get(&self, id: usize) -> Result<T, String> {
        if id >= self.len {
            return Err("Invalid index record ID".into());
        }
        let per_page = PAGE_BYTES / T::WIDTH;
        let page = id / per_page;
        let mut state = self.state.lock().map_err(|_| "Index table lock failed")?;
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        if let Some((records, stamp)) = state.pages.get_mut(&page) {
            *stamp = clock;
            return Ok(records[id % per_page]);
        }
        let count = per_page.min(self.len - page * per_page);
        let mut bytes = vec![0; count * T::WIDTH];
        let file = state.file.as_mut().ok_or("Index table is closed")?;
        file.seek(SeekFrom::Start(
            page as u64 * per_page as u64 * T::WIDTH as u64,
        ))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|e| format!("Cannot read index page: {e}"))?;
        let mut records = Vec::with_capacity(count);
        for bytes in bytes.chunks_exact(T::WIDTH) {
            records.push(T::decode(bytes)?);
        }
        let cost = records.capacity() * std::mem::size_of::<T>();
        if cost > self.budget {
            return Err("Index page exceeds cache budget".into());
        }
        while state.bytes + cost > self.budget {
            let victim = *state
                .pages
                .iter()
                .min_by_key(|(_, (_, stamp))| stamp)
                .ok_or("Invalid page cache budget")?
                .0;
            let (old, _) = state.pages.remove(&victim).unwrap();
            state.bytes -= old.capacity() * std::mem::size_of::<T>();
        }
        let value = records[id % per_page];
        state.bytes += cost;
        state.pages.insert(page, (records, clock));
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_evict_reload_and_propagate_io_errors() {
        let values: Vec<u32> = (0..100_000).collect();
        let table = Paged::create(values, PAGE_BYTES * 2, &|| false).unwrap();
        for id in [0, 16_384, 32_768, 99_999, 0] {
            assert_eq!(table.get(id).unwrap(), id as u32);
        }
        assert!(table.cache_bytes() <= PAGE_BYTES * 2);
        assert_eq!(table.disk_bytes(), 400_000);
        assert!(table.get(100_000).is_err());
        {
            let mut state = table.state.lock().unwrap();
            state.pages.clear();
            state.bytes = 0;
            state.file.as_mut().unwrap().set_len(0).unwrap();
        }
        assert!(table.get(0).unwrap_err().contains("Cannot read index page"));
    }
    #[test]
    fn cancellation_cleans_new_tables() {
        assert!(
            Paged::create(vec![0u32; 100], PAGE_BYTES, &|| true)
                .err()
                .unwrap()
                .contains("cancelled")
        );
        let writes = std::cell::Cell::new(0);
        assert!(
            Paged::create(vec![0u32; 40_000], PAGE_BYTES, &|| {
                writes.set(writes.get() + 1);
                writes.get() == 2
            })
            .err()
            .unwrap()
            .contains("cancelled")
        );
        assert_eq!(writes.get(), 2);
        let table = Paged::create(vec![1u32], PAGE_BYTES, &|| false).unwrap();
        if let Some(path) = table.path.clone() {
            drop(table);
            assert!(!path.exists());
        }
    }
}
