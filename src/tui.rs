//! Unix terminal frontend (Linux by default, macOS with the tui feature). All file, tree, search, query and output work stays
//! on the existing bounded worker lanes; the UI only holds presentation pages.
use crate::{
    document::SearchScope,
    engine::{budget::Reservation, results::{State, Status as ResultStatus}, search::Query,
        session::SessionStatus, sparse::OpenOptions, tree::{Clipboard, TreeFrame},
        tree_navigation::Navigation},
    terminal_wake::Context,
    tree_worker::{Backend, ResultPage, Status, Task},
    vim::{Action, Vim},
};
use crossterm::{
    cursor::{Hide, Show}, event::{self, DisableBracketedPaste, EnableBracketedPaste,
        DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind}, execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style}, text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph}, Frame, Terminal};
use std::{fs::{File, OpenOptions as FileOptions}, io::{self, IsTerminal, Write}, path::PathBuf,
    sync::{Arc, atomic::{AtomicBool, Ordering}}, thread::JoinHandle, time::Duration};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const INPUT_LIMIT: usize = 64 * 1024;
const PAGE: u64 = 16;

fn tty() -> io::Result<File> {
    FileOptions::new().read(true).write(true).open("/dev/tty")
}
fn restore_terminal() {
    let _ = disable_raw_mode();
    if let Ok(mut output) = tty() {
        let _ = execute!(output, DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen, Show);
    }
}
struct TerminalGuard {
    signals: Vec<signal_hook::SigId>,
    active: bool,
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active { restore_terminal(); }
        for signal in self.signals.drain(..) { signal_hook::low_level::unregister(signal); }
    }
}

pub fn run(file: Option<PathBuf>, text: Option<String>, indexed: bool, sparse: bool,
    cache_dir: Option<PathBuf>) -> Result<(), String> {
    let output = tty().map_err(|_| "TUI requires an interactive terminal (/dev/tty); use --get, --query, --search or --pretty for noninteractive output".to_string())?;
    if !output.is_terminal() || std::env::var("TERM").is_ok_and(|v| v == "dumb") {
        return Err("TUI requires an ANSI terminal; use CLI output options in this environment".into());
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| { restore_terminal(); previous(info); }));
    let stop = Arc::new(AtomicBool::new(false));
    let mut guard = TerminalGuard { signals: Vec::new(), active: false };
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP, signal_hook::consts::SIGINT] {
        guard.signals.push(signal_hook::flag::register(signal, stop.clone()).map_err(|e| e.to_string())?);
    }
    enable_raw_mode().map_err(|e| e.to_string())?;
    guard.active = true;
    let mut terminal = Terminal::new(CrosstermBackend::new(output)).map_err(|e| e.to_string())?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture, Hide).map_err(|e| e.to_string())?;
    terminal.clear().map_err(|e| e.to_string())?;
    let wake = Context::default();
    let mut app = App::new(indexed, sparse, cache_dir, wake.clone());
    if let Some(text) = text {
        app.schedule(PathBuf::from("<stdin>"), Backend::Stdin(text))?;
    } else if let Some(path) = file {
        app.open(path)?;
    } else {
        app.prompt = Some(Prompt::new(Input::Open, String::new()));
    }
    let result = (|| -> Result<(), String> {
        let mut dirty = true;
        while !stop.load(Ordering::Relaxed) {
            dirty |= wake.take_repaint();
            app.poll()?;
            if let Some(clipboard) = app.clipboard.take() {
                match copy_terminal(terminal.backend_mut(), clipboard.text()) {
                    Ok(()) => app.notice = "Copy sent to terminal (OSC 52 support required)".into(),
                    Err(error) => app.notice = error,
                }
                dirty = true;
            }
            if dirty {
                terminal.draw(|frame| app.draw(frame)).map_err(|e| e.to_string())?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        match app.key(key) {
                            Ok(true) => break,
                            Ok(false) => (),
                            Err(error) => app.notice = error,
                        }
                    }
                    Event::Paste(text) => {
                        if app.help { continue; }
                        if let Some(prompt) = &mut app.prompt { prompt.insert(&text); }
                        else {
                            if let Err(error) = app.schedule(PathBuf::from("<paste>"), Backend::Stdin(text)) { app.notice = error; }
                        }
                    }
                    Event::Mouse(mouse) => { if let Err(error) = app.mouse(mouse) { app.notice = error; } }
                    Event::Resize(..) => (),
                    _ => continue,
                }
                dirty = true;
            }
        }
        Ok(())
    })();
    // Restore the terminal before waiting for cancellation/temporary-file cleanup.
    drop(guard);
    drop(app);
    result
}

