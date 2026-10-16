//! Compatibility entry for search callers; output is shared with structural queries.
use super::{Input, ResultStore};
use std::{path::Path, sync::atomic::AtomicBool};

pub fn export(
    input: Input,
    store: &ResultStore,
    path: &Path,
    paths: bool,
    prefix: Option<u64>,
    stop: &AtomicBool,
) -> Result<(), String> {
    if input.session() != store.session() {
        return Err("Result export belongs to another session".into());
    }
    crate::engine::results::output::export(input.provider()?, store, path, paths, prefix, stop)
}
