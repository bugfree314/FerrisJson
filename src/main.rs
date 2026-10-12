mod app;
mod document;
use document::{Document, SearchScope};
use std::{
    io::{self, IsTerminal, Read},
    path::PathBuf,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let mut file = None;
    let mut get = None;
    let mut search = None;
    let mut pretty = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "Rust JSON Viewer 0.1.0\n\nUsage: rust-json-viewer [FILE | -] [OPTION]\n\n  --get PATH     Print selected JSON (JSONPath subset or JSON Pointer)\n  --search TEXT  Print matching paths (case-insensitive keys and values)\n  --pretty       Print formatted JSON\n  --version      Print version\n\nWithout options, opens the desktop viewer. Piped stdin is supported."
                );
                return Ok(());
            }
            "--version" => {
                println!("rust-json-viewer {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--get" => get = Some(args.next().ok_or("--get requires a path")?),
            "--search" => search = Some(args.next().ok_or("--search requires text")?),
            "--pretty" => pretty = true,
            "-" if file.is_none() => file = Some(PathBuf::from("-")),
            _ if arg.starts_with('-') => return Err(format!("Unknown option: {arg}")),
            _ if file.is_none() => file = Some(PathBuf::from(arg)),
            _ => return Err("Only one input file is supported".into()),
        }
    }
    if usize::from(get.is_some()) + usize::from(search.is_some()) + usize::from(pretty) > 1 {
        return Err("Choose one of --get, --search, or --pretty".into());
    }
    let use_stdin = file.as_ref().is_some_and(|f| f.as_os_str() == "-")
        || (file.is_none() && !io::stdin().is_terminal());
    let text = if use_stdin {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("Cannot read stdin: {e}"))?;
        Some(text)
    } else {
        None
    };
    if get.is_some() || search.is_some() || pretty {
        let text = match text {
            Some(text) => text,
            None => std::fs::read_to_string(
                file.as_ref()
                    .ok_or("Provide a file or pipe JSON to stdin")?,
            )
            .map_err(|e| format!("Cannot open file: {e}"))?,
        };
        let doc = Document::parse(&text)?;
        if let Some(path) = get {
            println!(
                "{}",
                serde_json::to_string_pretty(doc.at(doc.find_path(&path)?)).unwrap()
            );
        } else if let Some(term) = search {
            for id in doc.search(&term, false, false, SearchScope::All)? {
                println!("{}", doc.nodes[id].path);
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&doc.value).unwrap());
        }
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([700.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Rust JSON Viewer",
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
