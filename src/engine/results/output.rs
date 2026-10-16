use super::{ResultStore, Status};
use crate::engine::{budget::Resource, tree::Provider};
use crate::engine::tree::PagedTree;
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// Write committed values in result order, including duplicate selections.
pub fn write_values(provider: &mut dyn Provider, store: &ResultStore, prefix: Option<u64>, writer: &mut dyn std::io::Write, cancel: &dyn Fn() -> bool) -> serde_json::Result<()> {
    let io = |error: String| serde_json::Error::io(std::io::Error::other(error));
    if !provider.complete() { return Err(io("Pending: complete validation required for output".into())); }
    if provider.root(cancel).map_err(io)?.reference.session != store.session() { return Err(io("Result output belongs to another session".into())); }
    let mut cursor = store.cursor(prefix).map_err(io)?;
    writer.write_all(b"[").map_err(serde_json::Error::io)?;
    let mut first = true;
    while let Some(node) = cursor.next_node(cancel).map_err(io)? {
        if !first { writer.write_all(b",").map_err(serde_json::Error::io)?; } first = false;
        provider.output(node, cancel).map_err(io)?.write(writer, cancel)?;
    }
    writer.write_all(b"]").map_err(serde_json::Error::io)
}

/// The paged browser keeps its existing clipboard limit; larger output uses export.
pub fn copy(mut provider: Box<dyn Provider>, store: &ResultStore, prefix: Option<u64>, cancel: &dyn Fn() -> bool) -> Result<crate::engine::tree::Clipboard, String> {
    use std::io::{self, Write};
    struct Buffer<'a> { bytes: Vec<u8>, cancel: &'a dyn Fn() -> bool }
    impl Write for Buffer<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if (self.cancel)() { return Err(io::Error::other("Query copy cancelled")); }
            if self.bytes.len().saturating_add(bytes.len()) > crate::export::COPY_LIMIT {
                return Err(io::Error::other("Query results exceed the 8 MiB clipboard limit; use Export"));
            }
            self.bytes.extend_from_slice(bytes); Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let lease = provider.budget().reserve(Resource::Memory, crate::export::COPY_LIMIT, "paged query clipboard transfer")?;
    let mut buffer = Buffer { bytes: Vec::with_capacity(crate::export::COPY_LIMIT), cancel };
    write_values(provider.as_mut(), store, prefix, &mut buffer, cancel).map_err(|e| e.to_string())?;
    let text = String::from_utf8(buffer.bytes).map_err(|e| e.to_string())?;
    if cancel() { return Err("Query copy cancelled".into()); }
    Ok(crate::engine::tree::Clipboard { text, _lease: lease })
}

/// None means a complete result set; Some(count) captures a committed prefix.
pub fn export(
    mut provider: Box<dyn Provider>,
    store: &ResultStore,
    path: &Path,
    paths: bool,
    prefix: Option<u64>,
    stop: &AtomicBool,
) -> Result<(), String> {
    if provider.root(&|| stop.load(Ordering::Relaxed))?.reference.session != store.session() {
        return Err("Result export belongs to another session".into());
    }
    let cancel = || stop.load(Ordering::Relaxed);
    let count = if let Some(count) = prefix {
        if count > store.state().count { return Err("Requested export prefix is not committed".into()); }
        count
    } else {
        loop {
            if cancel() {
                return Err("Export cancelled".into());
            }
            let state = store.state();
            match state.status {
                Status::Completed => break state.count,
                Status::Scanning => std::thread::sleep(Duration::from_millis(10)),
                _ => return Err(
                    "Complete export requires a completed task; explicitly choose partial export"
                        .into(),
                ),
            }
        }
    };
    let budget = provider.budget();
    if !paths {
        return crate::export::atomic_write_with_budget(path, stop, &budget, |writer| {
            write_values(provider.as_mut(), store, Some(count), writer, &cancel)
        });
    }
    let mut cursor = store.cursor(Some(count))?;
    let _workspace = budget.reserve(
        Resource::Memory,
        64 * 1024 * 1024,
        "result export path workspace",
    )?;
    let mut tree = PagedTree::new(provider, &cancel)?;
    crate::export::atomic_write_with_budget(path, stop, &budget, |writer| {
        let io = |e: String| serde_json::Error::io(std::io::Error::other(e));
        writer.write_all(b"[").map_err(serde_json::Error::io)?;
        for index in 0..count {
            if cancel() {
                return Err(io("Export cancelled".into()));
            }
            if index > 0 {
                writer.write_all(b",").map_err(serde_json::Error::io)?;
            }
            let location = cursor.next(&cancel).map_err(io)?.ok_or_else(|| io("Missing result ordinal".into()))?;
            if paths {
                let location = tree.normalize(&location, &cancel).map_err(io)?;
                let path = tree
                    .path(&location, 8 * 1024 * 1024, true, &cancel)
                    .map_err(io)?;
                serde_json::to_writer(&mut *writer, &path)?;
            } else {
                tree.provider
                    .output(location.node, &cancel)
                    .map_err(io)?
                    .write(writer, &cancel)?;
            }
        }
        writer.write_all(b"]").map_err(serde_json::Error::io)
    })
}
