#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#![allow(
    non_snake_case,
    reason = "The binary and crate deliberately use the FerrisJson brand name"
)]

mod app;
mod search_worker;
mod sparse_app;
mod tree_app;
mod tree_worker;
use FerrisJson::{document, engine, export, loading};
mod fonts;
mod legacy_search;
mod palette;
mod preview_worker;
mod query_worker;
mod query_stream_worker;
mod paged_query_worker;
mod row_cache;
mod shutdown;
mod task_worker;
mod ui;
mod vim;
use document::{Document, SearchScope};
use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
};

fn main() {
    // Attach to an existing terminal for CLI use; never allocate a new console.
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let output = io::stdout().as_raw_handle();
        // A valid inherited output handle may be a pipe: leave it intact.
        if output.is_null() || output as isize == -1 {
            unsafe {
                windows_sys::Win32::System::Console::AttachConsole(
                    windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
                );
            }
        }
    }
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let mut file = None;
    let mut get = None;
    let mut query = None;
    let mut search = None;
    let mut pretty = false;
    let mut indexed = false;
    let mut sparse = false;
    let mut sparse_pages = false;
    let mut paged_tree = false;
    let mut cache_dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "FerrisJson {}\n\nUsage: FerrisJson [FILE | -] [OPTION]\n\n  --get PATH     Print exactly one selected JSON value\n  --query PATH   Print all JSONPath results as a JSON array\n  --search TEXT  Print matching paths (case-insensitive keys and values)\n  --pretty       Print formatted JSON\n  --indexed      File snapshot/range backend (limited JSONPath; no full Value)\n  --sparse       Experimental progressive paged tree GUI (file only)\n  --sparse-pages  S4 finite-page browser fallback\n  --paged-tree    Paged renderer for legacy backends (file only)\n  --cache-dir D  S4 persistent snapshot/index cache; requires --sparse\n  --version      Print version\n\nWithout options, opens the desktop viewer. Piped stdin is supported in in-memory mode.",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            "--version" => {
                println!("FerrisJson {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--get" => get = Some(args.next().ok_or("--get requires a path")?),
            "--query" => query = Some(args.next().ok_or("--query requires a JSONPath")?),
            "--search" => search = Some(args.next().ok_or("--search requires text")?),
            "--pretty" => pretty = true,
            "--indexed" => indexed = true,
            "--sparse" => sparse = true,
            "--sparse-pages" => {
                sparse = true;
                sparse_pages = true;
            }
            "--paged-tree" => paged_tree = true,
            "--cache-dir" => {
                cache_dir = Some(PathBuf::from(
                    args.next().ok_or("--cache-dir requires a directory")?,
                ))
            }
            "-" if file.is_none() => file = Some(PathBuf::from("-")),
            _ if arg.starts_with('-') => return Err(format!("Unknown option: {arg}")),
            _ if file.is_none() => file = Some(PathBuf::from(arg)),
            _ => return Err("Only one input file is supported".into()),
        }
    }
    if usize::from(get.is_some())
        + usize::from(query.is_some())
        + usize::from(search.is_some())
        + usize::from(pretty)
        > 1
    {
        return Err("Choose one of --get, --query, --search, or --pretty".into());
    }
    let use_stdin = file.as_ref().is_some_and(|f| f.as_os_str() == "-")
        || (file.is_none() && stdin_redirected());
    if paged_tree
        && (sparse
            || use_stdin
            || get.is_some()
            || query.is_some()
            || search.is_some()
            || pretty
            || file.is_none())
    {
        return Err("--paged-tree requires a file and opens the experimental tree renderer".into());
    }
    if cache_dir.is_some() && !sparse {
        return Err("--cache-dir requires --sparse".into());
    }
    if sparse
        && (indexed
            || use_stdin
            || get.is_some()
            || query.is_some()
            || search.is_some()
            || pretty
            || file.is_none())
    {
        return Err("--sparse requires a file and opens the experimental paged tree; use the sparse_index example for CLI queries".into());
    }
    if indexed && use_stdin {
        return Err("--indexed requires a file; stdin keeps the in-memory backend".into());
    }
    let text = if use_stdin {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("Cannot read stdin: {e}"))?;
        Some(text)
    } else {
        None
    };
    if get.is_some() || query.is_some() || search.is_some() || pretty {
        let doc = if indexed {
            Document::open_indexed(
                file.as_ref().ok_or("Provide a file for --indexed")?,
                || false,
                &document::LoadProgress::default(),
            )?
        } else {
            let text = match text {
                Some(text) => text,
                None => std::fs::read_to_string(
                    file.as_ref()
                        .ok_or("Provide a file or pipe JSON to stdin")?,
                )
                .map_err(|e| format!("Cannot open file: {e}"))?,
            };
            Document::parse_owned(text)?
        };
        let doc = std::sync::Arc::new(doc);
        let stdout = io::stdout();
        let mut output = io::BufWriter::new(stdout.lock());
        if let Some(path) = get {
            let request = engine::query::QueryRequest::new(path.clone());
            let store = engine::query::QueryEngine::run(engine::query::stream::Input::Document(doc.clone()), &request, 0, &|| false, &mut |_| Ok(()))?;
            let state = store.state();
            if state.status != engine::results::Status::Completed { return Err(state.error); }
            let id = match state.count {
                0 => return Err(format!("Path not found: {path}")),
                1 => doc.resolve(store.node(0, &|| false)?.reference)?,
                _ => return Err("Path selected multiple nodes; use --query to retrieve all results".into()),
            };
            doc.write_node(&mut output, id)
                .map_err(|e| e.to_string())?;
            writeln!(output).map_err(|e| e.to_string())?;
        } else if let Some(expression) = query {
            let request = engine::query::QueryRequest::new(expression);
            let store = engine::query::QueryEngine::run(engine::query::stream::Input::Document(doc.clone()), &request, 0, &|| false, &mut |_| Ok(()))?;
            let state = store.state();
            if state.status != engine::results::Status::Completed { return Err(state.error); }
            export::Selection::Stored { store, prefix: None }.write(&doc, &mut output)
                .map_err(|e| e.to_string())?;
            writeln!(output).map_err(|e| e.to_string())?;
        } else if let Some(term) = search {
            for id in doc.search(&term, false, false, SearchScope::All)? {
                writeln!(output, "{}", doc.path(id as usize)).map_err(|e| e.to_string())?;
            }
        } else {
            doc.write_node(&mut output, 0).map_err(|e| e.to_string())?;
            writeln!(output).map_err(|e| e.to_string())?;
        }
        output.flush().map_err(|e| e.to_string())?;
        return Ok(());
    }
    // Finder launches with an empty redirected stdin. Start with an empty viewer
    // instead of trying to parse that as a JSON document; CLI parsing stays strict.
    let text = text.filter(|text| !text.is_empty());
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/FerrisJson.png"))
                    .map_err(|e| format!("Cannot load FerrisJson icon: {e}"))?,
            )
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([700.0, 480.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "FerrisJson",
        options,
        Box::new(move |cc| {
            if paged_tree {
                return Ok(Box::new(tree_app::Viewer::new(
                    cc,
                    file.unwrap(),
                    tree_worker::Backend::Legacy(indexed),
                )) as Box<dyn eframe::App>);
            }
            if sparse && !sparse_pages {
                return Ok(Box::new(tree_app::Viewer::new(
                    cc,
                    file.unwrap(),
                    tree_worker::Backend::Sparse(engine::sparse::OpenOptions {
                        cache_dir,
                        ..Default::default()
                    }),
                )) as Box<dyn eframe::App>);
            }
            if sparse {
                return Ok(Box::new(sparse_app::Viewer::new(
                    cc,
                    file.unwrap(),
                    engine::sparse::OpenOptions {
                        cache_dir,
                        ..Default::default()
                    },
                )) as Box<dyn eframe::App>);
            }
            Ok(Box::new(app::Viewer::new(
                cc,
                if use_stdin { None } else { file },
                text,
                indexed,
            )))
        }),
    );
    shutdown::finish();
    result.map_err(|e| e.to_string())
}
fn stdin_redirected() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let handle = io::stdin().as_raw_handle();
        if handle.is_null() || handle as isize == -1 {
            return false;
        }
    }
    !io::stdin().is_terminal()
}
