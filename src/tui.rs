//! Linux terminal frontend. All file, tree, search, query and output work stays
//! on the existing bounded worker lanes; the UI only holds presentation pages.
use crate::{
    document::SearchScope,
    engine::{results::{State, Status as ResultStatus}, search::Query,
        session::SessionStatus, sparse::OpenOptions, tree::TreeFrame,
        tree_navigation::Navigation},
    terminal_wake::Context,
    tree_worker::{Backend, ResultPage, Status, Task},
    vim::{Action, Vim},
};
use crossterm::{
    cursor::{Hide, Show}, event::{self, DisableBracketedPaste, EnableBracketedPaste,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers}, execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style}, text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap}, Frame, Terminal};
use std::{fs::{File, OpenOptions as FileOptions}, io::{self, IsTerminal}, path::PathBuf,
    sync::{Arc, atomic::{AtomicBool, Ordering}}, thread::JoinHandle, time::Duration};
use unicode_width::UnicodeWidthStr;

const INPUT_LIMIT: usize = 64 * 1024;
const PAGE: u64 = 16;

fn tty() -> io::Result<File> {
    FileOptions::new().read(true).write(true).open("/dev/tty")
}
fn restore_terminal() {
    let _ = disable_raw_mode();
    if let Ok(mut output) = tty() {
        let _ = execute!(output, DisableBracketedPaste, LeaveAlternateScreen, Show);
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
    let output = tty().map_err(|_| "Linux TUI requires an interactive terminal (/dev/tty); use --get, --query, --search or --pretty for noninteractive output".to_string())?;
    if !output.is_terminal() || std::env::var("TERM").is_ok_and(|v| v == "dumb") {
        return Err("Linux TUI requires an ANSI terminal; use CLI output options in this environment".into());
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
    execute!(terminal.backend_mut(), EnterAlternateScreen, EnableBracketedPaste, Hide).map_err(|e| e.to_string())?;
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
                        if let Some(prompt) = &mut app.prompt { prompt.insert(&text); }
                    }
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
            Self::Locate => "Locate path", Self::Export(_, false) => "Export to (replaces destination)",
            Self::Export(_, true) => "Export committed partial results to (replaces destination)" }
    }
}
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
struct App {
    task: Option<Task>, retiring: Option<JoinHandle<()>>, pending: Option<(PathBuf, Backend)>,
    wake: Context, indexed: bool, sparse: bool, cache_dir: Option<PathBuf>,
    title: String, status: Option<Status>, frame: Option<TreeFrame>, pane: Pane,
    search: Results, query: Results, search_text: String, query_text: String,
    case: bool, regex: bool, scope: SearchScope, prompt: Option<Prompt>, help: bool, help_scroll: u16,
    notice: String, vim: Vim, preview_scroll: u16, viewport: u16,
}
impl App {
    fn new(indexed: bool, sparse: bool, cache_dir: Option<PathBuf>, wake: Context) -> Self {
        Self { task: None, retiring: None, pending: None, wake, indexed, sparse, cache_dir,
            title: "No file".into(), status: None, frame: None, pane: Pane::Tree,
            search: Results::default(), query: Results::default(), search_text: String::new(),
            query_text: String::new(), case: false, regex: false, scope: SearchScope::All,
            prompt: None, help: false, help_scroll: 0, notice: "o: open file   F1: help   q: quit".into(),
            vim: Vim::default(), preview_scroll: 0, viewport: 0 }
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
        self.title = path.display().to_string(); self.pending = Some((path, backend));
        self.frame = None; self.status = None; self.search = Results::default(); self.query = Results::default();
        self.search_text.clear(); self.query_text.clear(); self.preview_scroll = 0; self.viewport = 0;
        self.vim.cancel(); self.pane = Pane::Tree; self.notice = "Opening… Esc: cancel".into();
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
        // The terminal frontend has no desktop clipboard. Drain transfers here
        // so their strings and budget reservations are released immediately.
        drop(poll.query_clipboard);
        drop(poll.clipboard);
        drop(poll.path_copy);
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
                    }
                    self.frame = Some(frame);
                }
                Ok(_) => (), Err(error) => self.notice = error,
            }
        }
        if !poll.status.message.is_empty() { self.notice = poll.status.message.clone(); }
        if !poll.notice.is_empty() { self.notice = poll.notice; }
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
        if text.is_empty() { return Err("Input is empty".into()); }
        if matches!(prompt.kind, Input::Open) { return self.open(PathBuf::from(text)); }
        let task = self.task.as_ref().ok_or("Open a file first")?;
        if self.status.as_ref().is_some_and(|s| matches!(s.state, SessionStatus::Failed | SessionStatus::Cancelled)) {
            return Err("Session unavailable; open a file to start again".into());
        }
        match prompt.kind {
            Input::Open => unreachable!(),
            Input::Search(reverse) => {
                task.search_query(Query { term: text.clone(), case: self.case, regex: self.regex, scope: self.scope }, reverse)?;
                self.search_text = text; self.search = Results::default(); self.pane = Pane::Search;
            }
            Input::Query => {
                task.query_request(text.clone())?; self.query_text = text;
                self.query = Results::default(); self.pane = Pane::Query;
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
    fn key(&mut self, key: KeyEvent) -> Result<bool, String> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') { return Ok(true); }
        if key.code == KeyCode::F(1) { self.help = !self.help; self.help_scroll = 0; self.vim.cancel(); return Ok(false); }
        if self.help {
            match key.code {
                KeyCode::Esc => self.help = false,
                KeyCode::Down | KeyCode::Char('j') => self.help_scroll = self.help_scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => self.help_scroll = self.help_scroll.saturating_sub(1),
                KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(self.viewport),
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(self.viewport),
                KeyCode::Home => self.help_scroll = 0,
                _ => (),
            }
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.begin(Input::Open, String::new()); return Ok(false);
        }
        if let Some(mut prompt) = self.prompt.take() {
            match key.code {
                KeyCode::Esc => return Ok(false),
                KeyCode::Enter => { self.submit(prompt)?; return Ok(false); }
                KeyCode::Left => prompt.left(), KeyCode::Right => prompt.right(),
                KeyCode::Home => prompt.cursor = 0, KeyCode::End => prompt.cursor = prompt.text.len(),
                KeyCode::Backspace if prompt.cursor > 0 => {
                    let end = prompt.cursor; prompt.left(); prompt.text.replace_range(prompt.cursor..end, "");
                }
                KeyCode::Delete if prompt.cursor < prompt.text.len() => {
                    let start = prompt.cursor; prompt.right(); prompt.text.replace_range(start..prompt.cursor, ""); prompt.cursor = start;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => { prompt.text.clear(); prompt.cursor = 0; }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => prompt.insert(&c.to_string()),
                KeyCode::F(5) => self.toggle_search(5), KeyCode::F(6) => self.toggle_search(6), KeyCode::F(7) => self.toggle_search(7),
                _ => (),
            }
            self.prompt = Some(prompt); return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('o') => self.begin(Input::Open, String::new()),
            KeyCode::Char('/') => self.begin(Input::Search(false), self.search_text.clone()),
            KeyCode::Char('?') => self.begin(Input::Search(true), self.search_text.clone()),
            KeyCode::Char(':') | KeyCode::F(2) => self.begin(Input::Query, self.query_text.clone()),
            KeyCode::Char('p') => self.begin(Input::Locate, String::new()),
            KeyCode::Char('e' | 'E') => self.begin(Input::Export(self.pane, key.code == KeyCode::Char('E')), String::new()),
            KeyCode::Tab => { self.pane = self.pane.next(); self.viewport = 0; self.vim.cancel(); }
            KeyCode::BackTab => { self.pane = self.pane.next().next().next(); self.viewport = 0; self.vim.cancel(); }
            KeyCode::F(3) => { self.step_result(Pane::Search, key.modifiers.contains(KeyModifiers::SHIFT), 1)?; }
            KeyCode::F(4) => { self.step_result(Pane::Query, key.modifiers.contains(KeyModifiers::SHIFT), 1)?; }
            KeyCode::F(n @ 5..=7) => self.toggle_search(n),
            KeyCode::Esc => {
                self.vim.cancel();
                if self.pending.is_some() { self.pending = None; self.notice = "Open cancelled".into(); }
                if let Some(task) = &self.task {
                    if self.status.as_ref().is_some_and(|s| matches!(s.state, SessionStatus::Preparing | SessionStatus::Validating)) { task.cancel(); }
                    else { task.search_clear(true); task.query_clear(false); let _ = task.command(Navigation::CancelPending); }
                }
                self.notice = "Cancelled; committed search/query results remain available".into();
            }
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
            5 => self.case = !self.case, 6 => self.regex = !self.regex,
            _ => self.scope = match self.scope { SearchScope::All => SearchScope::Keys, SearchScope::Keys => SearchScope::Values, SearchScope::Values => SearchScope::All },
        }
        if let Some(task) = &self.task { task.search_clear(false); }
        self.search = Results::default(); self.notice = "Search options changed; / then Enter to search again".into();
    }
    fn navigation_key(&mut self, key: KeyEvent) -> Result<(), String> {
        let page = u64::from(self.viewport.max(1));
        if matches!(self.pane, Pane::Search | Pane::Query) {
            let delta = match key.code { KeyCode::Down | KeyCode::Char('j') => Some((false, 1)),
                KeyCode::Up | KeyCode::Char('k') => Some((true, 1)), KeyCode::PageDown => Some((false, PAGE)),
                KeyCode::PageUp => Some((true, PAGE)), _ => None };
            if let Some((backwards, count)) = delta { return self.step_result(self.pane, backwards, count); }
            let total = self.results(self.pane).count();
            if total > 0 {
                match key.code {
                    KeyCode::Home | KeyCode::Char('g') => return self.select_result(self.pane, 0),
                    KeyCode::End | KeyCode::Char('G') => return self.select_result(self.pane, total - 1),
                    KeyCode::Enter => { let index = self.results(self.pane).selected.unwrap_or(0); self.select_result(self.pane, index)?; self.pane = Pane::Tree; }
                    _ => (),
                }
            }
            return Ok(());
        }
        if self.pane == Pane::Preview {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => self.preview_scroll = self.preview_scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => self.preview_scroll = self.preview_scroll.saturating_sub(1),
                KeyCode::PageDown => self.preview_scroll = self.preview_scroll.saturating_add(page as u16),
                KeyCode::PageUp => self.preview_scroll = self.preview_scroll.saturating_sub(page as u16),
                KeyCode::Home | KeyCode::Char('g') => self.preview_scroll = 0,
                _ => (),
            }
            return Ok(());
        }
        let command = match key.code {
            KeyCode::Up => Some(Navigation::Move(-1)), KeyCode::Down => Some(Navigation::Move(1)),
            KeyCode::Left => Some(Navigation::Left(1)), KeyCode::Right => Some(Navigation::Right(1)),
            KeyCode::Home => Some(Navigation::First), KeyCode::End => Some(Navigation::Last),
            KeyCode::PageDown => Some(Navigation::Move(page as i64)), KeyCode::PageUp => Some(Navigation::Move(-(page as i64))),
            KeyCode::Enter | KeyCode::Char(' ') => Some(Navigation::Toggle),
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                if let Some(cmd) = self.vim.feed(c) {
                    let count = cmd.count as u64;
                    match cmd.action {
                        Action::Move(delta) => Some(Navigation::Move(delta as i64 * count as i64)),
                        Action::First => Some(if cmd.explicit_count { Navigation::Row(count - 1) } else { Navigation::First }),
                        Action::Last => Some(if cmd.explicit_count { Navigation::Row(count - 1) } else { Navigation::Last }),
                        Action::Left => Some(Navigation::Left(count)), Action::Right => Some(Navigation::Right(count)),
                        Action::Toggle => Some(Navigation::Toggle), Action::Open => Some(Navigation::Open), Action::Close => Some(Navigation::Close),
                        Action::ExpandAll => Some(Navigation::ExpandAll), Action::CollapseAll => Some(Navigation::CollapseAll),
                        Action::History(back) => Some(Navigation::History(back)), Action::Screen(c) => Some(Navigation::Screen(c)),
                        Action::Match(back) => { self.step_result(Pane::Search, back, count)?; None }
                        Action::Copy(_) => { self.notice = "Use e to export; system clipboard is not required in the TUI".into(); None }
                        _ => None,
                    }
                } else { None }
            }
            _ => None,
        };
        if let Some(command) = command { self.navigate(command)?; }
        Ok(())
    }
    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.width < 24 || area.height < 8 {
            frame.render_widget(Paragraph::new("Terminal too small. Resize to at least 24×8. Ctrl+C: quit"), area);
            return;
        }
        let layout = Layout::default().direction(Direction::Vertical).constraints([
            Constraint::Length(2), Constraint::Min(1), Constraint::Length(3),
        ]).split(area);
        let backend = if self.sparse { "Sparse" } else if self.indexed { "Indexed" } else { "Memory" };
        let flags = format!("F5 case:{}  F6 regex:{}  F7 scope:{:?}", self.case, self.regex, self.scope);
        frame.render_widget(Paragraph::new(vec![
            Line::from(format!("FerrisJson {} · Linux TUI · {} · {}", env!("CARGO_PKG_VERSION"), backend, safe(&self.title, false))),
            Line::from(format!("Tab: pane · o: open · /: search · F2: JSONPath · e: export · F1: help · {flags}")),
        ]), layout[0]);
        let body = layout[1];
        let height = body.height.saturating_sub(2).clamp(1, 64);
        if self.viewport != height {
            self.viewport = height;
            if let Some(task) = &self.task {
                if let Err(error) = task.command(Navigation::ViewportRows(u64::from(height))) { self.notice = error; }
            }
        }
        if body.width >= 100 {
            let columns = Layout::default().direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)]).split(body);
            self.draw_tree(frame, columns[0]);
            if matches!(self.pane, Pane::Search | Pane::Query) { self.draw_results(frame, columns[1], self.pane); }
            else { self.draw_preview(frame, columns[1]); }
        } else {
            match self.pane { Pane::Tree => self.draw_tree(frame, body), Pane::Preview => self.draw_preview(frame, body),
                pane => self.draw_results(frame, body, pane) }
        }
        let status = self.status.as_ref().map_or_else(|| "No document".into(), |s|
            format!("{:?} · {} / {} bytes", s.state, s.done, s.total));
        let position = self.frame.as_ref().map_or_else(String::new, |f| format!(" · Ln {}/{}{} · {}",
            f.selected_rank + 1, if f.exact { "" } else { "≥" }, f.total, safe(&f.selection.path, false)));
        frame.render_widget(Paragraph::new(vec![
            Line::from(format!("[{}] {status}{position}", self.pane.name())),
            Line::from(safe(&self.notice, false)),
            Line::from(format!("{} · Esc: cancel task · Delete: clear results · q: quit", self.vim.display())),
        ]), layout[2]);
        if self.help { self.draw_help(frame, body); }
        else if let Some(prompt) = &self.prompt {
            let popup = Rect { x: area.x, y: area.y + area.height.saturating_sub(5), width: area.width, height: 5 };
            frame.render_widget(Clear, popup);
            let block = Block::default().borders(Borders::ALL).title(prompt.kind.label()).border_style(Style::default().fg(Color::Cyan));
            let inner = block.inner(popup); frame.render_widget(block, popup);
            // Keep the caret visible without cutting through a UTF-8 codepoint.
            let available = usize::from(inner.width.saturating_sub(1));
            let mut start = prompt.cursor;
            for (i, _) in prompt.text[..prompt.cursor].char_indices().rev() {
                if prompt.text[i..prompt.cursor].width() > available { break; }
                start = i;
            }
            frame.render_widget(Paragraph::new(vec![Line::from(&prompt.text[start..]),
                Line::from("Enter: accept · Esc: close · Ctrl+U: clear"), Line::from(flags)]), inner);
            frame.set_cursor_position((inner.x + prompt.text[start..prompt.cursor].width() as u16, inner.y));
        }
    }
    fn block(&self, pane: Pane, title: String) -> Block<'static> {
        Block::default().borders(Borders::ALL).title(title).border_style(
            Style::default().fg(if self.pane == pane { Color::Cyan } else { Color::DarkGray }))
    }
    fn draw_tree(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = self.block(Pane::Tree, "Tree · hjkl / gg / G / za / zR / zM".into());
        let inner = block.inner(area);
        let mut lines = Vec::new();
        if let Some(tree) = &self.frame {
            for row in tree.rows.iter().take(usize::from(inner.height)) {
                let selected = row.location.node.reference == tree.selected.node.reference;
                let number = if selected { row.rank + 1 } else { row.rank.abs_diff(tree.selected_rank) };
                let arrow = if row.location.node.container() { if row.expanded { "▾" } else { "▸" } } else { " " };
                let indent = "  ".repeat(row.location.ancestors.len().min(32));
                let text = format!("{number:>6} {indent}{arrow} {}  {}", safe(&row.label, false), safe(&row.preview, false));
                let mut style = Style::default().fg(match row.location.node.kind {
                    "string" => Color::Green, "number" => Color::Yellow, "boolean" => Color::Magenta,
                    "null" => Color::DarkGray, _ => Color::White,
                });
                if selected { style = style.bg(Color::DarkGray).add_modifier(Modifier::BOLD); }
                lines.push(Line::from(Span::styled(text, style)));
            }
        } else {
            lines.push(Line::from("o: open JSON file · F1: help"));
            lines.push(Line::from("Large files: start with --sparse"));
        }
        frame.render_widget(block, area); frame.render_widget(Paragraph::new(lines), inner);
    }
    fn draw_preview(&self, frame: &mut Frame<'_>, area: Rect) {
        let text = self.frame.as_ref().map_or_else(String::new, |f| safe(&f.selection.preview, true));
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false })
            .scroll((self.preview_scroll, 0)).block(self.block(Pane::Preview, "Value preview · Tab to focus / scroll · e: export".into())), area);
    }
    fn draw_results(&self, frame: &mut Frame<'_>, area: Rect, pane: Pane) {
        let (results, expression) = if pane == Pane::Query { (&self.query, &self.query_text) } else { (&self.search, &self.search_text) };
        let state = results.state.as_ref().map_or_else(|| if results.waiting { "Waiting".into() } else { "Idle".into() }, |s| format!("{:?}", s.status));
        let elapsed = results.elapsed.map_or_else(String::new, |elapsed| format!(" · {:.3} s", elapsed.as_secs_f64()));
        let block = self.block(pane, format!("{} · {} / {} · {state}{elapsed}", pane.name(), results.selected.map_or(0, |i| i + 1), results.count()));
        let inner = block.inner(area);
        let mut lines = vec![Line::from(safe(expression, false))];
        if !results.error.is_empty() { lines.push(Line::from(Span::styled(safe(&results.error, false), Style::default().fg(Color::Red)))); }
        if let Some(state) = &results.state && !state.error.is_empty() {
            lines.push(Line::from(Span::styled(safe(&state.error, false), Style::default().fg(Color::Red))));
        }
        let available = usize::from(inner.height).saturating_sub(lines.len());
        if let Some(page) = &results.page {
            let offset = results.selected.unwrap_or(page.start).saturating_sub(page.start) as usize;
            let skip = offset.saturating_add(1).saturating_sub(available);
            for row in page.rows.iter().skip(skip).take(available) {
                let style = if results.selected == Some(row.index) { Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD) } else { Style::default() };
                lines.push(Line::from(Span::styled(format!("{} {}  {}", row.index + 1,
                    safe(&row.path, false), safe(&row.preview, false)), style)));
            }
        } else if results.count() > 0 { lines.push(Line::from("Loading result page…")); }
        frame.render_widget(block, area); frame.render_widget(Paragraph::new(lines), inner);
    }
    fn draw_help(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(HELP).wrap(Wrap { trim: false }).scroll((self.help_scroll, 0))
            .block(Block::default().borders(Borders::ALL).title("Help · ↑↓ / PgUp PgDn: scroll · F1 / Esc: close")
                .border_style(Style::default().fg(Color::Cyan))), area);
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
fn safe(text: &str, multiline: bool) -> String {
    let mut output = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' && multiline { output.push(c); }
        else if c.is_control() { output.extend(c.escape_default()); }
        else { output.push(c); }
    }
    output
}
const HELP: &str = "FerrisJson Linux TUI\n\n\
Tab / Shift+Tab: switch Tree, Preview, Search, JSONPath. Narrow terminals show one pane.\n\
o / Ctrl+O: open a file by path. Paths are literal; shell expansion is not performed.\n\
Arrow keys / hjkl: move, collapse, expand. Enter / Space / za: toggle container.\n\
gg / G: first / last visible row. 20j, 10k, 30gg: numeric Vim commands.\n\
zR / zM: expand / collapse all. [ / ]: history. H / M / L: screen position.\n\
PgUp / PgDn: page. Preview focus: j/k or arrows to scroll, Home: top.\n\
/ / ?: forward / backward search. Enter runs, F5: case, F6: regex, F7: All/Keys/Values.\n\
F3 / Shift+F3, n / N in Tree: next / previous search match.\n\
: / F2: JSONPath or JSON Pointer; Enter runs. F4 / Shift+F4: next / previous query result.\n\
Results focus: arrows / j/k select, PgUp/PgDn move 16 results, Enter reveals in Tree.\n\
e: export selected value, or all results of the focused Search/JSONPath pane.\n\
E: explicitly export committed partial results of the focused results pane.\n\
Destination paths replace existing files atomically on success. No desktop clipboard is required.\n\
Esc: close input/help first; otherwise cancel loading, navigation, search and query tasks.\n\
Delete in a results pane: clear its task and results. q / Ctrl+C: quit.\n\
Input: arrows, Home/End, Backspace/Delete, Ctrl+U; bracketed paste inserts text without executing.\n\n\
Query examples: $.items[*].name  |  $..price  |  $.items[?@.active == true]  |  /items/0\n\
Memory supports library advanced queries; Indexed/Sparse retain existing bounded syntax limits.\n\
--indexed: file snapshot backend. --sparse: experimental progressive large-file backend.\n\
--cache-dir DIR requires --sparse. --paged-tree uses the same terminal tree.\n\
--sparse-pages is a desktop-only legacy fallback. Stdin uses Memory; file backends require files.\n\
The Memory task budget is 256 MiB (not an RSS cap); use --sparse for large files.\n\
Source validation must complete before queries/full export. Preview text can be truncated; export reads the value.\n\
Unicode widths, emoji, colors and function keys depend on your terminal and SSH configuration.\n\
No controlling terminal: use --get / --query / --search / --pretty for CLI output.";
