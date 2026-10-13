//! Document-engine benchmark; each invocation measures one fresh process.
#[allow(dead_code)]
#[path = "../src/document.rs"]
mod document;

use document::Document;
use serde_json::json;
use std::{hint::black_box, time::Instant};

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
    if !matches!(mode.as_str(), "retain" | "release") || args.next().is_some() {
        return Err("Usage: memory_bench FILE retain|release".into());
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
    let doc = Document::from_value(value, bytes);
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
            "nodes": doc.nodes.len(), "node_size": std::mem::size_of::<document::Node>(),
            "node_capacity_bytes": doc.nodes.capacity() * std::mem::size_of::<document::Node>(),
        "structure_capacity_bytes": doc.structure_capacity_bytes(),
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
