use crate::document::{Document, NodeIds};
use crate::engine::budget::{BudgetManager, Reservation, Resource, reserve_vec};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub const COPY_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub enum Selection {
    Node(usize),
    Results(Arc<NodeIds>),
    Stored { store: crate::engine::results::ResultStore, prefix: Option<u64> },
}
#[derive(Debug, PartialEq)]
pub enum CopyError {
    TooLarge,
    Serialization(String),
}
impl Selection {
    pub fn write(&self, doc: &Document, writer: impl Write) -> serde_json::Result<()> {
        self.write_cancellable(doc, writer, &|| false)
    }
    pub fn write_cancellable(&self, doc: &Document, mut writer: impl Write, cancel: &dyn Fn() -> bool) -> serde_json::Result<()> {
        match self {
            Self::Node(id) => doc.write_node(writer, *id),
            Self::Results(ids) => doc.write_results(writer, ids),
            Self::Stored { store, prefix } => {
                let io = |error: String| serde_json::Error::io(io::Error::other(error));
                if store.session() != doc.session_id() { return Err(io("Results belong to another document".into())); }
                let mut cursor = store.cursor(*prefix).map_err(io)?;
                writer.write_all(b"[").map_err(serde_json::Error::io)?;
                let mut first = true;
                while let Some(node) = cursor.next_node(cancel).map_err(io)? {
                    if !first { writer.write_all(b",").map_err(serde_json::Error::io)?; }
                    first = false;
                    let id = doc.resolve(node.reference).map_err(io)?;
                    doc.write_node(&mut writer, id)?;
                }
                writer.write_all(b"]").map_err(serde_json::Error::io)
            }
        }
    }
    pub fn json(&self, doc: &Document, limit: Option<usize>) -> Result<String, CopyError> {
        self.json_cancellable(doc, limit, &|| false)
    }
    pub fn json_cancellable(&self, doc: &Document, limit: Option<usize>, cancel: &dyn Fn() -> bool) -> Result<String, CopyError> {
        struct Buffer<'a> {
            bytes: Vec<u8>,
            limit: usize,
            exceeded: bool,
            reservation: Reservation,
            cancel: &'a dyn Fn() -> bool,
        }
        impl Write for Buffer<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if (self.cancel)() { return Err(io::Error::other("Copy cancelled")); }
                let needed = self.bytes.len().saturating_add(bytes.len());
                if needed > self.limit {
                    self.exceeded = true;
                    return Err(io::Error::other("Clipboard limit exceeded"));
                }
                if needed > self.bytes.capacity() {
                    let capacity = needed
                        .max(self.bytes.capacity().saturating_mul(2))
                        .min(self.limit);
                    let extra = capacity - self.bytes.len();
                    reserve_vec(&mut self.bytes, extra, &mut self.reservation)
                        .map_err(io::Error::other)?;
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut buffer = Buffer {
            bytes: Vec::new(),
            limit: limit.unwrap_or(usize::MAX),
            exceeded: false,
            reservation: doc
                .budget()
                .reserve(Resource::Memory, 0, "clipboard serialization")
                .map_err(CopyError::Serialization)?,
            cancel,
        };
        if let Err(error) = self.write_cancellable(doc, &mut buffer, cancel) {
            return Err(if buffer.exceeded {
                CopyError::TooLarge
            } else {
                CopyError::Serialization(error.to_string())
            });
        }
        String::from_utf8(buffer.bytes).map_err(|e| CopyError::Serialization(e.to_string()))
    }
}

