use super::*;
use crate::engine::tree::PagedTree;
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// None means a complete result set; Some(count) captures a committed prefix.
pub fn export(
    input: Input,
    store: &ResultStore,
    path: &Path,
    paths: bool,
    prefix: Option<u64>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let cancel = || stop.load(Ordering::Relaxed);
    let count = if let Some(count) = prefix {
        count.min(store.state().count)
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
                    "Complete export requires a completed search; explicitly choose partial export"
                        .into(),
                ),
            }
        }
    };
    let budget = input.budget();
    let _workspace = budget.reserve(
        Resource::Memory,
        64 * 1024 * 1024,
        "search export path workspace",
    )?;
    let mut tree = PagedTree::new(input.provider()?, &cancel)?;
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
            let location = store.location(index, &cancel).map_err(io)?;
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
