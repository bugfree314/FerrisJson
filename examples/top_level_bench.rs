#![allow(
    dead_code,
    reason = "Reuse the original desktop UI without a native window"
)]
#[cfg(not(test))]
use FerrisJson::{document, engine, export, loading};
#[cfg(not(test))]
#[path = "../src/app.rs"]
mod app;
#[cfg(not(test))]
#[path = "../src/fonts.rs"]
mod fonts;
#[cfg(not(test))]
#[path = "../src/palette.rs"]
mod palette;
#[cfg(not(test))]
#[path = "../src/preview_worker.rs"]
mod preview_worker;
#[cfg(not(test))]
#[path = "../src/row_cache.rs"]
mod row_cache;
#[cfg(not(test))]
#[path = "../src/shutdown.rs"]
mod shutdown;
#[cfg(not(test))]
#[path = "../src/ui.rs"]
mod ui;
#[cfg(not(test))]
#[path = "../src/vim.rs"]
mod vim;
#[cfg(not(test))]
#[path = "../src/worker.rs"]
mod worker;
#[cfg(not(test))]
fn main() {
    let result = (|| {
        let mut args = std::env::args().skip(1);
        let path = std::path::PathBuf::from(
            args.next()
                .ok_or("Usage: top_level_bench FILE [--indexed]")?,
        );
        app::benchmark_top_level(path, args.next().as_deref() == Some("--indexed"))
    })();
    match result {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
#[cfg(test)]
fn main() {}