struct TempFile {
    path: PathBuf,
    committed: bool,
}
impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}
static SERIAL: AtomicU64 = AtomicU64::new(0);
fn temporary(target: &Path) -> io::Result<(File, TempFile)> {
    let directory = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for _ in 0..100 {
        let path = directory.join(format!(
            ".FerrisJson-export-{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                return Ok((
                    file,
                    TempFile {
                        path,
                        committed: false,
                    },
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("Cannot create export temporary file"))
}

pub fn atomic_write(
    target: &Path,
    cancelled: &AtomicBool,
    serialize: impl FnOnce(&mut dyn Write) -> serde_json::Result<()>,
) -> Result<(), String> {
    atomic_write_with_budget(target, cancelled, &BudgetManager::default(), serialize)
}

pub fn atomic_write_with_budget(
    target: &Path,
    cancelled: &AtomicBool,
    budget: &BudgetManager,
    serialize: impl FnOnce(&mut dyn Write) -> serde_json::Result<()>,
) -> Result<(), String> {
    struct CancelWriter<'a> {
        output: &'a mut dyn Write,
        cancelled: &'a AtomicBool,
        disk: &'a mut Reservation,
    }
    impl Write for CancelWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            for chunk in bytes.chunks(64 * 1024) {
                if self.cancelled.load(Ordering::Relaxed) {
                    return Err(io::Error::other("Export cancelled"));
                }
                self.disk
                    .resize(
                        self.disk
                            .bytes()
                            .checked_add(chunk.len())
                            .ok_or_else(|| io::Error::other("Export size overflow"))?,
                    )
                    .map_err(io::Error::other)?;
                crate::engine::file_source::with_io_slot(
                    &|| self.cancelled.load(Ordering::Relaxed),
                    || self.output.write_all(chunk),
                )?;
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.cancelled.load(Ordering::Relaxed) {
                return Err(io::Error::other("Export cancelled"));
            }
            crate::engine::file_source::with_io_slot(
                &|| self.cancelled.load(Ordering::Relaxed),
                || self.output.flush(),
            )
        }
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    let _buffer_lease = budget.reserve(Resource::Memory, 64 * 1024, "export buffer")?;
    let mut disk = budget.reserve(Resource::Disk, 0, "export temporary file")?;
    let (file, mut temp) = temporary(target).map_err(|e| e.to_string())?;
    let mut output = BufWriter::with_capacity(64 * 1024, file);
    let written = (|| {
        let mut writer = CancelWriter {
            output: &mut output,
            cancelled,
            disk: &mut disk,
        };
        serialize(&mut writer).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())
    })();
    if let Err(error) = written {
        // Do not flush an unwritten buffer from BufWriter::drop after cancellation.
        let (file, buffer) = output.into_parts();
        drop(buffer);
        drop(file);
        return Err(error);
    }
    crate::engine::file_source::with_io_slot(&|| cancelled.load(Ordering::Relaxed), || {
        output.get_ref().sync_all()
    })
    .map_err(|e| e.to_string())?;
    drop(output);
    if cancelled.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    fs::rename(&temp.path, target).map_err(|e| e.to_string())?;
    temp.committed = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "FerrisJson-export-test-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn target(&self) -> PathBuf {
            self.0.join("output.json")
        }
        fn assert_clean(&self) {
            assert_eq!(fs::read_dir(&self.0).unwrap().count(), 1);
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.target());
            let _ = fs::remove_dir(&self.0);
        }
    }
    #[test]
    fn atomic_export_preserves_old_target_on_error_and_cancel() {
        let dir = Directory::new();
        let path = dir.target();
        fs::write(&path, b"previous").unwrap();
        let cancelled = AtomicBool::new(false);
        let failure = atomic_write(&path, &cancelled, |writer| {
            writer.write_all(b"partial").unwrap();
            Err(serde_json::Error::io(io::Error::other("Injected failure")))
        });
        assert!(failure.unwrap_err().contains("Injected failure"));
        assert_eq!(fs::read(&path).unwrap(), b"previous");
        dir.assert_clean();
        let failure = atomic_write(&path, &cancelled, |writer| {
            writer.write_all(b"partial").unwrap();
            cancelled.store(true, Ordering::Relaxed);
            Ok(())
        });
        assert!(failure.unwrap_err().contains("Export cancelled"));
        assert_eq!(fs::read(&path).unwrap(), b"previous");
        dir.assert_clean();
    }
    #[test]
    fn streaming_export_preserves_order_duplicates_precision_and_replaces_target() {
        let dir = Directory::new();
        let path = dir.target();
        fs::write(&path, b"old").unwrap();
        let doc = Document::parse(r#"{"z":"中文","a":123456789012345678901234567890}"#).unwrap();
        let selection = Selection::Results(Arc::new(doc.query_path("$['a','z','a']").unwrap()));
        atomic_write(&path, &AtomicBool::new(false), |writer| {
            selection.write(&doc, writer)
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            selection.json(&doc, None).unwrap()
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[\n  123456789012345678901234567890,\n  \"中文\",\n  123456789012345678901234567890\n]"
        );
        dir.assert_clean();
    }
    #[test]
    fn clipboard_limit_is_explicit_and_never_returns_truncated_json() {
        let doc = Document::parse(r#"{"text":"中文\nquoted\""}"#).unwrap();
        let selection = Selection::Node(0);
        let json = selection.json(&doc, None).unwrap();
        assert_eq!(selection.json(&doc, Some(json.len())).unwrap(), json);
        assert_eq!(
            selection.json(&doc, Some(json.len() - 1)),
            Err(CopyError::TooLarge)
        );
        assert_eq!(selection.json(&doc, Some(0)), Err(CopyError::TooLarge));
        let empty = Selection::Results(Arc::new(NodeIds::default()));
        assert_eq!(empty.json(&doc, Some(2)).unwrap(), "[]");
    }
}
