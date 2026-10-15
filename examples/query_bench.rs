#![allow(
    dead_code,
    reason = "The CPU harness reuses desktop modules without launching a native window"
)]
#[cfg(not(test))]
use FerrisJson::{document, engine};
#[cfg(not(test))]
#[path = "../src/fonts.rs"]
mod fonts;
#[cfg(not(test))]
#[path = "../src/palette.rs"]
mod palette;
#[cfg(not(test))]
#[path = "../src/search_worker.rs"]
mod search_worker;
#[cfg(not(test))]
#[path = "../src/shutdown.rs"]
mod shutdown;
#[cfg(not(test))]
#[path = "../src/tree_app.rs"]
mod tree_app;
#[cfg(not(test))]
#[path = "../src/tree_worker.rs"]
mod tree_worker;
#[cfg(not(test))]
#[path = "../src/ui.rs"]
mod ui;
#[cfg(not(test))]
#[path = "../src/vim.rs"]
mod vim;
#[cfg(not(test))]
fn main() {
    let result = (|| {
        let mut raw = std::env::args().skip(1);
        if raw.next().as_deref() == Some("--generate") {
            let path = std::path::PathBuf::from(raw.next().ok_or("Missing output path")?);
            let size = raw
                .next()
                .ok_or("Missing byte size")?
                .parse::<u64>()
                .map_err(|e| e.to_string())?;
            let count = generate(&path, size, raw.next().is_some_and(|s| s == "nested"))?;
            return Ok(serde_json::json!({"bytes":size,"count":count}));
        }
        let mut args = std::env::args().skip(1);
        let path = std::path::PathBuf::from(
            args.next()
                .ok_or("Usage: query_bench FILE COUNT [nested]")?,
        );
        let count = args
            .next()
            .ok_or("Missing expected count")?
            .parse::<u64>()
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("Nonempty array required".into());
        }
        tree_app::benchmark_query(path, count, args.next().is_some_and(|a| a == "nested"))
    })();
    match result {
        Ok(report) => println!("{report}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
#[cfg(test)]
fn main() {}

#[cfg(not(test))]
fn generate(path: &std::path::Path, size: u64, nested: bool) -> Result<u64, String> {
    use std::io::Write;
    let unit = if nested { 10_000 } else { 1000 };
    let count = size / unit;
    if count < 3 || !size.is_multiple_of(unit) {
        return Err("Size must be a multiple of record width and include >=3 items".into());
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut out = std::io::BufWriter::new(file);
    out.write_all(b"[").map_err(|e| e.to_string())?;
    for i in 0..count {
        let hit = i == 0 || i == count / 2 || i == count - 1;
        let (prefix, suffix) = if nested {
            (
                format!(
                    r#"{{"id":{i},"hit":{hit},"meta":{{"active":true,"values":[0,1]}},"payload":""#
                ),
                b"\"}".as_slice(),
            )
        } else {
            ((if hit { "1" } else { "0" }).to_owned(), b"".as_slice())
        };
        let length = unit as usize - 1 - usize::from(i == count - 1) - prefix.len() - suffix.len();
        out.write_all(prefix.as_bytes())
            .map_err(|e| e.to_string())?;
        out.write_all(&vec![if nested { b'x' } else { b' ' }; length])
            .map_err(|e| e.to_string())?;
        out.write_all(suffix).map_err(|e| e.to_string())?;
        if i < count - 1 {
            out.write_all(b",").map_err(|e| e.to_string())?;
        }
    }
    out.write_all(b"]").map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(count)
}
