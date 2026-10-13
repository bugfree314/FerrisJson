//! Document-engine benchmark; each invocation measures one fresh process.
use FerrisJson::{document, export, loading};

use document::{Document, LoadProgress, SearchScope};
use serde_json::json;
use std::{hint::black_box, sync::Arc, time::Instant};

fn backend(file: String, mode: String) -> Result<(), String> {
    let total = Instant::now();
    let started = Instant::now();
    let doc = if mode == "indexed-open" {
        Document::open_indexed(
            std::path::Path::new(&file),
            || false,
            &LoadProgress::default(),
        )?
    } else {
        Document::parse_owned(std::fs::read_to_string(&file).map_err(|e| e.to_string())?)?
    };
    let load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let after_load = memory();
    let started = Instant::now();
    black_box(doc.preview(0));
    let preview_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{}",
        json!({
            "file": file, "mode": mode, "backend": doc.backend_name(), "bytes": doc.bytes,
            "nodes": doc.node_count(), "has_whole_value": doc.value().is_some(),
            "structure_capacity_bytes": doc.structure_capacity_bytes(), "managed_budget": doc.budget().usage(), "load_ms": load_ms,
            "preview_ms": preview_ms, "total_ms": total.elapsed().as_secs_f64() * 1000.0,
            "package_version": env!("CARGO_PKG_VERSION"), "debug_assertions": cfg!(debug_assertions),
            "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "memory": {"after_load": after_load, "ready": memory()}
        })
    );
    Ok(())
}

fn switch(file: String, mode: String) -> Result<(), String> {
    let total = Instant::now();
    let started = Instant::now();
    let old = Arc::new(Document::parse_owned(
        std::fs::read_to_string(&file).map_err(|e| e.to_string())?,
    )?);
    let previous_load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let previous_ready = memory();
    let switch_started = Instant::now();
    let started = Instant::now();
    let retained = if mode == "switch-low" {
        loading::release_previous(Some(old), || false, |_| {});
        None
    } else {
        Some(old)
    };
    let previous_release_ms = started.elapsed().as_secs_f64() * 1000.0;
    let before_new = memory();
    let started = Instant::now();
    let new = Document::parse_owned(std::fs::read_to_string(&file).map_err(|e| e.to_string())?)?;
    let new_load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let after_new = memory();
    black_box(&retained);
    drop(retained);
    black_box(new.preview(0));
    println!(
        "{}",
        json!({
            "file": file, "mode": mode, "bytes": new.bytes, "nodes": new.node_count(),
            "node_size": std::mem::size_of::<document::Node>(),
            "structure_capacity_bytes": new.structure_capacity_bytes(), "managed_budget": new.budget().usage(),
            "package_version": env!("CARGO_PKG_VERSION"), "debug_assertions": cfg!(debug_assertions),
            "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "previous_load_ms": previous_load_ms, "previous_release_ms": previous_release_ms,
            "new_load_ms": new_load_ms, "switch_ms": switch_started.elapsed().as_secs_f64() * 1000.0,
            "total_ms": total.elapsed().as_secs_f64() * 1000.0,
            "memory": {"previous_ready": previous_ready, "before_new": before_new,
                "after_new": after_new, "ready": memory()}
        })
    );
    Ok(())
}

fn tree(file: String) -> Result<(), String> {
    let total = Instant::now();
    let doc = Document::parse_owned(std::fs::read_to_string(&file).map_err(|e| e.to_string())?)?;
    let document_ready = memory();
    let expanded = vec![true; doc.node_count()];
    let started = Instant::now();
    let mut rows = doc.visible(&expanded)?;
    let rows_ms = started.elapsed().as_secs_f64() * 1000.0;
    let after_rows = memory();
    let allocation = rows.as_ptr();
    let started = Instant::now();
    doc.visible_into(&expanded, &mut rows)?;
    let rebuild_rows_ms = started.elapsed().as_secs_f64() * 1000.0;
    let reused_row_allocation = allocation == rows.as_ptr();
    let started = Instant::now();
    let matches = doc.search(".*", false, true, SearchScope::All)?;
    let search_all_ms = started.elapsed().as_secs_f64() * 1000.0;
    black_box((&expanded, &rows, &matches));
    println!(
        "{}",
        json!({
            "file": file, "mode": "tree", "bytes": doc.bytes, "nodes": doc.node_count(),
            "node_size": std::mem::size_of::<document::Node>(),
            "structure_capacity_bytes": doc.structure_capacity_bytes(), "managed_budget": doc.budget().usage(),
            "rows": rows.len(), "row_capacity_bytes": rows.capacity() * std::mem::size_of::<u32>(),
            "matches": matches.len(), "match_capacity_bytes": matches.capacity() * std::mem::size_of::<u32>(),
            "expanded_capacity_bytes": expanded.capacity() * std::mem::size_of::<bool>(),
            "package_version": env!("CARGO_PKG_VERSION"), "debug_assertions": cfg!(debug_assertions),
            "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "rows_ms": rows_ms, "search_all_ms": search_all_ms,
            "rebuild_rows_ms": rebuild_rows_ms, "reused_row_allocation": reused_row_allocation,
            "total_ms": total.elapsed().as_secs_f64() * 1000.0,
            "memory": {"document_ready": document_ready, "after_rows": after_rows, "ready": memory()}
        })
    );
    Ok(())
}

