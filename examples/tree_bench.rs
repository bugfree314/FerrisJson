#![allow(
    dead_code,
    reason = "The CPU harness reuses desktop modules without launching a native window"
)]
#[cfg(all(not(test), not(target_os = "linux")))]
use FerrisJson::{document, engine};
#[cfg(all(not(test), not(target_os = "linux")))]
#[path = "../src/fonts.rs"]
mod fonts;
#[cfg(all(not(test), not(target_os = "linux")))]
#[path = "../src/palette.rs"]
mod palette;
#[cfg(all(not(test), not(target_os = "linux")))]
#[path = "../src/search_worker.rs"]
mod search_worker;
#[cfg(all(not(test), not(target_os = "linux")))]
#[path = "../src/paged_query_worker.rs"]
mod paged_query_worker;
#[cfg(all(not(test), not(target_os = "linux")))]
#[path = "../src/shutdown.rs"]
mod shutdown;
#[cfg(all(not(test), not(target_os = "linux")))]
#[path = "../src/tree_app.rs"]
mod tree_app;
#[cfg(all(not(test), not(target_os = "linux")))]
#[path = "../src/tree_worker.rs"]
mod tree_worker;
#[cfg(all(not(test), not(target_os = "linux")))]
#[path = "../src/ui.rs"]
mod ui;
#[cfg(all(not(test), not(target_os = "linux")))]
#[path = "../src/vim.rs"]
mod vim;
#[cfg(all(not(test), not(target_os = "linux")))]
fn main() {
    let result = (|| {
        let mut args = std::env::args().skip(1);
        let path =
            std::path::PathBuf::from(args.next().ok_or("Usage: tree_bench FILE COUNT [nested]")?);
        let count = args
            .next()
            .ok_or("Missing expected count")?
            .parse::<u64>()
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("Nonempty array required".into());
        }
        let nested = args.next().is_some_and(|a| a == "nested");
        tree_app::benchmark_live(path, count, nested)
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

#[cfg(all(not(test), target_os = "linux"))]
fn main() {
    eprintln!("This harness measures the desktop GUI; Linux TUI acceptance is separate.");
    std::process::exit(1);
}
