#![allow(
    dead_code,
    reason = "The CPU harness reuses desktop modules without launching a native window"
)]
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
use FerrisJson::{document, engine};
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
#[path = "../src/fonts.rs"]
mod fonts;
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
#[path = "../src/palette.rs"]
mod palette;
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
#[path = "../src/search_worker.rs"]
mod search_worker;
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
#[path = "../src/paged_query_worker.rs"]
mod paged_query_worker;
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
#[path = "../src/shutdown.rs"]
mod shutdown;
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
#[path = "../src/tree_app.rs"]
mod tree_app;
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
#[path = "../src/tree_worker.rs"]
mod tree_worker;
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
#[path = "../src/ui.rs"]
mod ui;
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
#[path = "../src/vim.rs"]
mod vim;
#[cfg(all(not(test), not(any(target_os = "linux", all(target_os = "macos", feature = "tui")))))]
fn main() {
    let result = (|| {
        let mut args = std::env::args().skip(1);
        let path = std::path::PathBuf::from(args.next().ok_or("Usage: search_bench FILE COUNT")?);
        let count = args
            .next()
            .ok_or("Missing expected count")?
            .parse::<u64>()
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("Nonempty array required".into());
        }
        tree_app::benchmark_search(path, count)
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

#[cfg(all(not(test), any(target_os = "linux", all(target_os = "macos", feature = "tui"))))]
fn main() {
    eprintln!("This harness measures the desktop GUI; TUI acceptance is separate.");
    std::process::exit(1);
}
