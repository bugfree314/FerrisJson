#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#![allow(
    non_snake_case,
    reason = "The binary and crate deliberately use the FerrisJson brand name"
)]

mod app;
mod document;
mod export;
mod fonts;
mod loading;
mod palette;
mod row_cache;
mod ui;
mod vim;
mod worker;
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
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "FerrisJson {}\n\nUsage: FerrisJson [FILE | -] [OPTION]\n\n  --get PATH     Print exactly one selected JSON value\n  --query PATH   Print all JSONPath results as a JSON array\n  --search TEXT  Print matching paths (case-insensitive keys and values)\n  --pretty       Print formatted JSON\n  --version      Print version\n\nWithout options, opens the desktop viewer. Piped stdin is supported.",
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
        let text = match text {
            Some(text) => text,
            None => std::fs::read_to_string(
                file.as_ref()
                    .ok_or("Provide a file or pipe JSON to stdin")?,
            )
            .map_err(|e| format!("Cannot open file: {e}"))?,
        };
        let doc = Document::parse_owned(text)?;
        let stdout = io::stdout();
        let mut output = io::BufWriter::new(stdout.lock());
        if let Some(path) = get {
            serde_json::to_writer_pretty(&mut output, doc.at(doc.find_path(&path)?))
                .map_err(|e| e.to_string())?;
            writeln!(output).map_err(|e| e.to_string())?;
        } else if let Some(expression) = query {
            doc.write_results(&mut output, &doc.query_path(&expression)?)
                .map_err(|e| e.to_string())?;
            writeln!(output).map_err(|e| e.to_string())?;
        } else if let Some(term) = search {
            for id in doc.search(&term, false, false, SearchScope::All)? {
                writeln!(output, "{}", doc.path(id as usize)).map_err(|e| e.to_string())?;
            }
        } else {
            serde_json::to_writer_pretty(&mut output, &doc.value).map_err(|e| e.to_string())?;
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
    eframe::run_native(
        "FerrisJson",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::Viewer::new(
                cc,
                if use_stdin { None } else { file },
                text,
            )))
        }),
    )
    .map_err(|e| e.to_string())
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
