use super::{ResultStore, Status};
use crate::engine::{budget::Resource, tree::Provider};
use crate::engine::tree::PagedTree;
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

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
