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
