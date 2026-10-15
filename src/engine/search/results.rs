use super::*;
use crate::engine::{
    session::SessionId,
    tree::{Edge, Location},
};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
const RECORD: usize = 88;
const HEADER: u64 = 32;
const NONE: u64 = u64::MAX;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Scanning,
    Completed,
    Cancelled,
    Failed,
}
#[derive(Clone, Debug)]
pub struct State {
    pub count: u64,
    pub ancestors: u64,
    pub scanned: u64,
    pub status: Status,
    pub error: String,
}
struct Page {
    records: Vec<[u64; 10]>,
    _lease: Reservation,
}
struct Inner {
    files: [Option<File>; 2],
    paths: [PathBuf; 2],
    session: SessionId,
    generation: u64,
    budget: BudgetManager,
    state: Mutex<State>,
    disk: Mutex<Reservation>,
    cache: Mutex<std::collections::VecDeque<(u64, Arc<Page>)>>,
    _metadata: Reservation,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.files[0].take();
        self.files[1].take();
        for p in &self.paths {
            let _ = fs::remove_file(p);
        }
    }
}
#[derive(Clone)]
pub struct ResultStore(Arc<Inner>);
pub struct ResultWriter {
    path: Vec<(crate::engine::session::NodeRef, u64, u64)>,
    store: ResultStore,
    buffers: [Vec<u8>; 2],
    written: [u64; 2],
    counts: [u64; 2],
    _lease: Reservation,
}
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}
fn read_at(
    file: &File,
    mut pos: u64,
    mut out: &mut [u8],
    cancel: &dyn Fn() -> bool,
) -> Result<(), String> {
    while !out.is_empty() {
        if cancel() {
            return Err("Result read cancelled".into());
        }
        let n = crate::engine::file_source::with_io_slot(cancel, || {
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileExt;
                file.seek_read(out, pos)
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                file.read_at(out, pos)
            }
        })
        .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("Truncated result store".into());
        }
        pos += n as u64;
        out = &mut out[n..];
    }
    Ok(())
}
fn write_at(
    file: &File,
    mut pos: u64,
    mut data: &[u8],
    cancel: &dyn Fn() -> bool,
) -> Result<(), String> {
    while !data.is_empty() {
        if cancel() {
            return Err("Result write cancelled".into());
        }
        let n = crate::engine::file_source::with_io_slot(cancel, || {
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileExt;
                file.seek_write(data, pos)
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                file.write_at(data, pos)
            }
        })
        .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.to_string());
        }
        pos += n as u64;
        data = &data[n..];
    }
    Ok(())
}
impl ResultStore {
    pub fn new(
        session: SessionId,
        generation: u64,
        budget: BudgetManager,
    ) -> Result<(Self, ResultWriter), String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let metadata = budget.reserve(
            Resource::Memory,
            16 * 1024,
            "result cache metadata and ancestry workspace",
        )?;
        let disk = budget.reserve(Resource::Disk, 2 * HEADER as usize, "result files")?;
        let mut files = [None, None];
        let mut paths = [PathBuf::new(), PathBuf::new()];
        for i in 0..2 {
            let p = std::env::temp_dir().join(format!(
                "ferris-search-{}-{}-{i}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&p) {
                Ok(f) => {
                    paths[i] = p;
                    files[i] = Some(f);
                }
                Err(e) => {
                    for j in 0..i {
                        files[j].take();
                        let _ = fs::remove_file(&paths[j]);
                    }
                    return Err(e.to_string());
                }
            }
        }
        let store = Self(Arc::new(Inner {
            files,
            paths,
            session,
            generation,
            budget: budget.clone(),
            state: Mutex::new(State {
                count: 0,
                ancestors: 0,
                scanned: 0,
                status: Status::Scanning,
                error: String::new(),
            }),
            disk: Mutex::new(disk),
            cache: Mutex::new(std::collections::VecDeque::with_capacity(8)),
            _metadata: metadata,
        }));
        for i in 0..2 {
            let mut h = [0; 32];
            h[..8].copy_from_slice(b"FJSR0001");
            h[8..16].copy_from_slice(&generation.to_le_bytes());
            h[16..24].copy_from_slice(&(i as u64).to_le_bytes());
            let sum = checksum(&h[..24]);
            h[24..].copy_from_slice(&sum.to_le_bytes());
            write_at(store.0.files[i].as_ref().unwrap(), 0, &h, &|| false)?;
        }
        let lease = budget.reserve(
            Resource::Memory,
            2 * 64 * 1024 + 8192,
            "result append buffers and ancestry cursor",
        )?;
        let writer = ResultWriter {
            path: Vec::with_capacity(128),
            store: store.clone(),
            buffers: [Vec::with_capacity(64 * 1024), Vec::with_capacity(64 * 1024)],
            written: [HEADER, HEADER],
            counts: [0, 0],
            _lease: lease,
        };
        Ok((store, writer))
    }
    pub fn generation(&self) -> u64 {
        self.0.generation
    }
    pub fn budget(&self) -> BudgetManager {
        self.0.budget.clone()
    }
    pub fn state(&self) -> State {
        self.0.state.lock().unwrap().clone()
    }
    pub fn progress(&self, n: u64) {
        self.0.state.lock().unwrap().scanned = n;
    }
    pub fn cancel(&self) {
        let mut s = self.0.state.lock().unwrap();
        if s.status == Status::Scanning {
            s.status = Status::Cancelled;
            s.error = "Operation cancelled; committed results are partial".into();
        }
    }
    pub fn fail(&self, error: String) {
        let mut s = self.0.state.lock().unwrap();
        s.status = Status::Failed;
        s.error = error;
    }
    fn record(
        &self,
        file: usize,
        index: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<[u64; 10], String> {
        let state = self.state();
        let count = if file == 0 {
            state.count
        } else {
            state.ancestors
        };
        if index >= count {
            return Err("Result ordinal is not committed".into());
        }
        let mut bytes = [0; RECORD];
        read_at(
            self.0.files[file].as_ref().unwrap(),
            HEADER
                + index
                    .checked_mul(RECORD as u64)
                    .ok_or("Result position overflow")?,
            &mut bytes,
            cancel,
        )?;
        if checksum(&bytes[..80]) != u64::from_le_bytes(bytes[80..].try_into().unwrap()) {
            return Err("Result checksum mismatch".into());
        }
        let mut words = [0; 10];
        for (i, w) in words.iter_mut().enumerate() {
            *w = u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        }
        Ok(words)
    }
    fn hit_record(&self, index: u64, cancel: &dyn Fn() -> bool) -> Result<[u64; 10], String> {
        let start = index / 128 * 128;
        let count = (self.state().count.saturating_sub(start)).min(128) as usize;
        let mut cache = self.0.cache.lock().unwrap();
        if let Some(i) = cache
            .iter()
            .position(|(s, p)| *s == start && (index - start) < p.records.len() as u64)
        {
            let entry = cache.remove(i).unwrap();
            let r = entry.1.records[(index - start) as usize];
            cache.push_back(entry);
            return Ok(r);
        }
        if count == 0 {
            return Err("Result ordinal is not committed".into());
        }
        let lease = self.0.budget.reserve(
            Resource::Memory,
            count * std::mem::size_of::<[u64; 10]>(),
            "result hot page",
        )?;
        let mut records = Vec::with_capacity(count);
        for i in 0..count {
            records.push(self.record(0, start + i as u64, cancel)?);
        }
        let result = *records
            .get((index - start) as usize)
            .ok_or("Result ordinal out of range")?;
        if cache.len() == 8 {
            cache.pop_front();
        }
        cache.retain(|(s, _)| *s != start);
        cache.push_back((
            start,
            Arc::new(Page {
                records,
                _lease: lease,
            }),
        ));
        Ok(result)
    }
    pub fn location(&self, index: u64, cancel: &dyn Fn() -> bool) -> Result<Location, String> {
        let hit = self.hit_record(index, cancel)?;
        let node = TreeNode::restore(self.0.session, hit[..8].try_into().unwrap())?;
        let mut link = hit[8];
        let mut ordinal = hit[9];
        let mut ancestors = Vec::with_capacity(128);
        while link != NONE {
            if ancestors.len() == 127 {
                return Err("Result ancestor cycle/depth limit".into());
            }
            let parent = self.record(1, link, cancel)?;
            ancestors.push(Edge {
                parent: TreeNode::restore(self.0.session, parent[..8].try_into().unwrap())?,
                ordinal,
            });
            ordinal = parent[9];
            link = parent[8];
        }
        ancestors.reverse();
        Ok(Location { node, ancestors })
    }
}
impl ResultWriter {
    pub fn location(
        &mut self,
        location: &Location,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        if location.node.reference.session != self.store.0.session || location.ancestors.len() > 127
        {
            return Err("Query result session/depth mismatch".into());
        }
        let mut link = NONE;
        let mut ordinal = 0;
        for (depth, edge) in location.ancestors.iter().enumerate() {
            if edge.parent.reference.session != self.store.0.session {
                return Err("Query ancestor session mismatch".into());
            }
            let saved = self
                .path
                .get(depth)
                .copied()
                .filter(|(reference, incoming, _)| {
                    *reference == edge.parent.reference && *incoming == ordinal
                });
            link = if let Some((_, _, id)) = saved {
                id
            } else {
                self.path.truncate(depth);
                let id = self.ancestor(edge.parent, link, ordinal, cancel)?;
                self.path.push((edge.parent.reference, ordinal, id));
                id
            };
            ordinal = edge.ordinal;
        }
        self.path.truncate(location.ancestors.len());
        self.hit(location.node, link, ordinal, cancel)
    }
    pub fn count(&self) -> u64 {
        self.counts[0]
    }
    fn append(
        &mut self,
        file: usize,
        node: TreeNode,
        parent: u64,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        if self.buffers[file].len() + RECORD > 64 * 1024 {
            self.flush(file, cancel)?;
        }
        let total = 2 * HEADER
            + (self.counts[0] + self.counts[1] + 1)
                .checked_mul(RECORD as u64)
                .ok_or("Result store overflow")?;
        self.store
            .0
            .disk
            .lock()
            .unwrap()
            .resize(usize::try_from(total).map_err(|_| "Result disk accounting overflow")?)?;
        let mut data = [0; RECORD];
        let words = node.words();
        for (i, w) in words.iter().enumerate() {
            data[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        data[64..72].copy_from_slice(&parent.to_le_bytes());
        data[72..80].copy_from_slice(&ordinal.to_le_bytes());
        let sum = checksum(&data[..80]);
        data[80..].copy_from_slice(&sum.to_le_bytes());
        self.buffers[file].extend_from_slice(&data);
        let index = self.counts[file];
        self.counts[file] += 1;
        Ok(index)
    }
    pub fn ancestor(
        &mut self,
        node: TreeNode,
        parent: u64,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        self.append(1, node, parent, ordinal, cancel)
    }
    pub fn hit(
        &mut self,
        node: TreeNode,
        parent: u64,
        ordinal: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        self.append(0, node, parent, ordinal, cancel)
    }
    fn flush(&mut self, file: usize, cancel: &dyn Fn() -> bool) -> Result<(), String> {
        write_at(
            self.store.0.files[file].as_ref().unwrap(),
            self.written[file],
            &self.buffers[file],
            cancel,
        )?;
        self.written[file] += self.buffers[file].len() as u64;
        self.buffers[file].clear();
        Ok(())
    }
    pub fn publish(&mut self, cancel: &dyn Fn() -> bool) -> Result<(), String> {
        self.flush(1, cancel)?;
        self.flush(0, cancel)?;
        let mut s = self.store.0.state.lock().unwrap();
        s.count = self.counts[0];
        s.ancestors = self.counts[1];
        Ok(())
    }
    pub fn finish(&mut self, status: Status, scanned: u64, error: String) -> Result<(), String> {
        crate::engine::file_source::with_io_context(
            crate::engine::file_source::Priority::Background,
            crate::engine::file_source::Cancellation::Flag(Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            )),
            || self.publish(&|| false),
        )?;
        let mut s = self.store.0.state.lock().unwrap();
        if s.status != Status::Cancelled {
            s.status = status;
            s.error = error;
        }
        s.scanned = scanned;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupted_result_record_is_rejected_before_location_decode() {
        let budget = BudgetManager::new(crate::engine::budget::Limits {
            memory: 32 * 1024 * 1024,
            disk: 1024 * 1024,
        });
        let doc = Arc::new(
            Document::parse_owned_recover_with_budget("0".into(), budget.clone())
                .map_err(|(e, _)| e)
                .unwrap(),
        );
        let node = LegacyProvider(doc.clone()).node(0);
        let (store, mut writer) = ResultStore::new(doc.session_id(), 1, budget.clone()).unwrap();
        writer.hit(node, u64::MAX, 0, &|| false).unwrap();
        writer.finish(Status::Completed, 1, String::new()).unwrap();
        write_at(store.0.files[0].as_ref().unwrap(), HEADER, &[0xff], &|| {
            false
        })
        .unwrap();
        assert!(
            store
                .location(0, &|| false)
                .err()
                .unwrap()
                .contains("checksum")
        );
        drop(writer);
        drop(store);
        drop(doc);
        assert_eq!(budget.usage().memory, 0);
        assert_eq!(budget.usage().disk, 0);
    }
}