#[derive(Clone, Copy, PartialEq)]
enum Pane { Tree, Preview, Search, Query }
impl Pane {
    fn number(self) -> u8 { match self { Self::Tree => 1, Self::Preview => 2, Self::Search => 3, Self::Query => 4 } }
    fn next(self) -> Self {
        match self { Self::Tree => Self::Preview, Self::Preview => Self::Search,
            Self::Search => Self::Query, Self::Query => Self::Tree }
    }
    fn name(self) -> &'static str {
        match self { Self::Tree => "Tree", Self::Preview => "Preview",
            Self::Search => "Search", Self::Query => "JSONPath" }
    }
}
#[derive(Clone, Copy)]
enum Input { Open, Search(bool), Query, Locate, Export(Pane, bool) }
impl Input {
    fn label(self) -> &'static str {
        match self { Self::Open => "Open file", Self::Search(false) => "Search",
            Self::Search(true) => "Search backwards", Self::Query => "JSONPath / Pointer",
            Self::Locate => "Locate path (one node)", Self::Export(_, false) => "Export to (replaces destination)",
            Self::Export(_, true) => "Export committed partial results to (replaces destination)" }
    }
}
#[derive(Clone)]
struct Prompt { kind: Input, text: String, cursor: usize }
impl Prompt {
    fn new(kind: Input, text: String) -> Self { let cursor = text.len(); Self { kind, text, cursor } }
    fn insert(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            if self.text.len() + c.len_utf8() > INPUT_LIMIT { break; }
            self.text.insert(self.cursor, c); self.cursor += c.len_utf8();
        }
    }
    fn left(&mut self) { self.cursor = self.text[..self.cursor].char_indices().last().map_or(0, |(i, _)| i); }
    fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() { self.cursor += c.len_utf8(); }
    }
}
#[derive(Default)]
struct Results {
    generation: u64, state: Option<State>, waiting: bool, error: String,
    elapsed: Option<Duration>,
    selected: Option<u64>, start: u64, page: Option<ResultPage>, requested: Option<(u64, u64, u64)>,
}
impl Results {
    fn count(&self) -> u64 { self.state.as_ref().map_or(0, |s| s.count) }
    fn update(&mut self, generation: u64, state: Option<State>, waiting: bool, error: String) {
        if self.generation != generation { self.page = None; self.requested = None; }
        self.generation = generation; self.state = state; self.waiting = waiting; self.error = error;
        if self.count() == 0 { self.selected = None; self.start = 0; }
        else if self.selected.is_some_and(|i| i >= self.count()) { self.selected = Some(self.count() - 1); }
    }
    fn select(&mut self, index: u64) {
        self.selected = Some(index); let start = index / PAGE * PAGE;
        if self.start != start { self.start = start; self.page = None; self.requested = None; }
    }
    fn request_key(&self) -> Option<(u64, u64, u64)> {
        (self.count() > self.start).then(|| (self.generation, self.start, self.count().min(self.start + PAGE)))
    }
    fn accept(&mut self, page: ResultPage) {
        if page.generation == self.generation && page.start == self.start
            && self.requested.is_none_or(|(_, _, count)| page.count >= count) { self.page = Some(page); }
    }
}
#[derive(Clone, Copy)]
enum UiAction { Focus(Pane), TreeRow(u64), Result(Pane, u64), Page(Pane, bool), Edit(Input), Option(u8), Reload, Cancel }
struct Hit { area: Rect, action: UiAction }
// Keep library clipboard values intact. Path transfers already own a separate
// reservation; retain it until the terminal write finishes as well.
enum ClipboardTransfer {
    Value(Clipboard),
    Path { text: String, _lease: Reservation },
}
impl ClipboardTransfer {
    fn text(&self) -> &str {
        match self { Self::Value(value) => &value.text, Self::Path { text, .. } => text }
    }
}
struct App {
    task: Option<Task>, retiring: Option<JoinHandle<()>>, pending: Option<(PathBuf, Backend)>,
    wake: Context, indexed: bool, sparse: bool, cache_dir: Option<PathBuf>,
    title: String, file: Option<PathBuf>, status: Option<Status>, frame: Option<TreeFrame>, pane: Pane, detail: Pane,
    search: Results, query: Results, search_text: String, query_text: String, search_reverse: bool,
    case: bool, regex: bool, scope: SearchScope, prompt: Option<Prompt>, help: bool, help_scroll: u16, help_height: u16,
    notice: String, vim: Vim, preview_scroll: usize, preview_rows: usize, preview_height: u16,
    preview_wrap: bool, preview_x: usize, viewport: u16, fullscreen: bool,
    clipboard: Option<ClipboardTransfer>, hits: Vec<Hit>, panels: Vec<(Pane, Rect)>,
}
impl App {
    fn new(indexed: bool, sparse: bool, cache_dir: Option<PathBuf>, wake: Context) -> Self {
        Self { task: None, retiring: None, pending: None, wake, indexed, sparse, cache_dir,
            title: "No file".into(), file: None, status: None, frame: None, pane: Pane::Tree, detail: Pane::Preview,
            search: Results::default(), query: Results::default(), search_text: String::new(),
            query_text: String::new(), search_reverse: false, case: false, regex: false, scope: SearchScope::All,
            prompt: None, help: false, help_scroll: 0, help_height: 1, notice: "o: open file   F1: help   q: quit".into(),
            vim: Vim::default(), preview_scroll: 0, preview_rows: 0, preview_height: 1,
            preview_wrap: true, preview_x: 0, viewport: 0, fullscreen: false,
            clipboard: None, hits: Vec::new(), panels: Vec::new() }
    }
    fn open(&mut self, path: PathBuf) -> Result<(), String> {
        let backend = if self.sparse { Backend::Sparse(OpenOptions { cache_dir: self.cache_dir.clone(), ..Default::default() }) }
            else { Backend::Legacy(self.indexed) };
        self.schedule(path, backend)
    }
    fn schedule(&mut self, path: PathBuf, backend: Backend) -> Result<(), String> {
        if self.pending.is_some() || self.retiring.as_ref().is_some_and(|t| !t.is_finished()) {
            return Err("Previous document is still closing; retry opening shortly".into());
        }
        if let Some(task) = self.task.take() { self.retiring = Some(task.retire()); }
        self.title = path.display().to_string();
        self.file = (!matches!(&backend, Backend::Stdin(_))).then(|| path.clone());
        self.pending = Some((path, backend));
        self.frame = None; self.status = None; self.search = Results::default(); self.query = Results::default();
        self.search_text.clear(); self.query_text.clear(); self.preview_scroll = 0; self.preview_x = 0; self.viewport = 0;
        self.vim.cancel(); self.pane = Pane::Tree; self.detail = Pane::Preview; self.clipboard = None; self.search_reverse = false; self.notice = "Opening... Ctrl+X: cancel".into();
        self.wake.request_repaint(); Ok(())
    }
    fn poll(&mut self) -> Result<(), String> {
        if self.retiring.as_ref().is_some_and(|t| t.is_finished()) {
            let _ = self.retiring.take().unwrap().join();
        }
        if self.retiring.is_none() && let Some((path, backend)) = self.pending.take() {
            self.task = Some(Task::new(path, backend, self.wake.clone())?);
            self.wake.request_repaint();
        }
        let Some(task) = &self.task else { return Ok(()); };
        let poll = task.poll();
        if let Some(clipboard) = poll.clipboard { self.clipboard = Some(ClipboardTransfer::Value(clipboard)); }
        if let Some((text, lease)) = poll.path_copy { self.clipboard = Some(ClipboardTransfer::Path { text, _lease: lease }); }
        if let Some((generation, clipboard)) = poll.query_clipboard && generation == task.query.generation() {
            self.clipboard = Some(ClipboardTransfer::Value(clipboard));
        }
        let search = task.search.poll();
        self.search.update(search.generation, search.state, search.waiting, search.error);
        // Adopt the initial automatic search selection. Later selections belong
        // to the UI until the worker catches up, so rapid keys cannot rewind it.
        if self.search.selected.is_none() && let Some(cursor) = search.cursor {
            self.search.select(cursor); self.wake.request_repaint();
        }
        let query = task.query.poll();
        self.query_text = query.expression;
        self.query.elapsed = query.elapsed;
        self.query.update(query.generation, query.state, query.waiting, query.error);
        if self.query.selected.is_none() && self.query.count() > 0 {
            match task.query_select(0) {
                Ok(()) => self.query.select(0), Err(error) => self.notice = error,
            }
        }
        if let Some(page) = poll.search_page { self.search.accept(page); }
        if let Some(page) = poll.query_page { self.query.accept(page); }
        if let Some((generation, start, error)) = poll.query_page_error
            && generation == self.query.generation && start == self.query.start {
            self.notice = error; // Retry only when count, page, or generation changes.
        }
        if let Some(key) = self.search.request_key() && self.search.requested != Some(key) {
            task.search_page(self.search.start); self.search.requested = Some(key);
        }
        if let Some(key) = self.query.request_key() && self.query.requested != Some(key) {
            match task.query_page(self.query.start) {
                Ok(()) => self.query.requested = Some(key), Err(error) => self.notice = error,
            }
        }
        if let Some((epoch, result)) = poll.frame && epoch == task.epoch.load(Ordering::Relaxed) {
            match result {
                Ok(frame) if poll.status.session.is_none_or(|s| s == frame.selected.node.reference.session) => {
                    if self.frame.as_ref().is_none_or(|old| old.selected.node.reference != frame.selected.node.reference) {
                        self.preview_scroll = 0;
                        self.preview_x = 0;
                    }
                    self.frame = Some(frame);
                }
                Ok(_) => (), Err(error) => self.notice = terminal_notice(error),
            }
        }
        if self.status.as_ref().is_none_or(|old| old.state != poll.status.state)
            && poll.status.state == SessionStatus::Ready { self.notice = "Ready".into(); }
        if !poll.status.message.is_empty() { self.notice = poll.status.message.clone(); }
        if !poll.notice.is_empty() { self.notice = terminal_notice(poll.notice); }
        if matches!(poll.status.state, SessionStatus::Failed | SessionStatus::Cancelled) { self.frame = None; }
        self.status = Some(poll.status);
        Ok(())
    }
    fn navigate(&self, command: Navigation) -> Result<(), String> {
        self.task.as_ref().ok_or("Open a file first")?.command(command)
    }
    fn results(&mut self, pane: Pane) -> &mut Results {
        if pane == Pane::Query { &mut self.query } else { &mut self.search }
    }
    fn select_result(&mut self, pane: Pane, index: u64) -> Result<(), String> {
        let task = self.task.as_ref().ok_or("Open a file first")?;
        if pane == Pane::Query { task.query_select(index)?; }
        else { task.search_select(index); }
        self.results(pane).select(index); Ok(())
    }
    fn step_result(&mut self, pane: Pane, backwards: bool, count: u64) -> Result<(), String> {
        let results = self.results(pane); let total = results.count();
        if total == 0 { return Err("No committed results yet".into()); }
        let count = count % total;
        let index = match results.selected {
            None => if backwards { total - 1 } else { 0 },
            Some(i) => if backwards { (i + total - count) % total } else { (i + count) % total },
        };
        self.select_result(pane, index)
    }
    fn begin(&mut self, kind: Input, text: String) {
        self.vim.cancel(); self.prompt = Some(Prompt::new(kind, text));
    }
    fn submit(&mut self, prompt: Prompt) -> Result<(), String> {
        let text = prompt.text;
        if text.is_empty() {
            if let Some(task) = &self.task {
                match prompt.kind {
                    Input::Search(_) => { task.search_clear(false); self.search = Results::default(); self.search_text.clear(); return Ok(()); }
                    Input::Query => { task.query_clear(true); self.query = Results::default(); self.query_text.clear(); return Ok(()); }
                    _ => (),
                }
            }
            return Err("Input is empty".into());
        }
        if matches!(prompt.kind, Input::Open) { return self.open(PathBuf::from(text)); }
        let task = self.task.as_ref().ok_or("Open a file first")?;
        if self.status.as_ref().is_some_and(|s| matches!(s.state, SessionStatus::Failed | SessionStatus::Cancelled)) {
            return Err("Session unavailable; open a file to start again".into());
        }
        match prompt.kind {
            Input::Open => unreachable!(),
            Input::Search(reverse) => {
                task.search_query(Query { term: text.clone(), case: self.case, regex: self.regex, scope: self.scope }, reverse)?;
                self.search_text = text; self.search_reverse = reverse; self.search = Results::default(); self.focus(Pane::Search);
            }
            Input::Query => {
                task.query_request(text.clone())?; self.query_text = text;
                self.query = Results::default(); self.focus(Pane::Query);
            }
            Input::Locate => task.command(Navigation::Locate(text))?,
            Input::Export(pane, partial) => {
                let path = PathBuf::from(text);
                if pane == Pane::Query {
                    let prefix = partial.then(|| task.query.store().map_or(0, |s| s.state().count));
                    task.query_output(Some(path), prefix)?;
                } else if pane == Pane::Search {
                    if !partial && self.search.state.as_ref().is_none_or(|s| s.status != ResultStatus::Completed) {
                        return Err("Complete export requires completed search; use E for a partial export".into());
                    }
                    task.search_export(path, false, partial)?;
                } else {
                    let frame = self.frame.as_ref().ok_or("No selected value")?;
                    task.output(frame.selected.clone(), Some(path), false)?;
                }
                self.notice = "Export queued; destination is replaced atomically after success".into();
            }
        }
        Ok(())
    }
    fn focus(&mut self, pane: Pane) {
        self.pane = pane;
        if pane != Pane::Tree { self.detail = pane; }
        self.vim.cancel();
    }
    fn reload(&mut self) -> Result<(), String> {
        let path = self.file.clone().ok_or("Reload needs a file; paste or pipe input again")?;
        self.prompt = None;
        self.open(path)
    }
    fn copy(&self, path: bool) -> Result<(), String> {
        let task = self.task.as_ref().ok_or("Open a file first")?;
        let frame = self.frame.as_ref().ok_or("No selected value")?;
        task.output(frame.selected.clone(), None, path)
    }
    fn cancel_task(&mut self) {
        self.vim.cancel();
        if self.pending.take().is_some() { self.notice = "Open cancelled".into(); }
        if let Some(task) = &self.task {
            if self.status.as_ref().is_some_and(|s| matches!(s.state, SessionStatus::Preparing | SessionStatus::Validating)) {
                task.cancel();
            } else if self.pane == Pane::Query { task.query_clear(false); }
            else if self.pane == Pane::Search { task.search_clear(true); }
            else { let _ = task.command(Navigation::CancelPending); }
        }
        self.notice = "Task cancelled; committed results remain available".into();
    }
    fn page(&mut self, pane: Pane, backwards: bool, count: u64) -> Result<(), String> {
        if matches!(pane, Pane::Search | Pane::Query) {
            let results = self.results(pane);
            let total = results.count();
            if total == 0 { return Err("No committed results yet".into()); }
            let last = (total - 1) / PAGE * PAGE;
            let delta = PAGE.saturating_mul(count);
            let start = if backwards { results.start.saturating_sub(delta) }
                else { results.start.saturating_add(delta).min(last) };
            return self.select_result(pane, start);
        }
        let rows = if pane == Pane::Preview { self.preview_height } else { self.viewport };
        self.move_in(pane, backwards, u64::from(rows.max(1)).saturating_mul(count))
    }
    fn move_in(&mut self, pane: Pane, backwards: bool, count: u64) -> Result<(), String> {
        if pane == Pane::Preview {
            let count = usize::try_from(count).unwrap_or(usize::MAX);
            self.preview_scroll = if backwards { self.preview_scroll.saturating_sub(count) }
                else { self.preview_scroll.saturating_add(count) };
            return Ok(());
        }
        if matches!(pane, Pane::Search | Pane::Query) {
            let results = self.results(pane); let total = results.count();
            if total == 0 { return Err("No committed results yet".into()); }
            let index = match results.selected {
                None => if backwards { total - 1 } else { 0 },
                Some(i) => if backwards { i.saturating_sub(count) } else { i.saturating_add(count).min(total - 1) },
            };
            return self.select_result(pane, index);
        }
        let count = count.min(i64::MAX as u64) as i64;
        self.navigate(Navigation::Move(if backwards { -count } else { count }))
    }
    fn boundary(&mut self, last: bool, row: Option<u64>) -> Result<(), String> {
        match self.pane {
            Pane::Preview => self.preview_scroll = row.map_or(if last { usize::MAX } else { 0 }, |r| r as usize),
            pane @ (Pane::Search | Pane::Query) => {
                let total = self.results(pane).count();
                if total == 0 { return Err("No committed results yet".into()); }
                self.select_result(pane, row.unwrap_or(if last { total - 1 } else { 0 }).min(total - 1))?;
            }
            Pane::Tree => self.navigate(row.map_or(if last { Navigation::Last } else { Navigation::First }, Navigation::Row))?,
        }
        Ok(())
    }
    fn key(&mut self, key: KeyEvent) -> Result<bool, String> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('q') { return Ok(true); }
        if key.code == KeyCode::F(1) {
            self.help = !self.help; self.help_scroll = 0; self.vim.cancel(); return Ok(false);
        }
        if self.help {
            match key.code {
                KeyCode::Esc => self.help = false,
                KeyCode::Down | KeyCode::Char('j') => self.help_scroll = self.help_scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => self.help_scroll = self.help_scroll.saturating_sub(1),
                KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(self.help_height.max(1)),
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(self.help_height.max(1)),
                KeyCode::Home => self.help_scroll = 0,
                _ => (),
            }
            return Ok(false);
        }
        if control {
            match key.code {
                KeyCode::Char('o') => { self.begin(Input::Open, String::new()); return Ok(false); }
                KeyCode::Char('f') => { self.begin(Input::Search(false), self.search_text.clone()); return Ok(false); }
                KeyCode::Char('x') => { self.cancel_task(); return Ok(false); }
                _ => (),
            }
        }
        match key.code {
            KeyCode::F(3) => { self.step_result(Pane::Search, key.modifiers.contains(KeyModifiers::SHIFT), 1)?; return Ok(false); }
            KeyCode::F(4) => { self.step_result(Pane::Query, key.modifiers.contains(KeyModifiers::SHIFT), 1)?; return Ok(false); }
            KeyCode::F(5) => { self.reload()?; return Ok(false); }
            KeyCode::F(n @ 6..=8) => { self.toggle_search(n); return Ok(false); }
            _ => (),
        }
        if let Some(mut prompt) = self.prompt.take() {
            match key.code {
                KeyCode::Esc => return Ok(false),
                KeyCode::Enter => {
                    let retry = prompt.clone();
                    if let Err(error) = self.submit(prompt) { self.prompt = Some(retry); return Err(error); }
                    return Ok(false);
                }
                KeyCode::Left => prompt.left(), KeyCode::Right => prompt.right(),
                KeyCode::Home => prompt.cursor = 0, KeyCode::End => prompt.cursor = prompt.text.len(),
                KeyCode::Backspace if prompt.cursor > 0 => {
                    let end = prompt.cursor; prompt.left(); prompt.text.replace_range(prompt.cursor..end, "");
                }
                KeyCode::Delete if prompt.cursor < prompt.text.len() => {
                    let start = prompt.cursor; prompt.right(); prompt.text.replace_range(start..prompt.cursor, ""); prompt.cursor = start;
                }
                KeyCode::Char('u') if control => { prompt.text.clear(); prompt.cursor = 0; }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => prompt.insert(&c.to_string()),
                _ => (),
            }
            self.prompt = Some(prompt); return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            let pane = match key.code { KeyCode::Char('1') => Some(Pane::Tree), KeyCode::Char('2') => Some(Pane::Preview),
                KeyCode::Char('3') => Some(Pane::Search), KeyCode::Char('4') => Some(Pane::Query), _ => None };
            if let Some(pane) = pane { self.focus(pane); }
            return Ok(false);
        }
        if control {
            let count = self.vim.take_count("Ctrl shortcut") as u64;
            match key.code {
                KeyCode::Char('c') => self.copy(false)?, KeyCode::Char('p') => self.copy(true)?,
                KeyCode::Char('d' | 'u') => {
                    let height = match self.pane {
                        Pane::Preview => self.preview_height,
                        pane @ (Pane::Search | Pane::Query) => self.panels.iter().find(|(p, _)| *p == pane)
                            .map_or(1, |(_, area)| area.height.saturating_sub(6).clamp(1, PAGE as u16)),
                        Pane::Tree => self.viewport,
                    };
                    self.move_in(self.pane, key.code == KeyCode::Char('u'), u64::from((height / 2).max(1)) * count)?;
                }
                KeyCode::Char('e' | 'b') => self.page(self.pane, key.code == KeyCode::Char('b'), count)?,
                KeyCode::Char('v') => self.notice = "Paste using your terminal (usually Ctrl+Shift+V or Shift+Insert)".into(),
                _ => (),
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('o') => self.begin(Input::Open, String::new()),
            KeyCode::Char('/') => self.begin(Input::Search(false), self.search_text.clone()),
            KeyCode::Char('?') => self.begin(Input::Search(true), self.search_text.clone()),
            KeyCode::Char(':') | KeyCode::F(2) => self.begin(Input::Query, self.query_text.clone()),
            KeyCode::Char('.' | 'p') => { self.vim.record("."); self.begin(Input::Locate, String::new()); }
            KeyCode::Char('e' | 'E') => self.begin(Input::Export(self.pane, key.code == KeyCode::Char('E')), String::new()),
            KeyCode::Char('w') if self.pane == Pane::Preview => { self.preview_wrap = !self.preview_wrap; self.preview_x = 0; }
            KeyCode::F(9) => self.fullscreen = !self.fullscreen,
            KeyCode::Tab => self.focus(self.pane.next()),
            KeyCode::BackTab => self.focus(self.pane.next().next().next()),
            KeyCode::Esc => { self.vim.cancel(); let _ = self.navigate(Navigation::CancelPending); self.focus(Pane::Tree); }
            KeyCode::Delete if matches!(self.pane, Pane::Search | Pane::Query) => {
                if let Some(task) = &self.task {
                    if self.pane == Pane::Search { task.search_clear(false); self.search = Results::default(); self.search_text.clear(); }
                    else { task.query_clear(true); self.query = Results::default(); self.query_text.clear(); }
                }
            }
            _ => self.navigation_key(key)?,
        }
        Ok(false)
    }
    fn toggle_search(&mut self, key: u8) {
        match key {
            6 => self.case = !self.case, 7 => self.regex = !self.regex,
            _ => self.scope = match self.scope { SearchScope::All => SearchScope::Keys, SearchScope::Keys => SearchScope::Values, SearchScope::Values => SearchScope::All },
        }
        if let Some(task) = &self.task { task.search_clear(false); }
        self.search = Results::default(); self.notice = "Search options changed; / then Enter to search again".into();
    }
    fn navigation_key(&mut self, key: KeyEvent) -> Result<(), String> {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                let count = self.vim.take_count("arrow") as u64;
                return self.move_in(self.pane, key.code == KeyCode::Up, count);
            }
            KeyCode::Left | KeyCode::Right => {
                let count = self.vim.take_count("arrow") as u64;
                return self.horizontal(key.code == KeyCode::Left, count);
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let count = self.vim.take_count("page") as u64;
                return self.page(self.pane, key.code == KeyCode::PageUp, count);
            }
            KeyCode::Home | KeyCode::End => { self.vim.record("boundary"); return self.boundary(key.code == KeyCode::End, None); }
            KeyCode::Enter if matches!(self.pane, Pane::Search | Pane::Query) => {
                let pane = self.pane; let index = self.results(pane).selected.unwrap_or(0);
                self.select_result(pane, index)?; self.focus(Pane::Tree); return Ok(());
            }
            KeyCode::Enter | KeyCode::Char(' ') => { self.vim.record("toggle"); return self.navigate(Navigation::Toggle); }
            KeyCode::Char(c) => {
                if let Some(command) = self.vim.feed(c) {
                    let count = command.count as u64;
                    match command.action {
                        Action::Move(delta) => self.move_in(self.pane, delta < 0, count)?,
                        Action::Left => self.horizontal(true, count)?, Action::Right => self.horizontal(false, count)?,
                        Action::First | Action::Last => self.boundary(command.action == Action::Last, command.explicit_count.then_some(count - 1))?,
                        Action::Match(opposite) => self.step_result(Pane::Search, self.search_reverse ^ opposite, count)?,
                        Action::Copy(path) => self.copy(path)?,
                        Action::Toggle => self.navigate(Navigation::Toggle)?, Action::Open => self.navigate(Navigation::Open)?,
                        Action::Close => self.navigate(Navigation::Close)?, Action::ExpandAll => self.navigate(Navigation::ExpandAll)?,
                        Action::CollapseAll => self.navigate(Navigation::CollapseAll)?,
                        Action::History(back) => { for _ in 0..count { self.navigate(Navigation::History(back))?; } }
                        Action::Screen(c) => self.navigate(Navigation::Screen(c))?,
                        Action::Search(reverse) => self.begin(Input::Search(reverse), self.search_text.clone()),
                    }
                }
            }
            _ => (),
        }
        Ok(())
    }
    fn horizontal(&mut self, backwards: bool, count: u64) -> Result<(), String> {
        match self.pane {
            pane @ (Pane::Search | Pane::Query) => self.page(pane, backwards, count),
            Pane::Preview => {
                if !self.preview_wrap {
                    let count = count as usize;
                    self.preview_x = if backwards { self.preview_x.saturating_sub(count) } else { self.preview_x.saturating_add(count).min(200_000) };
                }
                Ok(())
            }
            Pane::Tree => self.navigate(if backwards { Navigation::Left(count) } else { Navigation::Right(count) }),
        }
    }
    fn mouse(&mut self, mouse: MouseEvent) -> Result<(), String> {
        if self.prompt.is_some() { return Ok(()); }
        if self.help {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => self.help_scroll = self.help_scroll.saturating_add(3),
                _ => (),
            }
            return Ok(());
        }
        let point = ratatui::layout::Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(action) = self.hits.iter().rev().find(|h| h.area.contains(point)).map(|h| h.action) {
                    match action {
                        UiAction::Focus(pane) => self.focus(pane),
                        UiAction::TreeRow(rank) => { self.focus(Pane::Tree); self.navigate(Navigation::Row(rank))?; }
                        UiAction::Result(pane, index) => { self.focus(pane); self.select_result(pane, index)?; }
                        UiAction::Page(pane, backwards) => { self.focus(pane); self.page(pane, backwards, 1)?; }
                        UiAction::Edit(kind) => {
                            let text = match kind { Input::Search(_) => self.search_text.clone(), Input::Query => self.query_text.clone(), _ => String::new() };
                            self.begin(kind, text);
                        }
                        UiAction::Option(key) => self.toggle_search(key), UiAction::Reload => self.reload()?, UiAction::Cancel => self.cancel_task(),
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if let Some(pane) = self.panels.iter().find(|(_, area)| area.contains(point)).map(|(pane, _)| *pane) {
                    self.focus(pane); self.move_in(pane, mouse.kind == MouseEventKind::ScrollUp, 3)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        self.hits.clear(); self.panels.clear();
        if area.width < 24 || area.height < 8 {
            frame.render_widget(Paragraph::new("Resize to 24x8 or larger. Ctrl+Q: quit; F1: help."), area);
            return;
        }
        let compact = area.height < 16 || area.width < 40;
        let flags = vec![
            (format!("Aa F6:{}", if self.case { "on" } else { "off" }), UiAction::Option(6)),
            (format!(".* F7:{}", if self.regex { "on" } else { "off" }), UiAction::Option(7)),
            (format!("F8:{:?}", self.scope), UiAction::Option(8)),
        ];
        let tools = vec![
            ("Open ^O".into(), UiAction::Edit(Input::Open)),
            ("Path .".into(), UiAction::Edit(Input::Locate)),
            ("Find /".into(), UiAction::Edit(Input::Search(false))),
            ("Query F2".into(), UiAction::Edit(Input::Query)),
            ("Reload F5".into(), UiAction::Reload),
            ("Stop ^X".into(), UiAction::Cancel),
        ];
        let search_options = self.pane == Pane::Search || self.prompt.as_ref().is_some_and(|p| matches!(p.kind, Input::Search(_)));
        let tool_rows = if compact { 0 } else { button_rows(&tools, area.width) };
        let flag_rows = if !compact && search_options { button_rows(&flags, area.width) } else { 0 };
        let header = 2 + tool_rows + flag_rows;
        let footer = if area.height < 10 { 1 } else if area.height >= 24 { 4 } else if compact { 2 } else { 3 };
        let layout = Layout::default().direction(Direction::Vertical).constraints([
            Constraint::Length(header), Constraint::Min(3), Constraint::Length(footer),
        ]).split(area);
        let backend = if self.file.is_none() { "Memory / input" } else if self.sparse { "Sparse" } else if self.indexed { "Indexed" } else { "Memory" };
        let title = if area.width >= 72 { format!("FerrisJson {} | {backend} | {}", env!("CARGO_PKG_VERSION"), safe(&self.title, false)) }
            else { format!("FerrisJson F1:Help | {}", safe(&self.title, false)) };
        frame.render_widget(Paragraph::new(fit(&title, area.width)), Rect::new(area.x, area.y, area.width, 1));
        let tabs = [Pane::Tree, Pane::Preview, Pane::Search, Pane::Query];
        let mut x = area.x;
        for pane in tabs {
            let text = if area.width < 48 { format!("{}:{} ", pane.number(), match pane { Pane::Tree => "T", Pane::Preview => "V", Pane::Search => "S", Pane::Query => "J" }) }
                else { format!(" {} {} ", pane.number(), pane.name()) };
            let width = (text.width() as u16).min(area.right().saturating_sub(x));
            let rect = Rect::new(x, area.y + 1, width, 1);
            let style = if self.pane == pane { Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD) }
                else { Style::default().fg(Color::Cyan) };
            frame.render_widget(Paragraph::new(text).style(style), rect);
            self.hits.push(Hit { area: rect, action: UiAction::Focus(pane) }); x += width;
        }
        if !compact {
            self.buttons(frame, Rect::new(area.x, area.y + 2, area.width, tool_rows), &tools);
            if search_options { self.buttons(frame, Rect::new(area.x, area.y + 2 + tool_rows, area.width, flag_rows), &flags); }
        }
        let body = layout[1];
        if self.fullscreen || body.width < 72 || body.height < 10 {
            self.panel(frame, body, self.pane);
        } else if body.width >= 110 && body.height >= 18 {
            let columns = Layout::default().direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)]).split(body);
            if !self.search_text.is_empty() || self.pane == Pane::Search {
                let left = Layout::default().direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(60), Constraint::Percentage(40)]).split(columns[0]);
                self.panel(frame, left[0], Pane::Tree); self.panel(frame, left[1], Pane::Search);
            } else { self.panel(frame, columns[0], Pane::Tree); }
            let right = Layout::default().direction(Direction::Vertical)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)]).split(columns[1]);
            self.panel(frame, right[0], Pane::Preview); self.panel(frame, right[1], Pane::Query);
        } else {
            let columns = Layout::default().direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(52), Constraint::Percentage(48)]).split(body);
            self.panel(frame, columns[0], Pane::Tree); self.panel(frame, columns[1], self.detail);
        }
        let status = self.status.as_ref().map_or_else(|| "No document".into(), |s| {
            if s.state == SessionStatus::Ready { "Ready".into() }
            else { format!("{:?} {}/{} B", s.state, s.done, s.total) }
        });
        let position = self.frame.as_ref().map_or_else(String::new, |f| format!(" | Ln {}/{}{}", f.selected_rank + 1, if f.exact { "" } else { ">=" }, f.total));
        let path = self.frame.as_ref().map_or_else(String::new, |f| safe(&f.selection.path, false));
        let hint = if area.width < 50 { "Tab pane | F1 help | q" } else { match self.pane {
            Pane::Tree => "hjkl move | . path | n/N match | PgUp/Dn tree | F1 help",
            Pane::Preview => "PgUp/Dn scroll | ^D/U half | w wrap | F9 zoom | F1 help",
            Pane::Search | Pane::Query => "PgUp/Dn page | h/l page | Enter tree | n/N search | F1 help",
        }};
        let summary = if footer <= 2 && self.notice != "Ready" && !self.notice.is_empty() {
            format!("[{}] {}", self.pane.name(), self.notice)
        } else { format!("[{}] {status}{position} | {}", self.pane.name(), self.vim.display()) };
        let mut lines = vec![Line::from(fit(&summary, layout[2].width))];
        if footer >= 3 { lines.push(Line::from(fit(&path, layout[2].width))); }
        if footer >= 4 { lines.push(Line::from(fit(&self.notice, layout[2].width))); }
        else if self.notice != "Ready" && !self.notice.is_empty() {
            // Errors and task notices take priority over a path in short windows.
            if footer >= 3 { lines[1] = Line::from(fit(&self.notice, layout[2].width)); }
        }
        if footer > 1 { lines.push(Line::from(fit(hint, layout[2].width))); }
        frame.render_widget(Paragraph::new(lines), layout[2]);
        if self.help { self.draw_help(frame, body); }
        else if let Some(prompt) = &self.prompt {
            let search = matches!(prompt.kind, Input::Search(_));
            let prompt_height = if search && !compact { 6 } else { 4 };
            let popup = Rect::new(area.x, area.bottom().saturating_sub(prompt_height), area.width, prompt_height);
            frame.render_widget(Clear, popup);
            let block = Block::default().borders(Borders::ALL).title(fit(prompt.kind.label(), area.width.saturating_sub(2)))
                .border_style(Style::default().fg(Color::Cyan));
            let inner = block.inner(popup); frame.render_widget(block, popup);
            let available = usize::from(inner.width.saturating_sub(1));
            let mut start = prompt.cursor;
            for (i, _) in prompt.text[..prompt.cursor].char_indices().rev() {
                if prompt.text[i..prompt.cursor].width() > available { break; }
                start = i;
            }
            let mut lines = vec![Line::from(&prompt.text[start..]), Line::from(fit("Enter accept | Esc close | ^U clear", inner.width))];
            if search && !compact {
                lines.push(Line::from(fit(&format!("F6 case:{} F7 regex:{}", self.case, self.regex), inner.width)));
                lines.push(Line::from(fit(&format!("F8 scope:{:?}", self.scope), inner.width)));
            }
            frame.render_widget(Paragraph::new(lines), inner);
            frame.set_cursor_position((inner.x + prompt.text[start..prompt.cursor].width() as u16, inner.y));
        }
    }
    fn buttons(&mut self, frame: &mut Frame<'_>, area: Rect, buttons: &[(String, UiAction)]) {
        let (mut x, mut y) = (area.x, area.y);
        for (label, action) in buttons {
            let text = format!("[{label}] "); let width = (text.width() as u16).min(area.width);
            if x + width > area.right() { x = area.x; y += 1; }
            if y >= area.bottom() { break; }
            let rect = Rect::new(x, y, width, 1);
            frame.render_widget(Paragraph::new(fit(&text, width)).style(Style::default().fg(Color::Cyan)), rect);
            self.hits.push(Hit { area: rect, action: *action }); x += width;
        }
    }
    fn panel(&mut self, frame: &mut Frame<'_>, area: Rect, pane: Pane) {
        if area.height < 3 || area.width < 4 { return; }
        self.panels.push((pane, area)); self.hits.push(Hit { area, action: UiAction::Focus(pane) });
        match pane { Pane::Tree => self.draw_tree(frame, area), Pane::Preview => self.draw_preview(frame, area),
            pane => self.draw_results(frame, area, pane) }
    }
    fn block(&self, pane: Pane, title: String, width: u16) -> Block<'static> {
        Block::default().borders(Borders::ALL).title(fit(&title, width.saturating_sub(2))).border_style(
            Style::default().fg(if self.pane == pane { Color::Cyan } else { Color::DarkGray }))
    }
    fn draw_tree(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = self.block(Pane::Tree, "Tree".into(), area.width);
        let inner = block.inner(area); let rows = inner.height.saturating_sub(1).max(1).min(64);
        if self.viewport != rows {
            self.viewport = rows;
            if let Some(task) = &self.task {
                let result = task.command(Navigation::ViewportRows(u64::from(rows))).and_then(|()| {
                    // Resize keeps the selected row inside the new, smaller viewport.
                    if let Some(frame) = &self.frame {
                        let anchor = if frame.selected_rank < frame.start || frame.selected_rank >= frame.start.saturating_add(u64::from(rows)) {
                            frame.selected_rank.saturating_sub(u64::from(rows) / 4)
                        } else { frame.start };
                        task.command(Navigation::Anchor(anchor))
                    } else { Ok(()) }
                });
                if let Err(error) = result { self.notice = error; }
            }
        }
        let mut lines = Vec::new();
        if let Some(tree) = &self.frame {
            let digits = (tree.selected_rank + 1).to_string().len().clamp(3, 10);
            for (offset, row) in tree.rows.iter().take(usize::from(rows)).enumerate() {
                let selected = row.location.node.reference == tree.selected.node.reference;
                let number = if selected { row.rank + 1 } else { row.rank.abs_diff(tree.selected_rank) };
                let arrow = if row.location.node.container() { if row.expanded { "v" } else { ">" } } else { " " };
                let indent = "  ".repeat(row.location.ancestors.len().min(usize::from(inner.width / 6).min(24)));
                let text = format!("{number:>digits$} {indent}{arrow} {}  {}", safe(&row.label, false), safe(&row.preview, false));
                let mut style = Style::default().fg(match row.location.node.kind {
                    "string" => Color::Green, "number" => Color::Yellow, "boolean" => Color::Magenta,
                    "null" => Color::DarkGray, _ => Color::White,
                });
                if selected { style = style.bg(Color::DarkGray).add_modifier(Modifier::BOLD); }
                lines.push(Line::from(Span::styled(fit(&text, inner.width), style)));
                self.hits.push(Hit { area: Rect::new(inner.x, inner.y + offset as u16, inner.width, 1), action: UiAction::TreeRow(row.rank) });
            }
        } else {
            lines.push(Line::from(fit("Open a JSON file: Ctrl+O", inner.width)));
        }
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), Rect::new(inner.x, inner.y, inner.width, rows));
        frame.render_widget(Paragraph::new(fit("PgUp/PgDn | hjkl | . path", inner.width)).style(Style::default().fg(Color::DarkGray)),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
    }
    fn draw_preview(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = self.block(Pane::Preview, format!("Value preview | wrap:{}", self.preview_wrap), area.width);
        let inner = block.inner(area); let rows = inner.height.saturating_sub(1).max(1);
        let text = self.frame.as_ref().map_or_else(String::new, |f| safe(&f.selection.preview, true));
        let wrapped = preview_lines(&text, inner.width, self.preview_wrap);
        self.preview_rows = wrapped.len(); self.preview_height = rows;
        self.preview_scroll = self.preview_scroll.min(self.preview_rows.saturating_sub(usize::from(rows)));
        let max_x = wrapped.iter().map(|line| line.width()).max().unwrap_or(0).saturating_sub(usize::from(inner.width));
        self.preview_x = self.preview_x.min(max_x);
        let lines: Vec<Line<'_>> = wrapped.iter().skip(self.preview_scroll).take(usize::from(rows))
            .map(|line| Line::from(cell_window(line, self.preview_x, inner.width))).collect();
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), Rect::new(inner.x, inner.y, inner.width, rows));
        let footer = format!("{}-{}/{} | PgUp/PgDn | w wrap", self.preview_scroll + 1,
            (self.preview_scroll + usize::from(rows)).min(self.preview_rows), self.preview_rows);
        frame.render_widget(Paragraph::new(fit(&footer, inner.width)).style(Style::default().fg(Color::DarkGray)),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
    }
    fn draw_results(&mut self, frame: &mut Frame<'_>, area: Rect, pane: Pane) {
        let (results, expression) = if pane == Pane::Query { (&self.query, &self.query_text) } else { (&self.search, &self.search_text) };
        let total = results.count();
        let state = results.state.as_ref().map_or_else(|| if results.waiting { "Waiting".into() } else { "Idle".into() }, |s| format!("{:?}", s.status));
        let elapsed = results.elapsed.map_or_else(String::new, |elapsed| format!(" {:.3}s", elapsed.as_secs_f64()));
        let block = self.block(pane, format!("{} {}/{}", pane.name(), results.selected.map_or(0, |i| i + 1), total), area.width);
        let inner = block.inner(area);
        let footer_rows = if inner.height >= 5 { 2 } else { 1 };
        let header_rows = if inner.height >= 6 { 2 } else { 1 };
        let available = usize::from(inner.height.saturating_sub(footer_rows + header_rows));
        let mut lines = vec![Line::from(fit(&format!("{state}{elapsed} | {}", safe(expression, false)), inner.width))];
        if header_rows == 2 {
            let error = if !results.error.is_empty() { results.error.as_str() }
                else { results.state.as_ref().map_or("", |s| s.error.as_str()) };
            lines.push(Line::from(Span::styled(fit(error, inner.width), Style::default().fg(Color::Red))));
        }
        if let Some(page) = &results.page {
            let offset = results.selected.unwrap_or(page.start).saturating_sub(page.start) as usize;
            let skip = offset.saturating_add(1).saturating_sub(available);
            for row in page.rows.iter().skip(skip).take(available) {
                let style = if results.selected == Some(row.index) { Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD) } else { Style::default() };
                let path_width = (inner.width * 2 / 3).max(1);
                let text = format!("{} {} {}", row.index + 1, fit(&safe(&row.path, false), path_width), safe(&row.preview, false));
                let y = inner.y + lines.len() as u16;
                lines.push(Line::from(Span::styled(fit(&text, inner.width), style)));
                self.hits.push(Hit { area: Rect::new(inner.x, y, inner.width, 1), action: UiAction::Result(pane, row.index) });
            }
        } else if total > 0 { lines.push(Line::from("Loading page...")); }
        let pages = total.div_ceil(PAGE);
        let summary = format!("Page {}/{} | {}-{}/{}", if total == 0 { 0 } else { results.start / PAGE + 1 }, pages,
            if total == 0 { 0 } else { results.start + 1 }, (results.start + PAGE).min(total), total);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(footer_rows)));
        if footer_rows == 2 {
            frame.render_widget(Paragraph::new(fit(&summary, inner.width)).style(Style::default().fg(Color::DarkGray)),
                Rect::new(inner.x, inner.bottom() - 2, inner.width, 1));
        }
        let pager = vec![("< PgUp".into(), UiAction::Page(pane, true)), ("PgDn >".into(), UiAction::Page(pane, false))];
        self.buttons(frame, Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1), &pager);
    }
    fn draw_help(&mut self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL).title(fit("Help | PgUp/PgDn | F1/Esc close", area.width.saturating_sub(2)))
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        self.help_height = inner.height;
        let lines = preview_lines(HELP, inner.width, true);
        self.help_scroll = usize::from(self.help_scroll).min(lines.len().saturating_sub(usize::from(inner.height))) as u16;
        let visible: Vec<Line<'_>> = lines.iter().skip(usize::from(self.help_scroll)).take(usize::from(inner.height))
            .map(|line| Line::from(line.as_str())).collect();
        frame.render_widget(block, area); frame.render_widget(Paragraph::new(visible), inner);
    }
}
impl Drop for App {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() { drop(task); }
        if let Some(thread) = self.retiring.take() { crate::shutdown::defer(thread); }
    }
}

