//! S3 CLI prototype, deliberately separate from the shipping GUI/CLI backend.
use FerrisJson::engine::{
    budget::{BudgetManager, Limits},
    sparse::{Kind, OpenOptions, SparseIndex, open_progressive},
};
use std::{
    io::{self, Write},
    path::PathBuf,
};
fn number(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<usize, String> {
    args.next()
        .ok_or_else(|| format!("Missing value for {flag}"))?
        .parse()
        .map_err(|_| format!("Invalid value for {flag}"))
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().ok_or("Usage: sparse_index FILE [--pointer /path | --query JSONPath] [--memory-mib N] [--disk-mib N] [--expect-count N] [--check-array] [--raw] [--cache-dir DIR] [--events] [--stop-after-page | --crash-after-page]")?);
    let mut pointer = String::new();
    let mut pointer_set = false;
    let mut query = None;
    let mut memory = 8usize;
    let mut disk = 4096usize;
    let mut expect = None;
    let mut check_array = false;
    let mut raw = false;
    let mut cache_dir = None;
    let mut events = false;
    let mut stop_after_page = false;
    let mut crash_after_page = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--query" => query = Some(args.next().ok_or("Missing JSONPath expression")?),
            "--pointer" => {
                pointer_set = true;
                pointer = args.next().ok_or("Missing pointer")?;
                if pointer.len() > 16 * 1024 {
                    return Err("Pointer exceeds 16 KiB".into());
                }
            }
            "--memory-mib" => memory = number(&mut args, &flag)?,
            "--disk-mib" => disk = number(&mut args, &flag)?,
            "--expect-count" => expect = Some(number(&mut args, &flag)? as u64),
            "--check-array" => check_array = true,
            "--raw" => raw = true,
            "--cache-dir" => {
                cache_dir = Some(PathBuf::from(args.next().ok_or("Missing cache directory")?))
            }
            "--events" => events = true,
            "--stop-after-page" => stop_after_page = true,
            "--crash-after-page" => crash_after_page = true,
            _ => return Err(format!("Unknown option: {flag}")),
        }
    }
    if query.is_some() && (pointer_set || expect.is_some() || check_array || raw || events || stop_after_page || crash_after_page) {
        return Err("--query cannot combine with pointer/probe/raw/event/stop/crash options".into());
    }
    let budget = BudgetManager::new(Limits {
        memory: memory
            .checked_mul(1024 * 1024)
            .ok_or("Memory quota overflow")?,
        disk: disk.checked_mul(1024 * 1024).ok_or("Disk quota overflow")?,
    });
    let cancelled = std::cell::Cell::new(false);
    let started = std::time::Instant::now();
    let mut index = if cache_dir.is_some() || events || stop_after_page || crash_after_page {
        open_progressive(
            &path,
            budget.clone(),
            OpenOptions {
                cache_dir,
                ..Default::default()
            },
            1,
            &|| cancelled.get(),
            &mut |event| {
                if events {
                    eprintln!(
                        "state={:?} scanned={} total={} commit={} reused={} resumed={} page={} elapsed_ms={:.3}",
                        event.status,
                        event.scanned,
                        event.total_bytes,
                        event.commit_sequence,
                        event.reused,
                        event.resumed,
                        event.page.as_ref().map_or(0, |p| p.nodes.len()),
                        started.elapsed().as_secs_f64() * 1000.0
                    );
                }
                if stop_after_page
                    && event.status == FerrisJson::engine::session::SessionStatus::Validating
                    && event.page.is_some()
                {
                    cancelled.set(true);
                }
                if crash_after_page
                    && event.status == FerrisJson::engine::session::SessionStatus::Validating
                    && event.page.is_some()
                {
                    std::process::exit(9);
                }
                Ok(())
            },
        )?
    } else {
        SparseIndex::open(&path, budget.clone(), &|| false)?
    };
    if let Some(expression) = query {
        use FerrisJson::engine::{query::{QueryEngine, QueryRequest, stream::Input}, results::Status, tree::{Provider, SparseProvider}};
        let mut output = SparseProvider::new(index.read_view()?)?;
        let store = QueryEngine::run(Input::Sparse(Box::new(index)), &QueryRequest::new(expression), 0, &|| false, &mut |_| Ok(()))?;
        let state = store.state();
        if state.status != Status::Completed { return Err(state.error); }
        let mut cursor = store.cursor(None)?;
        let stdout = io::stdout();
        let mut writer = io::BufWriter::new(stdout.lock());
        writer.write_all(b"[").map_err(|e| e.to_string())?;
        let mut first = true;
        while let Some(node) = cursor.next_node(&|| false)? {
            if !first { writer.write_all(b",").map_err(|e| e.to_string())?; } first = false;
            output.output(node, &|| false)?.write(&mut writer, &|| false).map_err(|e| e.to_string())?;
        }
        writer.write_all(b"]\n").map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let selected = index.pointer(&pointer, &|| false)?;
    if expect.is_some_and(|count| count != selected.child_count()) {
        return Err("Unexpected child count".into());
    }
    if check_array {
        if selected.kind() != Kind::Array || selected.child_count() == 0 {
            return Err("--check-array requires a nonempty array".into());
        }
        for ordinal in [0, selected.child_count() / 2, selected.child_count() - 1] {
            let child = index.child(selected, ordinal, &|| false)?;
            // Verify complete generated fixture values: an integer ordinal or
            // a 997-byte string containing its ordinal, a space and x padding.
            struct Prefix {
                bytes: [u8; 1024],
                count: usize,
            }
            impl Write for Prefix {
                fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                    if bytes.len() > self.bytes.len() - self.count {
                        return Err(io::Error::other("Acceptance probe exceeds 1024 bytes"));
                    }
                    let n = bytes.len();
                    self.bytes[self.count..self.count + n].copy_from_slice(&bytes[..n]);
                    self.count += n;
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }
            let mut probe = Prefix {
                bytes: [0; 1024],
                count: 0,
            };
            index.write_raw(child, &mut probe, &|| false)?;
            let text =
                std::str::from_utf8(&probe.bytes[..probe.count]).map_err(|e| e.to_string())?;
            let ordinal_text = ordinal.to_string();
            let matches = match child.kind() {
                Kind::Number => text == ordinal_text,
                Kind::String => {
                    let prefix = format!("\"{ordinal} ");
                    text.len() == 999
                        && text.starts_with(&prefix)
                        && text.ends_with('"')
                        && text.as_bytes()[prefix.len()..text.len() - 1]
                            .iter()
                            .all(|b| *b == b'x')
                }
                _ => false,
            };
            if !matches {
                return Err(format!("Array ordinal {ordinal} content mismatch"));
            }
            eprintln!(
                "checked ordinal={ordinal} range={}..{} kind={}",
                child.start(),
                child.end(),
                child.kind().name()
            );
        }
    }
    if raw {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        index.write_raw(selected, &mut output, &|| false)?;
        writeln!(output).map_err(|e| e.to_string())?;
    } else {
        println!(
            "validated source_bytes={} index_bytes={} kind={} children={} managed_memory_limit={} managed_peak_memory={} managed_peak_disk={}",
            index.source_bytes(),
            index.index_bytes(),
            selected.kind().name(),
            selected.child_count(),
            memory * 1024 * 1024,
            budget.usage().peak_memory,
            budget.usage().peak_disk
        );
    }
    drop(index);
    if budget.usage().memory != 0 || budget.usage().disk != 0 {
        return Err("Resource reservations remain after closing sparse session".into());
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
