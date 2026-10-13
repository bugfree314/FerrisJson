//! S3 CLI prototype, deliberately separate from the shipping GUI/CLI backend.
use FerrisJson::engine::{
    budget::{BudgetManager, Limits},
    sparse::{Kind, SparseIndex},
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
    let path = PathBuf::from(args.next().ok_or("Usage: sparse_index FILE [--pointer /path] [--memory-mib N] [--disk-mib N] [--expect-count N] [--check-array] [--raw]")?);
    let mut pointer = String::new();
    let mut memory = 8usize;
    let mut disk = 4096usize;
    let mut expect = None;
    let mut check_array = false;
    let mut raw = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--pointer" => {
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
            _ => return Err(format!("Unknown option: {flag}")),
        }
    }
    let budget = BudgetManager::new(Limits {
        memory: memory
            .checked_mul(1024 * 1024)
            .ok_or("Memory quota overflow")?,
        disk: disk.checked_mul(1024 * 1024).ok_or("Disk quota overflow")?,
    });
    let mut index = SparseIndex::open(&path, budget.clone(), &|| false)?;
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