// JSON previews, paths, filenames, and errors may contain terminal escape/control
// characters. Only the frontend's own terminal commands reach the terminal raw.
fn terminal_notice(message: String) -> String {
    if message == "Path selected multiple nodes; use --query to retrieve all results" {
        "Path needs one node. Use F2 or : for JSONPath; Alt+4 shows results.".into()
    } else {
        message
    }
}
fn safe(text: &str, multiline: bool) -> String {
    let mut output = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' && multiline { output.push(c); }
        else if c.is_control() { output.extend(c.escape_default()); }
        else { output.push(c); }
    }
    output
}
fn fit(text: &str, width: u16) -> String {
    let text = safe(text, false);
    let width = usize::from(width);
    if text.width() <= width { return text; }
    if width == 0 { return String::new(); }
    let mut output = String::new(); let mut cells = 0;
    for c in text.chars() {
        let size = c.width().unwrap_or(0);
        if cells + size > width - 1 { break; }
        output.push(c); cells += size;
    }
    output.push('\u{2026}'); output
}
fn cell_window(text: &str, start: usize, width: u16) -> String {
    let mut output = String::new(); let mut cells = 0; let mut used = 0;
    for c in text.chars() {
        let size = c.width().unwrap_or(0);
        if cells < start { cells += size; continue; }
        if used + size > usize::from(width) { break; }
        output.push(c); used += size;
    }
    output
}
fn preview_lines(text: &str, width: u16, wrap: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for line in text.split('\n') {
        if !wrap { lines.push(line.to_string()); continue; }
        let mut output = String::new(); let mut cells = 0;
        for c in line.chars() {
            let size = c.width().unwrap_or(0);
            if cells + size > usize::from(width.max(1)) && !output.is_empty() {
                lines.push(std::mem::take(&mut output)); cells = 0;
            }
            output.push(c); cells += size;
        }
        lines.push(output);
    }
    lines
}
fn button_rows(buttons: &[(String, UiAction)], width: u16) -> u16 {
    let (mut rows, mut used) = (1, 0);
    for (label, _) in buttons {
        let cells = (label.width() as u16 + 3).min(width);
        if used + cells > width { rows += 1; used = 0; }
        used += cells;
    }
    rows
}
// Explicit copy actions use the terminal clipboard protocol, with no X11 or
// Wayland service. Do not query clipboard contents or truncate copied JSON.
fn copy_terminal(output: &mut impl Write, text: &str) -> Result<(), String> {
    if text.len() > 100 * 1024 { return Err("Terminal copy exceeds 100 KiB; use e to export the complete value".into()); }
    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(text.len().div_ceil(3) * 4);
    for chunk in text.as_bytes().chunks(3) {
        let (a, b, c) = (chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0));
        encoded.push(BASE64[(a >> 2) as usize] as char);
        encoded.push(BASE64[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 { BASE64[(((b & 15) << 2) | (c >> 6)) as usize] as char } else { '=' });
        encoded.push(if chunk.len() > 2 { BASE64[(c & 63) as usize] as char } else { '=' });
    }
    write!(output, "\x1b]52;c;{encoded}\x07").and_then(|()| output.flush()).map_err(|e| e.to_string())
}