fn export_file(file: String, mode: String) -> Result<(), String> {
    let total = Instant::now();
    let doc = Document::parse_owned(std::fs::read_to_string(&file).map_err(|e| e.to_string())?)?;
    let document_ready = memory();
    let directory =
        std::path::PathBuf::from(format!("target/memory-export-bench-{}", std::process::id()));
    std::fs::create_dir(&directory).map_err(|e| e.to_string())?;
    let path = directory.join("output.json");
    let started = Instant::now();
    let retained = if mode == "export-string" {
        let json = serde_json::to_string_pretty(doc.value().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&path, &json).map_err(|e| e.to_string())?;
        Some(json)
    } else {
        export::atomic_write(
            &path,
            &std::sync::atomic::AtomicBool::new(false),
            |writer| export::Selection::Node(0).write(&doc, writer),
        )?;
        None
    };
    let export_ms = started.elapsed().as_secs_f64() * 1000.0;
    black_box(&retained);
    let after_export = memory();
    let output_bytes = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
    let output_string_capacity = retained.as_ref().map_or(0, String::capacity);
    drop(retained);
    let ready = memory();
    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    std::fs::remove_dir(&directory).map_err(|e| e.to_string())?;
    println!(
        "{}",
        json!({
            "file": file, "mode": mode, "bytes": doc.bytes, "nodes": doc.node_count(),
            "node_size": std::mem::size_of::<document::Node>(), "output_bytes": output_bytes,
            "output_string_capacity": output_string_capacity, "export_ms": export_ms,
            "package_version": env!("CARGO_PKG_VERSION"), "debug_assertions": cfg!(debug_assertions),
            "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "total_ms": total.elapsed().as_secs_f64() * 1000.0,
            "memory": {"document_ready": document_ready, "after_export": after_export, "ready": ready}
        })
    );
    Ok(())
}

#[cfg(windows)]
fn memory() -> serde_json::Value {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        peak_paged_pool: usize,
        paged_pool: usize,
        peak_nonpaged_pool: usize,
        nonpaged_pool: usize,
        pagefile: usize,
        peak_pagefile: usize,
        private_usage: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut Counters,
            size: u32,
        ) -> i32;
    }
    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Default::default()
    };
    // Counters has the PROCESS_MEMORY_COUNTERS_EX layout and a valid writable buffer.
    if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) } == 0 {
        return json!({"error": std::io::Error::last_os_error().to_string()});
    }
    json!({
        "resident_bytes": counters.working_set,
        "peak_resident_bytes": counters.peak_working_set,
        "private_bytes": counters.private_usage,
        "peak_commit_bytes": counters.peak_pagefile,
    })
}

#[cfg(not(windows))]
fn memory() -> serde_json::Value {
    json!({"unavailable": "Built-in memory counters currently support Windows only"})
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let file = args
        .next()
        .ok_or("Usage: memory_bench FILE retain|release")?;
    let mode = args.next().ok_or("Specify retain or release")?;
    if !matches!(
        mode.as_str(),
        "retain"
            | "release"
            | "switch-normal"
            | "switch-low"
            | "tree"
            | "export-string"
            | "export-stream"
            | "memory-open"
            | "indexed-open"
    ) || args.next().is_some()
    {
        return Err("Usage: memory_bench FILE retain|release|switch-normal|switch-low|tree|export-string|export-stream".into());
    }
    if mode.starts_with("switch-") {
        return switch(file, mode);
    }
    if mode.starts_with("export-") {
        return export_file(file, mode);
    }
    if mode.ends_with("-open") {
        return backend(file, mode);
    }
    if mode == "tree" {
        return tree(file);
    }
    let initial = memory();
    let total = Instant::now();
    let started = Instant::now();
    let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
    let read_ms = started.elapsed().as_secs_f64() * 1000.0;
    let bytes = text.len();
    let after_read = memory();
    let started = Instant::now();
    // Same parser, BOM handling and feature flags as Document::parse_owned.
    let value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("Invalid JSON: {e}"))?;
    let parse_ms = started.elapsed().as_secs_f64() * 1000.0;
    let after_parse = memory();
    let started = Instant::now();
    let retained = if mode == "retain" {
        Some(text)
    } else {
        drop(text);
        None
    };
    let release_ms = started.elapsed().as_secs_f64() * 1000.0;
    let before_index = memory();
    let started = Instant::now();
    let doc = Document::from_value(value, bytes)?;
    let index_ms = started.elapsed().as_secs_f64() * 1000.0;
    let after_index = memory();
    // Keep baseline source alive through index construction, as the old GUI loader did.
    black_box(&retained);
    drop(retained);
    let after_source_drop = memory();
    let started = Instant::now();
    let preview = doc.preview(0);
    black_box(&preview);
    let preview_ms = started.elapsed().as_secs_f64() * 1000.0;
    let total_ms = total.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{}",
        json!({
            "file": file, "mode": mode, "bytes": bytes,
            "nodes": doc.node_count(), "node_size": std::mem::size_of::<document::Node>(),
            "node_capacity_bytes": doc.node_capacity_bytes(),
            "structure_capacity_bytes": doc.structure_capacity_bytes(), "managed_budget": doc.budget().usage(),
            "full_expand_rows_payload_bytes": doc.node_count() * std::mem::size_of::<u32>(),
            "expanded_flags_bytes": doc.node_count() * std::mem::size_of::<bool>(),
            "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "package_version": env!("CARGO_PKG_VERSION"), "debug_assertions": cfg!(debug_assertions),
            "read_ms": read_ms, "parse_ms": parse_ms, "release_ms": release_ms,
            "count_and_index_ms": index_ms, "preview_ms": preview_ms, "total_ms": total_ms,
            "memory": {"initial": initial, "after_read": after_read, "after_parse": after_parse,
                "before_index": before_index, "after_index": after_index,
                "after_source_drop": after_source_drop, "ready": memory()}
        })
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