const HELP: &str = "FerrisJson TUI (Linux / macOS) - keyboard and mouse\n\n\
Tab / Shift+Tab: cycle Tree, Preview, Search, JSONPath. Alt+1/2/3/4: choose a pane.\n\
Click a pane/tab to focus. Mouse wheel scrolls under the pointer. F9: maximize/restore focused pane.\n\
Wide: tree + search on the left, preview + JSONPath on the right. Medium: tree + chosen detail pane. Small: one pane.\n\n\
Shared desktop shortcuts:\n\
Ctrl+O / o: open file. F5: reload current file (stdin/paste must be supplied again).\n\
. / p: locate ONE node. Multiple matches: use : / F2 for JSONPath, then Alt+4 for results.\n\
/ / Ctrl+F: forward search; ?: backward search. Enter runs.\n\
n / N: next / opposite search match in ALL panes, following the / or ? direction. Numeric prefixes work.\n\
F3 / Shift+F3: forward / backward search match. F4 / Shift+F4: next / previous JSONPath result.\n\
F6: case; F7: regex; F8: All/Keys/Values. Changing options clears old search; / then Enter reruns.\n\
hjkl / arrows: tree navigation. gg / G: first / last visible row. 20j / 30gg: numeric prefixes.\n\
za / Space / Enter: toggle. zo / zc: open / close. zR / zM: expand / collapse all.\n\
[ / ]: history (prefix supported). H / M / L: tree top/middle/bottom of screen.\n\
yy / Ctrl+C: copy selected JSON; yp / Ctrl+P: copy path. Terminal must support OSC 52.\n\
Terminal copy limit: 100 KiB, no truncation; use export for larger values. Multiplexer settings may affect copy.\n\
Paste with terminal paste (usually Ctrl+Shift+V / Shift+Insert): edit active input, or load JSON outside input.\n\n\
Paging and scrolling (focus with Tab, Alt+number, or mouse):\n\
Preview: PgUp/PgDn or Ctrl+B/E scroll one visible page; Ctrl+U/D half page; j/k one line.\n\
Preview gg/G or Home/End: top/bottom. w: wrap on/off; h/l scroll horizontally when wrap is off.\n\
Search / JSONPath: PgUp/PgDn, Ctrl+B/E, h/l, or clickable page buttons: previous/next 16-result page, stops at ends.\n\
Result j/k or arrows: select one result (stops at ends); gg/G or Home/End: first/last; Enter: return to tree.\n\
Tree: PgUp/PgDn or Ctrl+B/E one viewport; Ctrl+U/D half viewport. Numeric prefixes multiply the movement.\n\
Page numbers and visible ranges appear inside panels; small panes scroll within a result page to reveal selection.\n\n\
e: export node, or all results of focused Search/JSONPath pane. E: explicitly export committed partial results.\n\
Export replaces destination atomically on success. Delete in results: clear that task/results.\n\
Esc: close input/help, otherwise return to Tree and cancel pending navigation. Ctrl+X / Stop: cancel focused task.\n\
q / Ctrl+Q: quit. Ctrl+C copies (same as desktop). External SIGINT still exits and restores the terminal.\n\
Input: arrows/Home/End, Backspace/Delete, Ctrl+U. Bracketed paste never submits an input automatically.\n\n\
Queries: $.items[*].name | $..price | $.items[?@.active == true] | /items/0\n\
Memory supports library advanced syntax; Indexed/Sparse retain existing bounded syntax limits.\n\
--indexed: file snapshot. --sparse: experimental progressive backend. --cache-dir requires --sparse.\n\
--paged-tree uses the same TUI. --sparse-pages is desktop-only. Stdin/paste use Memory.\n\
TUI has no fixed memory quota; --sparse avoids retaining the full Value. Preview is bounded; export reads full values.\n\
No controlling terminal: use --get / --query / --search / --pretty. Unicode and function keys depend on terminal/SSH.";
