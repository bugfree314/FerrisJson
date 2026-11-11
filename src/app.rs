//! Default viewer: Memory and Indexed documents with a paged tree,
//! selection inspector and JSONPath result panel. Rendered with gpui.
use crate::engine::budget::{Reservation, Resource};
use crate::engine::file_source::{Cancellation, Priority, with_io_context};
use crate::engine::query::QueryEngine;
use crate::preview_worker::{Output as PreviewOutput, PreviewWorker, ROW_BATCH};
use crate::ui::{self, InputEvent};
use crate::vim::{Action, Command, Vim};
use crate::{
    document::{Document, LoadProgress, NodeIds, SearchScope},
    export::{self, COPY_LIMIT, CopyError, Selection},
    legacy_search::{SearchRequest, SearchResults, SearchWorker},
    palette::{ColorSettings, Palette, Preset, color},
    query_worker::{QueryRequest, QueryWorker},
    row_cache::RowCache,
};
use gpui::prelude::*;
use gpui::*;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

type LoadResult = Result<(Document, String, Option<PathBuf>), String>;
struct PathResults {
    expression: String,
    store: Option<crate::engine::results::ResultStore>,
    start: u64,
    ids: Arc<NodeIds>,
    preview: String,
    error: Option<String>,
}
struct CopyRequest {
    doc: Arc<Document>,
    selection: Selection,
}

pub struct Viewer {
    doc: Option<Arc<Document>>,
    preview_worker: Option<PreviewWorker>,
    requested_rows: Vec<usize>,
    search_worker: Option<SearchWorker>,
    path_worker: Option<QueryWorker>,
    search_due: Option<Instant>,
    search_highlight: Option<ui::SearchHighlight>,
    pending_expression: String,
    row_labels: RowCache,
    pending_export: Option<Receiver<Result<PathBuf, String>>>,
    export_thread: Option<std::thread::JoinHandle<()>>,
    export_cancelled: Arc<AtomicBool>,
    large_copy: Option<CopyRequest>,
    selected_path: String,
    source: String,
    file: Option<PathBuf>,
    pending: Option<Receiver<LoadResult>>,
    waiting_for_old: Arc<AtomicBool>,
    loading_cancelled: Arc<AtomicBool>,
    indexed_progress: Arc<LoadProgress>,
    indexing: bool,
    load_started: Option<Instant>,
    open_duration: Option<Duration>,
    search_duration: Option<Duration>,
    path_duration: Option<Duration>,
    expanded: Vec<bool>,
    expanded_resource: Option<Reservation>,
    preview_resource: Option<Reservation>,
    selected: usize,
    rows: NodeIds,
    matches: SearchResults,
    match_index: Option<usize>,
    term: String,
    case: bool,
    regex: bool,
    scope: SearchScope,
    query: String,
    path_results: Option<PathResults>,
    history: Vec<usize>,
    history_index: usize,
    vim: Vim,
    search_reverse: bool,
    search_origin: usize,
    inspector_ratio: f32,
    dragging_split: Option<(f32, f32)>,
    viewport_rows: usize,
    error: Option<String>,
    search_error: Option<String>,
    notice: String,
    paste_open: bool,
    paste: String,
    paste_recovery: Option<Receiver<String>>,
    help: bool,
    jsonpath_help: bool,
    colors_open: bool,
    color_target: usize,
    focus_search: bool,
    focus_query: bool,
    pending_blur: bool,
    sync_query_input: bool,
    scroll_to: bool,
    detail: String,
    colors: ColorSettings,
    focus_handle: FocusHandle,
    wake: crate::repaint::Context,
    search_input: Entity<ui::TextInput>,
    query_input: Entity<ui::TextInput>,
    hex_input: Entity<ui::TextInput>,
    search_sub: Option<Subscription>,
    query_sub: Option<Subscription>,
    hex_sub: Option<Subscription>,
    tree_scroll: ScrollHandle,
    search_generation: u64,
    export_ticker: bool,
}

impl Viewer {
    pub fn new(
        _window: &mut Window,
        cx: &mut Context<Self>,
        file: Option<PathBuf>,
        text: Option<String>,
        indexed: bool,
    ) -> Self {
        let mut app = Self::empty(cx);
        match ColorSettings::load() {
            Ok(settings) => app.colors = settings,
            Err(error) => app.error = Some(error),
        }
        if indexed {
            app.colors.indexed_files = true;
        }

        let (wake_tx, wake_rx) = async_channel::unbounded::<()>();
        app.wake = crate::repaint::Context::with_notify(Arc::new(move || {
            let _ = wake_tx.try_send(());
        }));
        cx.spawn(async move |this, cx| {
            while wake_rx.recv().await.is_ok() {
                if this
                    .update(cx, |this, cx| {
                        this.tick(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let search_input = app.search_input.clone();
        app.search_sub = Some(cx.subscribe(
            &search_input,
            |this: &mut Self, _input, event: &InputEvent, cx| {
                match event {
                    InputEvent::Changed(text) => {
                        this.term = text.to_string();
                        this.schedule_search(cx);
                    }
                    InputEvent::Submitted => {
                        this.search();
                        this.pending_blur = true;
                    }
                    InputEvent::Escape => {
                        this.vim.cancel();
                        this.pending_blur = true;
                    }
                }
                cx.notify();
            },
        ));
        let query_input = app.query_input.clone();
        app.query_sub = Some(cx.subscribe(
            &query_input,
            |this: &mut Self, _input, event: &InputEvent, cx| {
                match event {
                    InputEvent::Changed(text) => {
                        this.query = text.to_string();
                        if this.query.trim().is_empty() {
                            this.path_results = None;
                            if let Some(worker) = &mut this.path_worker {
                                worker.cancel();
                            }
                            this.path_duration = None;
                        }
                    }
                    InputEvent::Submitted => {
                        this.go_to_path();
                        this.pending_blur = true;
                    }
                    InputEvent::Escape => {
                        this.vim.cancel();
                        this.pending_blur = true;
                    }
                }
                cx.notify();
            },
        ));
        let hex_input = app.hex_input.clone();
        app.hex_sub = Some(cx.subscribe(
            &hex_input,
            |this: &mut Self, _input, event: &InputEvent, cx| {
                if let InputEvent::Changed(text) = event
                    && let Some(rgb) = parse_hex(text)
                {
                    this.colors.customize();
                    if this.colors.dark {
                        *palette_slot(&mut this.colors.custom_dark, this.color_target) = rgb;
                    } else {
                        *palette_slot(&mut this.colors.custom_light, this.color_target) = rgb;
                    }
                    this.colors.preset = Preset::Custom;
                    cx.notify();
                }
            },
        ));

        if let Some(text) = text {
            app.load_text(text, "Standard input".into(), cx);
        } else if let Some(file) = file {
            app.load_file(file, cx);
        }
        app
    }

    fn empty(cx: &mut Context<Self>) -> Self {
        Self {
            doc: None,
            preview_worker: None,
            requested_rows: Vec::new(),
            search_worker: None,
            path_worker: None,
            search_due: None,
            search_highlight: None,
            pending_expression: String::new(),
            row_labels: RowCache::default(),
            pending_export: None,
            export_thread: None,
            export_cancelled: Arc::new(AtomicBool::new(false)),
            large_copy: None,
            selected_path: String::new(),
            source: "No document".into(),
            file: None,
            pending: None,
            waiting_for_old: Arc::new(AtomicBool::new(false)),
            loading_cancelled: Arc::new(AtomicBool::new(false)),
            indexed_progress: Arc::new(LoadProgress::default()),
            indexing: false,
            load_started: None,
            open_duration: None,
            search_duration: None,
            path_duration: None,
            expanded: Vec::new(),
            expanded_resource: None,
            preview_resource: None,
            selected: 0,
            rows: NodeIds::default(),
            matches: SearchResults::default(),
            match_index: None,
            term: String::new(),
            case: false,
            regex: false,
            scope: SearchScope::All,
            query: "$".into(),
            path_results: None,
            history: Vec::new(),
            history_index: 0,
            vim: Vim::default(),
            search_reverse: false,
            search_origin: 0,
            inspector_ratio: 0.5,
            dragging_split: None,
            viewport_rows: 24,
            error: None,
            search_error: None,
            notice: String::new(),
            paste_open: false,
            paste: String::new(),
            paste_recovery: None,
            help: false,
            jsonpath_help: false,
            colors_open: false,
            color_target: 0,
            focus_search: false,
            focus_query: false,
            pending_blur: false,
            sync_query_input: false,
            scroll_to: false,
            detail: String::new(),
            colors: ColorSettings::default(),
            focus_handle: cx.focus_handle(),
            wake: crate::repaint::Context::default(),
            search_input: cx.new(|cx| ui::TextInput::new("/ Search forward...", 16 * 1024, cx)),
            query_input: cx.new(|cx| {
                let mut input = ui::TextInput::new("$.users[*].name or $.users[?(@.active)]", 4096, cx);
                input.set_text("$", cx);
                input
            }),
            hex_input: cx.new(|cx| ui::TextInput::new("#RRGGBB", 7, cx)),
            search_sub: None,
            query_sub: None,
            hex_sub: None,
            tree_scroll: ScrollHandle::new(),
            search_generation: 0,
            export_ticker: false,
        }
    }

    fn palette(&self) -> Palette {
        self.colors.active()
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.preview_worker.is_none() {
            self.preview_worker = Some(PreviewWorker::new(self.wake.clone()));
        }
        self.search_worker.get_or_insert_with(|| SearchWorker::new(self.wake.clone()));
        self.path_worker.get_or_insert_with(|| QueryWorker::new(self.wake.clone()));
        self.poll_previews();
        self.poll();
        self.poll_export();
        self.poll_operations(cx);
        self.refresh_visible();
        if self.pending_export.is_some() {
            self.ensure_export_ticker(cx);
        }
    }

    fn refresh_visible(&mut self) {
        let total = self.rows.len();
        if total == 0 {
            if !self.requested_rows.is_empty() {
                if let Some(worker) = &mut self.preview_worker {
                    worker.cancel_rows();
                }
                self.requested_rows.clear();
            }
            return;
        }
        let first = self.viewport_first().min(total.saturating_sub(1));
        let end = (first + self.viewport_rows.max(1)).min(total);
        if let Some(worker) = &mut self.search_worker {
            worker.visible(&self.rows[first..end]);
        }
        let mut missing = Vec::new();
        for &row in &self.rows[first..end] {
            let id = row as usize;
            if self.row_labels.get(id).is_none() && missing.len() < ROW_BATCH {
                missing.push(id);
            }
        }
        if missing.is_empty() {
            if !self.requested_rows.is_empty() {
                if let Some(worker) = &mut self.preview_worker {
                    worker.cancel_rows();
                }
                self.requested_rows.clear();
            }
            return;
        }
        if missing != self.requested_rows
            && let (Some(doc), Some(worker)) = (&self.doc, &mut self.preview_worker)
        {
            let references = missing.iter().filter_map(|&id| doc.node_ref(id).ok()).collect();
            match worker.rows(Arc::clone(doc), references) {
                Ok(()) => self.requested_rows = missing,
                Err(error) => self.error = Some(error),
            }
        }
    }

    fn viewport_first(&self) -> usize {
        (f32::from(self.tree_scroll.offset().y) / 25.0).max(0.0) as usize
    }

    fn cursor_location(&self) -> Option<(String, String)> {
        self.doc.as_ref()?;
        // Visible IDs stay sorted; avoid scanning the whole document on every frame.
        let row = self.rows.binary_search(&(self.selected as u32)).ok()? + 1;
        let total = self.rows.len();
        let progress = if total <= 1 {
            "All".into()
        } else if row == 1 {
            "Top".into()
        } else if row == total {
            "Bot".into()
        } else {
            format!("{}%", (row as f64 / total as f64 * 100.0).floor() as usize)
        };
        Some((format!("Ln {row}/{total} {progress}"), self.selected_path.clone()))
    }

    fn save_colors(&mut self) {
        match self.colors.save() {
            Ok(()) => self.notice = "Color settings saved".into(),
            Err(error) => self.error = Some(error),
        }
    }

    fn load_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        self.load_started = Some(Instant::now());
        let old = self.prepare_load();
        let waiting = Arc::clone(&self.waiting_for_old);
        let cancelled = Arc::clone(&self.loading_cancelled);
        self.indexing = self.colors.indexed_files;
        self.indexed_progress = Arc::new(LoadProgress::default());
        let progress = Arc::clone(&self.indexed_progress);
        let indexed = self.indexing;
        self.open_duration = None;
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            if !wait_for_old_document(old, &waiting, &cancelled, &wake) {
                return;
            }
            let result = with_io_context(
                Priority::Background,
                Cancellation::Flag(Arc::clone(&cancelled)),
                || {
                    if indexed {
                        Document::open_indexed(
                            &path,
                            || cancelled.load(Ordering::Relaxed),
                            &progress,
                        )
                    } else {
                        std::fs::read_to_string(&path)
                            .map_err(|e| format!("Cannot open {}: {e}", path.display()))
                            .and_then(Document::parse_owned)
                    }
                },
            )
            .map(|doc| (doc, path.display().to_string(), Some(path)));
            let _ = tx.send(result);
            wake.request_repaint();
        });
        cx.notify();
    }

    fn load_text(&mut self, text: String, source: String, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        self.load_started = Some(Instant::now());
        self.indexing = false;
        let old = self.prepare_load();
        let recovery = if source == "Pasted JSON" {
            let (tx, rx) = mpsc::channel();
            self.paste_recovery = Some(rx);
            Some(tx)
        } else {
            None
        };
        let waiting = Arc::clone(&self.waiting_for_old);
        let cancelled = Arc::clone(&self.loading_cancelled);
        self.open_duration = None;
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            if !wait_for_old_document(old, &waiting, &cancelled, &wake) {
                return;
            }
            let result = if let Some(recovery) = recovery {
                Document::parse_owned_recover(text).map_err(|(error, text)| {
                    if let Some(text) = text {
                        let _ = recovery.send(text);
                    }
                    error
                })
            } else {
                Document::parse_owned(text)
            };
            let _ = tx.send(result.map(|doc| (doc, source, None)));
            wake.request_repaint();
        });
        cx.notify();
    }

    fn prepare_load(&mut self) -> Option<Arc<Document>> {
        self.loading_cancelled.store(false, Ordering::Relaxed);
        self.large_copy = None;
        self.paste_recovery = None;
        if !self.colors.low_memory_load {
            return None;
        }
        self.cancel_operations();
        self.export_cancelled.store(true, Ordering::Relaxed);
        let old = self.doc.take();
        self.waiting_for_old.store(old.is_some(), Ordering::Relaxed);
        self.rows = NodeIds::default();
        self.expanded = Vec::new();
        self.expanded_resource = None;
        self.preview_resource = None;
        self.matches = SearchResults::default();
        self.path_results = None;
        self.row_labels.clear();
        self.detail = String::new();
        self.selected_path = String::new();
        self.history = Vec::new();
        self.history_index = 0;
        self.selected = 0;
        self.match_index = None;
        self.source.clear();
        self.file = None;
        self.term.clear();
        self.query = "$".into();
        self.pending_expression.clear();
        self.search_duration = None;
        self.path_duration = None;
        self.search_error = None;
        self.vim.cancel();
        self.notice =
            "Low memory load: previous document closed; waiting for background work".into();
        old
    }

    fn poll(&mut self) {
        let Some(rx) = &self.pending else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                match result {
                    Ok((_doc, _, _))
                        if self.indexing && self.loading_cancelled.load(Ordering::Relaxed) =>
                    {
                        self.notice =
                            "Indexed opening cancelled; reopen the previous file if needed".into();
                    }
                    Ok((doc, source, file)) => {
                        self.paste_recovery = None;
                        self.large_copy = None;
                        let expanded_lease = doc.budget().reserve(
                            Resource::Memory,
                            doc.node_count(),
                            "GUI expansion flags",
                        );
                        let preview_lease = doc.budget().reserve(
                            Resource::Memory,
                            512 * 1024,
                            "GUI preview strings",
                        );
                        let (expanded_lease, preview_lease) = match (expanded_lease, preview_lease)
                        {
                            (Ok(expanded), Ok(preview)) => (expanded, preview),
                            (Err(error), _) | (_, Err(error)) => {
                                self.error = Some(error);
                                self.open_duration =
                                    self.load_started.take().map(|start| start.elapsed());
                                return;
                            }
                        };
                        self.expanded = (0..doc.node_count())
                            .map(|id| doc.node_info(id).depth < 2)
                            .collect();
                        self.expanded_resource = Some(expanded_lease);
                        self.preview_resource = Some(preview_lease);
                        self.rows = NodeIds::new(doc.budget());
                        self.cancel_operations();
                        self.row_labels.clear();
                        self.detail.clear();
                        self.doc = Some(Arc::new(doc));
                        self.source = source;
                        self.file = file;
                        self.selected = 0;
                        self.term.clear();
                        self.matches = SearchResults::default();
                        self.search_error = None;
                        self.search_duration = None;
                        self.path_duration = None;
                        self.path_results = None;
                        self.query = "$".into();
                        self.sync_query_input = true;
                        self.history.clear();
                        self.history_index = 0;
                        self.vim.cancel();
                        self.notice = "Document loaded".into();
                        self.select(0);
                        self.rebuild_rows();
                        self.scroll_to = true;
                    }
                    Err(error) => {
                        if error == "Loading cancelled" {
                            self.error = None;
                            self.notice =
                                "Indexed opening cancelled; reopen the previous file if needed"
                                    .into();
                        } else {
                            if let Some(text) =
                                self.paste_recovery.take().and_then(|rx| rx.try_recv().ok())
                            {
                                self.paste = text;
                                self.paste_open = true;
                            }
                            self.error = Some(error);
                            if self.doc.is_none() && self.colors.low_memory_load {
                                self.notice =
                                    "Loading failed. The previous document must be reopened."
                                        .into();
                            }
                        }
                    }
                }
                self.open_duration = self.load_started.take().map(|start| start.elapsed());
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                if self.loading_cancelled.load(Ordering::Relaxed) {
                    self.error = None;
                    self.notice = "Opening cancelled; reopen the previous file if needed".into();
                } else {
                    self.error = Some("Loading worker stopped".into());
                }
                self.open_duration = self.load_started.take().map(|start| start.elapsed());
            }
            Err(mpsc::TryRecvError::Empty) => (),
        }
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_none()
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            self.load_file(path, cx);
        }
    }

    fn rebuild_rows(&mut self) {
        if let Some(doc) = &self.doc
            && let Err(error) = doc.visible_into(&self.expanded, &mut self.rows)
        {
            self.error = Some(error);
        }
    }

    fn select(&mut self, id: usize) {
        let Some(doc) = &self.doc else {
            return;
        };
        if self.history.get(self.history_index) != Some(&id) {
            if !self.history.is_empty() {
                self.history.truncate(self.history_index + 1);
            }
            self.history.push(id);
            self.history_index = self.history.len() - 1;
        }
        if self.selected != id || self.detail.is_empty() {
            if let Some(worker) = &mut self.preview_worker {
                self.detail = "Loading preview...".into();
                self.selected_path = "Loading path...".into();
                if let Err(error) = worker.select(Arc::clone(doc), doc.node_ref(id).unwrap()) {
                    self.detail = error;
                    self.selected_path = "Path unavailable".into();
                }
            } else {
                // Context-free callers use the same engine synchronously.
                self.detail = doc.preview(id);
                self.selected_path = doc.path(id);
            }
        }
        self.selected = id;
    }

    fn history_move(&mut self, backwards: bool) {
        if self.history.is_empty() {
            return;
        }
        self.history_index = if backwards {
            self.history_index.saturating_sub(1)
        } else {
            (self.history_index + 1).min(self.history.len() - 1)
        };
        // select() recognizes the history cursor, so replay does not add a new entry.
        self.reveal(self.history[self.history_index]);
    }

    fn move_rows(&mut self, delta: i32, count: usize) {
        if self.rows.is_empty() {
            return;
        }
        let position = self
            .rows
            .iter()
            .position(|&id| id as usize == self.selected)
            .unwrap_or(0);
        let next = (position as i64 + delta as i64 * count as i64)
            .clamp(0, self.rows.len() as i64 - 1) as usize;
        self.reveal(self.rows[next] as usize);
    }

    fn start_search(&mut self, reverse: bool) {
        self.search_reverse = reverse;
        self.search_origin = self.selected;
        self.focus_search = true;
    }

    fn execute_vim(&mut self, command: Command, cx: &mut Context<Self>) {
        let count = command.count;
        match command.action {
            Action::Search(reverse) => self.start_search(reverse),
            Action::Copy(path) => self.copy(path, cx),
            _ if self.doc.is_none() => (),
            Action::Move(delta) => self.move_rows(delta, count),
            Action::Left | Action::Right => {
                for _ in 0..count {
                    let doc = self.doc.as_ref().unwrap();
                    let node = doc.node_info(self.selected);
                    if command.action == Action::Left {
                        if node.container() && self.expanded[self.selected] {
                            self.toggle(self.selected);
                        } else if let Some(parent) = node.parent() {
                            self.reveal(parent);
                        } else {
                            break;
                        }
                    } else if node.container() && !self.expanded[self.selected] {
                        self.toggle(self.selected);
                    } else if node.container() {
                        match doc.children_page(node.reference, 0, 1) {
                            Ok(page) if !page.children.is_empty() => {
                                self.reveal(page.children[0].reference.address as usize)
                            }
                            Ok(_) => break,
                            Err(error) => {
                                self.error = Some(error);
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
            }
            Action::First | Action::Last => {
                let pos = if command.explicit_count {
                    count.saturating_sub(1).min(self.rows.len() - 1)
                } else if command.action == Action::First {
                    0
                } else {
                    self.rows.len() - 1
                };
                self.reveal(self.rows[pos] as usize);
            }
            Action::Match(opposite) => self.match_step(self.search_reverse ^ opposite, count),
            Action::History(backwards) => {
                for _ in 0..count {
                    self.history_move(backwards);
                }
            }
            Action::Toggle | Action::Open | Action::Close => {
                if self
                    .doc
                    .as_ref()
                    .unwrap()
                    .node_info(self.selected)
                    .container()
                {
                    let want_open = command.action == Action::Open;
                    if command.action == Action::Toggle || self.expanded[self.selected] != want_open
                    {
                        self.toggle(self.selected);
                    }
                }
            }
            Action::ExpandAll => {
                self.expanded.fill(true);
                self.rebuild_rows();
            }
            Action::CollapseAll => {
                self.expanded.fill(false);
                self.reveal(0);
            }
            Action::Screen(which) => {
                let offset = match which {
                    'M' => self.viewport_rows / 2,
                    'L' => self.viewport_rows.saturating_sub(1),
                    _ => 0,
                };
                self.reveal(
                    self.rows[(self.viewport_first() + offset).min(self.rows.len() - 1)] as usize,
                );
            }
        }
    }

    fn reveal(&mut self, id: usize) {
        if let Some(doc) = &self.doc {
            let mut parent = doc.node_info(id).parent();
            while let Some(p) = parent {
                self.expanded[p] = true;
                parent = doc.node_info(p).parent();
            }
        }
        self.select(id);
        self.rebuild_rows();
        self.scroll_to = true;
    }

    fn cancel_operations(&mut self) {
        self.search_highlight = None;
        self.search_due = None;
        self.requested_rows.clear();
        if let Some(worker) = &mut self.preview_worker {
            worker.cancel();
        }
        if let Some(worker) = &mut self.search_worker {
            worker.cancel();
        }
        if let Some(worker) = &mut self.path_worker {
            worker.cancel();
        }
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.search_highlight = None;
        if let Some(worker) = &mut self.search_worker {
            worker.cancel();
        }
        self.matches = SearchResults::default();
        self.search_error = None;
        self.search_duration = None;
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        if self.term.is_empty() || !self.colors.live_search {
            self.search_due = None;
        } else {
            self.search_due = Some(Instant::now() + Duration::from_millis(200));
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.search_generation == generation
                        && this.search_due.is_some_and(|due| Instant::now() >= due)
                    {
                        this.search();
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    fn search(&mut self) {
        self.search_highlight =
            ui::SearchHighlight::new(&self.term, self.case, self.regex, self.scope);
        self.search_due = None;
        self.matches = SearchResults::default();
        self.search_error = None;
        self.match_index = None;
        self.search_duration = None;
        if let Some(worker) = &mut self.search_worker {
            worker.cancel();
        }
        let Some(doc) = &self.doc else {
            return;
        };
        if self.term.is_empty() {
            return;
        }
        if let Some(worker) = &mut self.search_worker {
            let result = worker.submit(
                Arc::clone(doc),
                SearchRequest {
                    term: self.term.clone(),
                    case: self.case,
                    regex: self.regex,
                    scope: self.scope,
                },
                self.search_origin,
                self.search_reverse,
            );
            if let Err(error) = result {
                self.search_highlight = None;
                self.search_error = Some(error);
            }
        } else {
            let started = Instant::now();
            let result = crate::legacy_search::run(
                doc.clone(),
                SearchRequest {
                    term: self.term.clone(),
                    case: self.case,
                    regex: self.regex,
                    scope: self.scope,
                },
            );
            self.finish_search(result, started.elapsed());
        }
    }

    fn finish_search(&mut self, result: Result<SearchResults, String>, elapsed: Duration) {
        match result {
            Ok(results) => {
                self.matches = results;
                if let Some(store) = &self.matches.store {
                    match crate::legacy_search::initial(store, self.search_origin, self.search_reverse)
                    {
                        Ok(Some((index, id))) => {
                            self.match_index = Some(index as usize);
                            self.reveal(id);
                        }
                        Ok(None) => {}
                        Err(error) => self.search_error = Some(error),
                    }
                }
            }
            Err(error) => {
                self.search_highlight = None;
                self.search_error = Some(error);
            }
        }
        self.search_duration = Some(elapsed);
    }

    fn go_to_path(&mut self) {
        self.path_duration = None;
        if let Some(worker) = &mut self.path_worker {
            worker.cancel();
        }
        if self.query.trim().is_empty() {
            self.path_results = None;
            return;
        }
        let Some(doc) = &self.doc else {
            return;
        };
        self.pending_expression = self.query.clone();
        self.path_results = None;
        if let Some(worker) = &mut self.path_worker {
            if let Err(error) = worker.submit(Arc::clone(doc), QueryRequest::new(self.query.clone()))
            {
                self.finish_path(Err(error), Duration::ZERO);
            }
        } else {
            let started = Instant::now();
            let result = QueryEngine::collect(doc.clone(), &QueryRequest::new(self.query.clone()), &|| false)
                .map(|ids| {
                    let preview = doc.results_preview(&ids);
                    (ids, preview)
                });
            self.finish_path(result, started.elapsed());
        }
    }

    fn finish_path(&mut self, result: Result<(NodeIds, String), String>, elapsed: Duration) {
        let started = Instant::now();
        match result {
            Ok((ids, preview)) => {
                let first = ids.first().copied();
                self.path_results = Some(PathResults {
                    store: None,
                    start: 0,
                    expression: self.pending_expression.clone(),
                    preview,
                    ids: Arc::new(ids),
                    error: None,
                });
                if let Some(id) = first {
                    self.reveal(id as usize);
                }
            }
            Err(error) => {
                self.path_results = Some(PathResults {
                    store: None,
                    start: 0,
                    expression: self.pending_expression.clone(),
                    ids: Arc::new(NodeIds::default()),
                    preview: String::new(),
                    error: Some(error),
                });
            }
        }
        self.path_duration = Some(elapsed + started.elapsed());
    }

    fn poll_operations(&mut self, cx: &mut Context<Self>) {
        if self.search_due.is_some_and(|due| Instant::now() >= due) {
            self.search();
        }
        if let Some(update) = self.search_worker.as_mut().and_then(SearchWorker::poll) {
            self.matches = update.results;
            if let Some((index, id)) = update.selection {
                self.match_index = Some(index as usize);
                self.reveal(id);
            }
            if let Some(elapsed) = update.elapsed {
                self.search_duration = Some(elapsed);
            }
            self.search_error = update.error;
        }
        if let Some(update) = self.path_worker.as_mut().and_then(QueryWorker::poll) {
            self.pending_expression = update.expression.clone();
            let results = self.path_results.get_or_insert_with(|| PathResults {
                expression: update.expression.clone(),
                store: None,
                start: 0,
                ids: Arc::new(NodeIds::default()),
                preview: String::new(),
                error: None,
            });
            results.expression = update.expression;
            results.store = update.store;
            results.error = update.error;
            if results.start != update.start {
                results.ids = Arc::new(NodeIds::default());
                results.preview.clear();
            }
            results.start = update.start;
            if let Some(page) = update.page {
                results.expression = page.expression;
                results.ids = Arc::new(page.ids);
                results.preview = page.preview;
            }
            if let Some(id) = update.first {
                self.reveal(id);
            }
            if let Some(elapsed) = update.elapsed {
                self.path_duration = Some(elapsed);
            }
            if let Some(clipboard) = update.clipboard {
                match clipboard.result {
                    Ok(text) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        self.notice = "Query results copied".into();
                    }
                    Err(CopyError::TooLarge) => {
                        if let Some(doc) = &self.doc {
                            self.large_copy = Some(CopyRequest {
                                doc: doc.clone(),
                                selection: clipboard.selection,
                            });
                        }
                    }
                    Err(CopyError::Serialization(error)) => self.error = Some(error),
                }
            }
        }
    }

    fn poll_previews(&mut self) {
        while let Some(completed) = self.preview_worker.as_mut().and_then(PreviewWorker::poll) {
            if self
                .doc
                .as_ref()
                .is_none_or(|doc| doc.session_id() != completed.task.session)
            {
                continue;
            }
            match completed.result {
                Ok(PreviewOutput::Selection { reference, preview, path }) => {
                    if reference.address as usize == self.selected {
                        self.detail = preview;
                        self.selected_path = path;
                    }
                }
                Ok(PreviewOutput::Rows(rows)) => {
                    self.requested_rows.clear();
                    for row in rows {
                        self.row_labels.insert(row.id, (row.key, row.preview));
                    }
                }
                Err(error) => {
                    if completed.selection {
                        self.detail = error;
                        self.selected_path = "Path unavailable".into();
                    } else {
                        self.requested_rows.clear();
                        self.error = Some(error);
                    }
                }
            }
        }
    }

    fn next_query_match(&mut self, backwards: bool) {
        if let Some(worker) = &mut self.path_worker {
            worker.next(backwards);
        }
    }

    fn next_match(&mut self, backwards: bool) {
        self.match_step(backwards, 1);
    }

    fn match_step(&mut self, backwards: bool, count: usize) {
        if let Some(worker) = &mut self.search_worker {
            worker.next(backwards, count);
            return;
        }
        if self.matches.is_empty() {
            return;
        }
        let count = count % self.matches.len();
        let current = self.match_index.unwrap_or(0);
        let index = if backwards {
            (current + self.matches.len() - count) % self.matches.len()
        } else {
            (current + count) % self.matches.len()
        };
        self.match_index = Some(index);
        if let Some(store) = &self.matches.store {
            match store.node(index as u64, &|| false) {
                Ok(node) => self.reveal(node.reference.address as usize),
                Err(error) => self.search_error = Some(error),
            }
        }
    }

    fn copy(&mut self, path: bool, cx: &mut Context<Self>) {
        if path {
            if let Some(doc) = &self.doc {
                cx.write_to_clipboard(ClipboardItem::new_string(doc.path(self.selected)));
                self.notice = "Path copied".into();
                cx.notify();
            }
        } else if let Some(doc) = &self.doc {
            self.copy_selection(Arc::clone(doc), Selection::Node(self.selected), cx);
        }
    }

    fn copy_selection(&mut self, doc: Arc<Document>, selection: Selection, cx: &mut Context<Self>) {
        match selection.json(&doc, Some(COPY_LIMIT)) {
            Ok(json) => {
                cx.write_to_clipboard(ClipboardItem::new_string(json));
                self.notice = "JSON copied".into();
            }
            Err(CopyError::TooLarge) => {
                self.large_copy = Some(CopyRequest { doc, selection });
            }
            Err(CopyError::Serialization(error)) => self.error = Some(error),
        }
        cx.notify();
    }

    fn export(&mut self) {
        if let Some(doc) = &self.doc {
            self.export_selection(Arc::clone(doc), Selection::Node(self.selected));
        }
    }

    fn export_matches(&mut self, paths: bool, partial: bool) {
        if self.pending_export.is_some() {
            self.notice = "An export is already in progress".into();
            return;
        }
        let (Some(doc), Some(store)) = (&self.doc, &self.matches.store) else {
            return;
        };
        let doc = doc.clone();
        let store = store.clone();
        let prefix = partial.then(|| store.state().count);
        if !partial && !self.matches.completed() {
            self.search_error = Some("Complete export requires completed search results".into());
            return;
        }
        let name = match (partial, paths) {
            (false, false) => "search-values.json",
            (false, true) => "search-paths.json",
            (true, false) => "search-partial-values.json",
            (true, true) => "search-partial-paths.json",
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name(name)
            .save_file()
        else {
            return;
        };
        self.export_cancelled = Arc::new(AtomicBool::new(false));
        let cancelled = self.export_cancelled.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_export = Some(rx);
        self.notice = if partial {
            "Exporting partial search results..."
        } else {
            "Exporting complete search results..."
        }
        .into();
        self.export_thread = Some(std::thread::spawn(move || {
            let result = with_io_context(Priority::User, Cancellation::Flag(cancelled.clone()), || {
                crate::engine::search::output::export(
                    crate::engine::search::Input::Legacy(doc),
                    &store,
                    &path,
                    paths,
                    prefix,
                    &cancelled,
                )
            })
            .map(|()| path);
            let _ = tx.send(result);
        }));
    }

    fn export_selection(&mut self, doc: Arc<Document>, selection: Selection) {
        if self.pending_export.is_some() {
            self.notice = "An export is already in progress".into();
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name("selection.json")
            .save_file()
        {
            self.start_export(path, doc, selection);
        }
    }

    fn start_export(&mut self, path: PathBuf, doc: Arc<Document>, selection: Selection) {
        self.error = None;
        self.export_cancelled = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&self.export_cancelled);
        let (tx, rx) = mpsc::channel();
        self.pending_export = Some(rx);
        self.notice = "Exporting JSON...".into();
        self.export_thread = Some(std::thread::spawn(move || {
            let result = with_io_context(
                Priority::User,
                Cancellation::Flag(Arc::clone(&cancelled)),
                || {
                    export::atomic_write_with_budget(&path, &cancelled, doc.budget(), |writer| {
                        selection.write_cancellable(&doc, writer, &|| {
                            cancelled.load(Ordering::Relaxed)
                        })
                    })
                },
            )
            .map_err(|error| {
                if cancelled.load(Ordering::Relaxed) {
                    "Export cancelled".into()
                } else {
                    error
                }
            })
            .map(|()| path);
            drop(selection);
            drop(doc);
            let _ = tx.send(result);
        }));
    }

    fn poll_export(&mut self) {
        if let Some(rx) = &self.pending_export {
            match rx.try_recv() {
                Ok(result) => {
                    self.pending_export = None;
                    if let Some(thread) = self.export_thread.take() {
                        let _ = thread.join();
                    }
                    match result {
                        Ok(path) => self.notice = format!("Exported {}", path.display()),
                        Err(error) if error.contains("Export cancelled") => {
                            self.notice = "Export cancelled".into()
                        }
                        Err(error) => self.error = Some(format!("Cannot export: {error}")),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending_export = None;
                    if let Some(thread) = self.export_thread.take() {
                        let _ = thread.join();
                    }
                    self.error = Some("Export worker stopped".into());
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
    }

    fn ensure_export_ticker(&mut self, cx: &mut Context<Self>) {
        if self.export_ticker {
            return;
        }
        self.export_ticker = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let more = this
                    .update(cx, |this, cx| {
                        this.tick(cx);
                        this.pending_export.is_some()
                    })
                    .unwrap_or(false);
                if !more {
                    let _ = this.update(cx, |this, _cx| this.export_ticker = false);
                    break;
                }
            }
        })
        .detach();
    }

    fn toggle(&mut self, id: usize) {
        self.expanded[id] = !self.expanded[id];
        if let Some(doc) = &self.doc
            && !self.expanded[id]
            && self.selected > id
            && self.selected < doc.node_info(id).end()
        {
            self.select(id);
        }
        self.rebuild_rows();
    }

    fn load_paste(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_none() {
            let text = std::mem::take(&mut self.paste);
            self.load_text(text, "Pasted JSON".into(), cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        let key = keystroke.key.as_str();
        let secondary = m.secondary();
        if self.jsonpath_help {
            if key == "escape" {
                self.jsonpath_help = false;
                cx.notify();
            }
            self.vim.cancel();
            return;
        }
        if key == "escape" {
            self.large_copy = None;
            self.help = false;
            self.paste_open = false;
            self.colors_open = false;
            self.vim.record("Esc");
            window.blur();
            cx.notify();
            return;
        }
        if key == "f1" {
            self.help = !self.help;
            self.vim.cancel();
            cx.notify();
            return;
        }
        if self.help || self.paste_open || self.colors_open || self.large_copy.is_some() {
            return;
        }
        if secondary && key == "o" {
            self.vim.cancel();
            self.open(cx);
            return;
        }
        if m.control && key == "f" {
            self.start_search(false);
            cx.notify();
        }
        if key == "f3" {
            self.next_match(m.shift);
        }
        if key == "f4" {
            self.next_query_match(m.shift);
        }
        if key == "f5"
            && let Some(file) = self.file.clone()
        {
            self.load_file(file, cx);
        }
        if self.input_focused(window, cx) {
            self.vim.cancel();
            return;
        }
        if !m.control && !m.alt && !m.platform && key == "." {
            self.vim.record(".");
            self.focus_query = true;
            cx.notify();
            return;
        }
        if secondary && key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.load_text(text, "Clipboard".into(), cx);
            }
            return;
        }
        if secondary && key == "c" {
            self.copy(false, cx);
            return;
        }
        if secondary && key == "p" {
            self.copy(true, cx);
            return;
        }
        for (shortcut, direction, half) in [
            ("d", 1, true),
            ("u", -1, true),
            ("b", -1, false),
            ("e", 1, false),
        ] {
            if m.control && key == shortcut {
                let label = format!("Ctrl+{shortcut}");
                let count = self.vim.take_count(&label);
                self.move_rows(
                    direction,
                    count * if half { (self.viewport_rows / 2).max(1) } else { self.viewport_rows },
                );
                cx.notify();
                return;
            }
        }
        if !m.control && !m.alt && !m.platform
            && let Some(ch) = keystroke.key_char.as_deref().and_then(|s| s.chars().next())
            && ch != ' '
            && let Some(command) = self.vim.feed(ch)
        {
            self.execute_vim(command, cx);
            cx.notify();
            return;
        }
        if self.doc.is_none() {
            return;
        }
        for (name, action, label) in [
            ("down", Action::Move(1), "↓"),
            ("up", Action::Move(-1), "↑"),
            ("left", Action::Left, "←"),
            ("right", Action::Right, "→"),
            ("home", Action::First, "Home"),
            ("end", Action::Last, "End"),
            ("space", Action::Toggle, "Space"),
        ] {
            if key == name {
                let count = self.vim.take_count(label);
                self.execute_vim(
                    Command { action, count, explicit_count: false },
                    cx,
                );
                cx.notify();
            }
        }
        if key == "pagedown" {
            self.vim.record("PageDown");
            self.move_rows(1, self.viewport_rows);
            cx.notify();
        }
        if key == "pageup" {
            self.vim.record("PageUp");
            self.move_rows(-1, self.viewport_rows);
            cx.notify();
        }
    }

    fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.search_input.read(cx).focus_handle(cx).is_focused(window)
            || self.query_input.read(cx).focus_handle(cx).is_focused(window)
            || self.hex_input.read(cx).focus_handle(cx).is_focused(window)
    }

    fn toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mut left = div().flex().items_center().flex_wrap().gap_2();
        left = left
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(0x60b4fa))
                    .child("FerrisJson"),
            )
            .child(div().w(px(1.)).h(px(18.)).bg(color(palette.selection)));
        if self.pending.is_none() {
            left = left
                .child(ui::button("open-file", "Open file", palette).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.open(cx);
                    },
                )))
                .child(ui::button("paste-json", "Paste JSON", palette).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.vim.cancel();
                        this.paste_open = true;
                        cx.notify();
                    },
                )))
                .child(
                    ui::toggle(
                        "low-memory",
                        "Low memory load",
                        self.colors.low_memory_load,
                        palette,
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.colors.low_memory_load = !this.colors.low_memory_load;
                        this.save_colors();
                        cx.notify();
                    })),
                )
                .child(
                    ui::toggle(
                        "indexed-files",
                        "Indexed files",
                        self.colors.indexed_files,
                        palette,
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.colors.indexed_files = !this.colors.indexed_files;
                        this.save_colors();
                        cx.notify();
                    })),
                );
        } else {
            left = left.child(div().text_xs().text_color(rgb(0x64b4eb)).child("Opening..."));
            if self.indexing {
                left = left.child(
                    ui::button("cancel-open", "Cancel open", palette).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            this.loading_cancelled.store(true, Ordering::Relaxed);
                            this.notice = "Cancelling indexed opening...".into();
                            cx.notify();
                        },
                    )),
                );
            }
        }
        if self.pending_export.is_some() {
            left = left
                .child(div().text_xs().text_color(rgb(0x64b4eb)).child("Exporting..."))
                .child(
                    ui::button("cancel-export", "Cancel export", palette).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            this.export_cancelled.store(true, Ordering::Relaxed);
                            this.notice = "Cancelling export...".into();
                            cx.notify();
                        },
                    )),
                );
        }
        left = left.child(ui::button("help", "Help", palette).on_click(cx.listener(
            |this, _: &ClickEvent, _, cx| {
                this.help = true;
                cx.notify();
            },
        )));

        let preset = self.colors.preset.next();
        let right = div()
            .flex()
            .items_center().gap_2()
            .child(
                ui::button("preset", self.colors.preset.name(), palette).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        this.colors.preset = preset;
                        this.save_colors();
                        cx.notify();
                    },
                )),
            )
            .child(ui::button("colors", "Colors", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.vim.cancel();
                    this.colors_open = true;
                    let hex = format!("#{:02X}{:02X}{:02X}",
                        palette_get(&this.colors.active(), this.color_target)[0],
                        palette_get(&this.colors.active(), this.color_target)[1],
                        palette_get(&this.colors.active(), this.color_target)[2]);
                    this.hex_input.update(cx, |input, cx| input.set_text(hex, cx));
                    cx.notify();
                },
            )))
            .child(
                ui::button("dark", if self.colors.dark { "Light" } else { "Dark" }, palette)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.colors.dark = !this.colors.dark;
                        this.save_colors();
                        cx.notify();
                    })),
            );
        div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .pt_2()
            .child(left)
            .child(div().flex_1())
            .child(right)
            .into_any_element()
    }

    fn search_running(&self) -> bool {
        self.search_due.is_some()
            || self
                .matches
                .store
                .as_ref()
                .is_some_and(|store| {
                    store.state().status == crate::engine::results::Status::Scanning
                })
            || (self.matches.store.is_none()
                && self.search_worker.as_ref().is_some_and(|worker| worker.busy))
    }

    fn search_counter(&self) -> String {
        let total = self.matches.len();
        let position = self.match_index.map_or_else(
            || if total == 0 { "0".into() } else { "—".into() },
            |index| (index + 1).to_string(),
        );
        if self.matches.completed() {
            format!("{position} / {total}")
        } else if self.matches.store.is_some() {
            format!("{position} / at least {total}")
        } else {
            "0 / 0".into()
        }
    }

    fn scope_name(&self) -> &'static str {
        match self.scope {
            SearchScope::All => "All",
            SearchScope::Keys => "Keys",
            SearchScope::Values => "Values",
        }
    }

    fn search_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let running = self.search_running();
        let mut row = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .py_1()
            .child(
                ui::toggle("live-search", "Live search", self.colors.live_search, palette)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.colors.live_search = !this.colors.live_search;
                        this.save_colors();
                        this.schedule_search(cx);
                        cx.notify();
                    })),
            )
            .child(ui::toggle("case", "Aa", self.case, palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.case = !this.case;
                    this.schedule_search(cx);
                    cx.notify();
                },
            )))
            .child(ui::toggle("regex", ".*", self.regex, palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.regex = !this.regex;
                    this.schedule_search(cx);
                    cx.notify();
                },
            )))
            .child(ui::button("scope", self.scope_name(), palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.scope = match this.scope {
                        SearchScope::All => SearchScope::Keys,
                        SearchScope::Keys => SearchScope::Values,
                        SearchScope::Values => SearchScope::All,
                    };
                    this.schedule_search(cx);
                    cx.notify();
                },
            )))
            .child(ui::field(&self.search_input, 280., palette))
            .child(div().text_xs().child(self.search_counter()));
        if running {
            row = row.child(div().text_xs().text_color(rgb(0x64b4eb)).child("…"));
        }
        row = row
            .child(ui::button("search-prev", "<", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.next_match(true);
                    cx.notify();
                },
            )))
            .child(ui::button("search-next", ">", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.next_match(false);
                    cx.notify();
                },
            )))
            .child(ui::button("search-clear", "Clear", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.term.clear();
                    this.search_input.update(cx, |input, cx| input.set_text("", cx));
                    this.search();
                    cx.notify();
                },
            )));
        let mut column = div().flex().flex_col().px_2().child(row);
        if let Some(error) = self.search_error.clone() {
            column = column.child(div().text_xs().text_color(rgb(0xff8080)).child(error));
        }
        column.into_any_element()
    }

    fn jsonpath_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let busy = self.path_worker.as_ref().is_some_and(|worker| worker.busy);
        let position = self.path_worker.as_ref().and_then(QueryWorker::position);
        let counter = position.map_or_else(|| "0/0".into(), |(index, count)| format!("{}/{count}", index + 1));
        let mut row = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .pb_1()
            .child(div().text_xs().child("JSONPath"))
            .child(ui::button("jsonpath-cheatsheet", "Cheatsheet", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.vim.cancel();
                    this.jsonpath_help = true;
                    cx.notify();
                }),
            ));
        if busy {
            row = row.child(div().text_xs().text_color(rgb(0x64b4eb)).child("…"));
        }
        row = row
            .child(ui::field(&self.query_input, 320., palette))
            .child(div().text_xs().child(counter))
            .child(ui::button("query-prev", "<", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.next_query_match(true);
                    cx.notify();
                },
            )))
            .child(ui::button("query-next", ">", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.next_query_match(false);
                    cx.notify();
                },
            )))
            .child(ui::button("expand-all", "Expand all", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.expanded.fill(true);
                    this.rebuild_rows();
                    cx.notify();
                },
            )))
            .child(ui::button("collapse-all", "Collapse all", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.expanded.fill(false);
                    this.reveal(0);
                    this.scroll_to = true;
                    cx.notify();
                },
            )));
        row.into_any_element()
    }

    fn tree_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mut panel = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(240.))
            .h_full()
            .overflow_hidden();
        if let Some(error) = self.error.clone() {
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(div().text_xs().text_color(rgb(0xff8080)).child(error))
                    .child(ui::button("dismiss-error", "Dismiss", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.error = None;
                            cx.notify();
                        }),
                    )),
            );
        }
        if self.doc.is_none() {
            return panel
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(div().text_2xl().text_color(rgb(0x60b4fa)).child("{ }"))
                        .child(div().text_xl().font_weight(FontWeight::BOLD).child("Explore your JSON"))
                        .child(div().child("Open, paste or drag a JSON file here."))
                        .child(ui::button("open-empty", "Open JSON file", palette).on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.open(cx);
                            }),
                        )),
                )
                .into_any_element();
        }
        let source = self.source.clone();
        panel = panel
            .child(div().px_2().py_1().text_sm().font_weight(FontWeight::BOLD).child(source))
            .child(div().h(px(1.)).bg(color(palette.selection)));
        let total = self.rows.len();
        let first = self.viewport_first().min(total.saturating_sub(1));
        let end = (first + self.viewport_rows.max(1) + 2).min(total);
        let mut list = div()
            .id("tree")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.tree_scroll);
        if first > 0 {
            list = list.child(div().h(px(first as f32 * 25.0)));
        }
        for row in first..end {
            list = list.child(self.tree_row(row, cx));
        }
        if end < total {
            list = list.child(div().h(px((total - end) as f32 * 25.0)));
        }
        panel.child(list).into_any_element()
    }

    fn tree_row(&mut self, row: usize, cx: &mut Context<Self>) -> AnyElement {
        let id = self.rows[row] as usize;
        // Snapshot the document data first: the label cache is an LRU, so reading
        // it needs `&mut self` and cannot overlap the document borrow.
        let Some((node_kind, depth, container)) = self.doc.as_ref().map(|doc| {
            let node = doc.node_info(id);
            (node.kind(), node.depth, node.container())
        }) else {
            return div().into_any_element();
        };
        let palette = self.palette();
        let selected_row = self.rows.binary_search(&(self.selected as u32)).unwrap_or(0);
        let digits = self.rows.len().max(1).to_string().len();
        let number = if row == selected_row { row + 1 } else { row.abs_diff(selected_row) };
        let (key, preview) = match self.row_labels.get(id) {
            Some(labels) => labels.clone(),
            None => {
                let key = self
                    .doc
                    .as_ref()
                    .map(|doc| doc.key(id).chars().take(160).collect::<String>())
                    .unwrap_or_default();
                (key, "Loading...".into())
            }
        };
        let is_match = self.matches.contains(id as u32);
        let highlight = self.search_highlight.as_ref().filter(|_| is_match);
        let key_text = if let Some(highlight) = highlight {
            StyledText::new(key.clone()).with_runs(highlight.runs(&key, palette.kind(node_kind), palette, true))
        } else {
            ui::plain_text(&key, palette.kind(node_kind))
        };
        let preview_text = ui::json_text(&preview, palette, highlight);
        let fill = if id == self.selected {
            color(palette.selection)
        } else if row % 2 == 0 {
            color(palette.stripe)
        } else {
            rgba(0x00000000)
        };
        let expanded = self.expanded.get(id).copied().unwrap_or(false);
        let mut number_color: Hsla = color(palette.punctuation).into();
        if row != selected_row {
            number_color = number_color.opacity(0.6);
        }
        div()
            .id(("row", row))
            .flex()
            .items_center()
            .h(px(25.0))
            .w_full()
            .whitespace_nowrap()
            .overflow_hidden()
            .bg(fill)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select(id);
                window.blur();
                cx.notify();
            }))
            .child(
                div()
                    .w(px(digits as f32 * 8.0 + 12.0))
                    .text_xs()
                    .text_color(number_color)
                    .child(format!("{number:>width$}", width = digits)),
            )
            .child(div().w(px(depth as f32 * 18.0)).flex_none())
            .child(if container {
                ui::button(("expander", row), if expanded { "▾" } else { "▸" }, palette)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.toggle(id);
                        window.blur();
                        cx.notify();
                    }))
                    .into_any_element()
            } else {
                div().w(px(22.0)).into_any_element()
            })
            .child(key_text)
            .child(div().text_color(color(palette.punctuation)).child(":"))
            .child(div().flex_1().min_w(px(0.)).overflow_hidden().child(preview_text))
            .into_any_element()
    }

    fn split_height(&self, window: &Window) -> f32 {
        (f32::from(window.viewport_size().height) - 190.0).max(180.0)
    }

    fn update_scroll(&mut self, window: &Window) {
        if self.scroll_to {
            if let Some(position) = self.rows.iter().position(|&id| id as usize == self.selected) {
                let viewport = f32::from(self.tree_scroll.bounds().size.height)
                    .max(f32::from(window.viewport_size().height) - 170.0);
                let target = (position as f32 * 25.0 - viewport / 2.0).max(0.0);
                self.tree_scroll.set_offset(point(px(0.), px(target)));
            }
            self.scroll_to = false;
        }
        let rows = (f32::from(self.tree_scroll.bounds().size.height) / 25.0).floor() as usize;
        self.viewport_rows = rows.max(8);
    }

    fn content(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .min_h(px(0.))
            .child(self.tree_panel(cx))
            .child(self.inspector(window, cx))
            .into_any_element()
    }

    fn inspector(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let height = self.split_height(window);
        let ratio = self.inspector_ratio.clamp(0.15, 0.85);
        div()
            .w(px(360.0))
            .min_w(px(240.0))
            .max_w(px(700.0))
            .flex()
            .flex_col()
            .h_full()
            .border_l_1()
            .border_color(color(palette.selection))
            .child(
                div()
                    .h(px(height * ratio))
                    .min_h(px(60.0))
                    .flex()
                    .flex_col()
                    .child(self.selection_panel(cx)),
            )
            .child(
                div()
                    .id("inspector-divider")
                    .h(px(10.0))
                    .w_full()
                    .bg(color(palette.stripe))
                    .cursor(CursorStyle::ResizeUpDown)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.dragging_split =
                                Some((f32::from(event.position.y), this.inspector_ratio));
                            let _ = window;
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if let Some((start_y, start_ratio)) = this.dragging_split {
                            let height = this.split_height(window).max(1.0);
                            this.inspector_ratio = (start_ratio
                                + (f32::from(event.position.y) - start_y) / height)
                                .clamp(0.15, 0.85);
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.dragging_split = None;
                            cx.notify();
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.dragging_split = None;
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(self.results_panel(cx)),
            )
            .into_any_element()
    }

    fn selection_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let Some(doc) = &self.doc else {
            return div().into_any_element();
        };
        let kind = doc.node_info(self.selected).kind().to_string();
        let path = self.selected_path.clone();
        let detail = self.detail.clone();
        let highlight = self.search_highlight.clone();
        let wrap = self.colors.wrap_values;
        let completed = self.matches.completed();
        let has_matches = !self.matches.is_empty();

        let header = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .child(div().font_weight(FontWeight::BOLD).child("Value preview"))
            .child(div().text_color(palette.kind(&kind)).child(kind))
            .child(ui::toggle("wrap-values", "Wrap", wrap, palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.colors.wrap_values = !this.colors.wrap_values;
                    this.save_colors();
                    cx.notify();
                },
            )));
        let buttons = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .child(ui::button("copy-json", "Copy JSON", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| this.copy(false, cx),
            )))
            .child(ui::button("copy-path", "Copy path", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| this.copy(true, cx),
            )))
            .child(ui::button("export", "Export", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, _| this.export(),
            )))
            .child(
                ui::button("export-match-values", "Match values", palette)
                    .opacity(if completed { 1.0 } else { 0.4 })
                    .when(completed, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                            this.export_matches(false, false)
                        }))
                    }),
            )
            .child(
                ui::button("export-match-paths", "Match paths", palette)
                    .opacity(if completed { 1.0 } else { 0.4 })
                    .when(completed, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                            this.export_matches(true, false)
                        }))
                    }),
            )
            .child(
                ui::button("export-partial-values", "Partial values", palette)
                    .opacity(if has_matches { 1.0 } else { 0.4 })
                    .when(has_matches, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                            this.export_matches(false, true)
                        }))
                    }),
            )
            .child(
                ui::button("export-partial-paths", "Partial paths", palette)
                    .opacity(if has_matches { 1.0 } else { 0.4 })
                    .when(has_matches, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                            this.export_matches(true, true)
                        }))
                    }),
            );

        let mut preview = div()
            .id("value-preview")
            .flex_1()
            .min_h(px(0.0))
            .p_1()
            .text_size(px(13.0));
        preview = if wrap {
            preview.overflow_y_scroll()
        } else {
            preview.overflow_scroll().whitespace_nowrap()
        };
        div()
            .flex()
            .flex_col()
            .h_full()
            .gap_1()
            .p_1()
            .child(header)
            .child(div().text_xs().whitespace_nowrap().overflow_hidden().child(path))
            .child(buttons)
            .child(div().h(px(1.0)).bg(color(palette.selection)))
            .child(preview.child(ui::json_text(&detail, palette, highlight.as_ref())))
            .into_any_element()
    }

    fn results_selection(&self, partial: bool) -> Option<Selection> {
        let results = self.path_results.as_ref()?;
        let count = results
            .store
            .as_ref()
            .map_or(results.ids.len() as u64, |store| store.state().count);
        Some(match &results.store {
            Some(store) => Selection::Stored {
                store: store.clone(),
                prefix: partial.then_some(count),
            },
            None => Selection::Results(results.ids.clone()),
        })
    }

    fn copy_results(&mut self, partial: bool, cx: &mut Context<Self>) {
        let Some(selection) = self.results_selection(partial) else {
            return;
        };
        if let Some(worker) = &mut self.path_worker {
            if let Err(error) = worker.copy(selection, Some(COPY_LIMIT)) {
                self.error = Some(error);
            }
        } else if let Some(doc) = &self.doc {
            let doc = doc.clone();
            self.copy_selection(doc, selection, cx);
        }
        cx.notify();
    }

    fn export_results(&mut self, partial: bool) {
        if self.pending_export.is_some() {
            self.notice = "An export is already in progress".into();
            return;
        }
        let Some(selection) = self.results_selection(partial) else {
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name(if partial { "query-partial.json" } else { "query-results.json" })
            .save_file()
        else {
            return;
        };
        if let Some(doc) = &self.doc {
            let doc = doc.clone();
            self.start_export(path, doc, selection);
        }
    }

    fn results_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let wrap = self.colors.wrap_results;
        let header = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .child(div().font_weight(FontWeight::BOLD).child("JSONPath results"))
            .child(ui::toggle("wrap-results", "Wrap", wrap, palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.colors.wrap_results = !this.colors.wrap_results;
                    this.save_colors();
                    cx.notify();
                },
            )));
        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_1()
            .p_1()
            .child(header);
        let Some(results) = &self.path_results else {
            return column
                .child(div().text_xs().child("Run a JSONPath query to explore matching values."))
                .child(div().text_xs().font_family(ui::mono_family()).child("$.users[*].name"))
                .child(div().text_xs().font_family(ui::mono_family()).child("$..email"))
                .into_any_element();
        };
        let state = results.store.as_ref().map(|store| store.state());
        let has_store = results.store.is_some();
        let count = state.as_ref().map_or(results.ids.len() as u64, |state| state.count);
        let completed = state.as_ref().map_or(
            results.error.is_none() && !self.path_worker.as_ref().is_some_and(|worker| worker.busy),
            |state| state.status == crate::engine::results::Status::Completed,
        );
        let scanning = state
            .as_ref()
            .is_some_and(|state| state.status == crate::engine::results::Status::Scanning)
            || (results.store.is_none()
                && self.path_worker.as_ref().is_some_and(|worker| worker.busy));
        let expression = results.expression.clone();
        let preview = results.preview.clone();
        let error = results.error.clone();
        let start = results.start;
        let ids_len = results.ids.len() as u64;
        let partial_copy = !completed && has_store && count > 0;

        column = column
            .child(div().text_xs().whitespace_nowrap().overflow_hidden().child(expression))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(div().text_xs().child(if completed {
                        format!("{count} matches")
                    } else {
                        format!("at least {count} matches")
                    }))
                    .when(scanning, |row| {
                        row.child(div().text_xs().text_color(rgb(0x64b4eb)).child("…"))
                    })
                    .child(
                        ui::button("copy-all", "Copy all", palette)
                            .opacity(if completed { 1.0 } else { 0.4 })
                            .when(completed, |button| {
                                button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.copy_results(false, cx)
                                }))
                            }),
                    )
                    .child(
                        ui::button("export-all", "Export all", palette)
                            .opacity(if completed { 1.0 } else { 0.4 })
                            .when(completed, |button| {
                                button.on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                                    this.export_results(false)
                                }))
                            }),
                    )
                    .child(
                        ui::button("copy-partial", "Copy partial", palette)
                            .opacity(if partial_copy { 1.0 } else { 0.4 })
                            .when(partial_copy, |button| {
                                button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.copy_results(true, cx)
                                }))
                            }),
                    )
                    .child(
                        ui::button("export-partial", "Export partial", palette)
                            .opacity(if partial_copy { 1.0 } else { 0.4 })
                            .when(partial_copy, |button| {
                                button.on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                                    this.export_results(true)
                                }))
                            }),
                    )
                    .child(
                        ui::button("cancel-query", "Cancel query", palette)
                            .opacity(if scanning { 1.0 } else { 0.4 })
                            .when(scanning, |button| {
                                button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    if let Some(worker) = &mut this.path_worker {
                                        worker.cancel_scan();
                                    }
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(ui::button("clear-query", "Clear", palette).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            if let Some(worker) = &mut this.path_worker {
                                worker.cancel();
                            }
                            this.path_results = None;
                            this.query.clear();
                            this.query_input.update(cx, |input, cx| input.set_text("", cx));
                            this.path_duration = None;
                            cx.notify();
                        },
                    ))),
            );
        if has_store {
            let next = start.saturating_add(crate::query_stream_worker::PAGE);
            let can_next = next < count
                || (scanning && ids_len == crate::query_stream_worker::PAGE);
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        ui::button("query-prev-page", "Previous page", palette)
                            .opacity(if start > 0 { 1.0 } else { 0.4 })
                            .when(start > 0, |button| {
                                button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    if let Some(worker) = &mut this.path_worker {
                                        worker.page(
                                            this.path_results
                                                .as_ref()
                                                .map_or(0, |results| results.start)
                                                .saturating_sub(crate::query_stream_worker::PAGE),
                                        );
                                    }
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(
                        div().text_xs().child(if ids_len == 0 {
                            "Loading / empty page".to_string()
                        } else {
                            format!("{}\u{2013}{}", start + 1, start + ids_len)
                        }),
                    )
                    .child(
                        ui::button("query-next-page", "Next page", palette)
                            .opacity(if can_next { 1.0 } else { 0.4 })
                            .when(can_next, |button| {
                                button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    if let Some(worker) = &mut this.path_worker {
                                        worker.page(
                                            this.path_results
                                                .as_ref()
                                                .map_or(0, |results| results.start)
                                                .saturating_add(crate::query_stream_worker::PAGE),
                                        );
                                    }
                                    cx.notify();
                                }))
                            }),
                    ),
            );
        }
        if let Some(error) = error {
            column = column.child(div().text_xs().text_color(rgb(0xff8080)).child(error));
        }
        if !preview.is_empty() {
            let highlight = self.search_highlight.clone();
            let mut area = div()
                .id("path-results")
                .flex_1()
                .min_h(px(0.0))
                .text_size(px(13.0));
            area = if wrap {
                area.overflow_y_scroll()
            } else {
                area.overflow_scroll().whitespace_nowrap()
            };
            column = column.child(area.child(ui::json_text(&preview, palette, highlight.as_ref())));
        }
        column.into_any_element()
    }

    fn status_bar(&mut self, window: &Window, cx: &App) -> AnyElement {
        let palette = self.palette();
        let input = self.input_focused(window, cx);
        let mut left = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .text_xs()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .font_family(ui::mono_family())
                    .child(if input { "INPUT" } else { "NORMAL" }),
            )
            .child(div().font_family(ui::mono_family()).text_color(rgb(0x64b4eb)).child(self.vim.display().to_owned()));
        if let Some(doc) = &self.doc {
            let backend = doc.backend_name().to_string();
            let nodes = doc.node_count();
            let bytes = doc.bytes;
            left = left
                .child(div().child(backend))
                .child(div().child(format!("{nodes} nodes")));
            let _ = bytes;
            if let Some((position, path)) = self.cursor_location() {
                left = left
                    .child(div().font_family(ui::mono_family()).child(position))
                    .child(div().whitespace_nowrap().overflow_hidden().child(path));
            }
        }
        left = left.child(div().whitespace_nowrap().overflow_hidden().child(self.notice.clone()));
        let mut right = div().flex().items_center().gap_2().text_xs();
        if self.pending.is_some() {
            right = right.child(div().child(if self.indexing {
                self.indexed_progress.status()
            } else {
                "Opening...".into()
            }));
        } else if let Some(duration) = self.open_duration {
            right = right.child(div().child(format!("Open: {}", format_duration(duration))));
        }
        if let Some(duration) = self.search_duration {
            right = right.child(div().child(format!("Search: {}", format_duration(duration))));
        }
        if let Some(duration) = self.path_duration {
            right = right.child(div().child(format!("Path: {}", format_duration(duration))));
        }
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_2()
            .py_1()
            .bg(color(palette.stripe))
            .child(left)
            .child(right)
            .into_any_element()
    }

    fn apply_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.sync_query_input) {
            let query = self.query.clone();
            self.query_input.update(cx, |input, cx| input.set_text(query, cx));
        }
        if std::mem::take(&mut self.focus_search) {
            let handle = self.search_input.read(cx).focus_handle(cx);
            window.focus(&handle);
        }
        if std::mem::take(&mut self.focus_query) {
            let handle = self.query_input.read(cx).focus_handle(cx);
            window.focus(&handle);
        }
        if std::mem::take(&mut self.pending_blur) {
            window.blur();
        }
    }

    fn overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut overlays = Vec::new();
        if self.help {
            overlays.push(self.help_modal(cx));
        }
        if self.jsonpath_help {
            overlays.push(self.jsonpath_modal(cx));
        }
        if self.colors_open {
            overlays.push(self.colors_modal(cx));
        }
        if self.paste_open {
            overlays.push(self.paste_modal(cx));
        }
        if self.large_copy.is_some() {
            overlays.push(self.large_copy_modal(cx));
        }
        overlays
    }

    fn help_modal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_xl().font_weight(FontWeight::BOLD).child("Navigate JSON at the speed of thought"))
            .child(div().text_xs().child(
                "Vim keys work in the tree. Current row shows its absolute visible row number; other rows show distance from the cursor. Text fields keep normal typing. Esc closes this page.",
            ));
        for &(title, rows) in help_groups() {
            body = body.child(help_section(title, rows, palette));
        }
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().font_weight(FontWeight::BOLD).child("JSONPath"))
                .child(ui::button("help-jsonpath", "Cheatsheet", palette).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.help = false;
                        this.jsonpath_help = true;
                        cx.notify();
                    },
                ))),
        );
        body = body
            .child(div().text_xs().child(
                "Press . outside text fields to focus JSONPath. The shortcut does not insert a dot. F4 / Shift+F4 jumps to the next / previous result and wraps around.",
            ))
            .child(div().text_xs().font_family(ui::mono_family()).child(
                "$.users[*].name   $..email   $.users[?(@.active == true)]",
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(
                        ["string", "number", "boolean", "null", "object", "array"]
                            .into_iter()
                            .map(|kind| div().text_xs().text_color(palette.kind(kind)).child(kind)),
                    ),
            )
            .child(ui::button("close-help", "Close", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.help = false;
                    cx.notify();
                },
            )));
        ui::modal("help-modal", palette, 760.0, body).into_any_element()
    }

    fn jsonpath_modal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_xl().font_weight(FontWeight::BOLD).child("JSONPath cheatsheet"))
            .child(div().text_xs().child(
                "JSONPath 常用语法。点击 Copy 复制表达式，粘贴到 JSONPath 输入框后执行。Esc 关闭。示例假设根对象包含 users 数组。",
            ));
        let mut index = 0usize;
        for &(title, rows) in jsonpath_groups() {
            body = body.child(div().text_sm().font_weight(FontWeight::BOLD).child(title));
            for (expression, explanation) in rows.iter() {
                let id = ("copy-jsonpath", index);
                index += 1;
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_1()
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(color(palette.selection))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .font_family(ui::mono_family())
                                        .text_xs()
                                        .child(*expression),
                                )
                                .child(ui::button(id, "Copy", palette).on_click(cx.listener(
                                    move |this, _: &ClickEvent, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            (*expression).to_string(),
                                        ));
                                        this.notice = "Copied".into();
                                        cx.notify();
                                    },
                                ))),
                        )
                        .child(div().text_xs().child(*explanation)),
                );
            }
        }
        body = body
            .child(div().text_sm().font_weight(FontWeight::BOLD).child("运算符与使用提示"))
            .child(div().text_xs().font_family(ui::mono_family()).child("==  !=  <  <=  >  >=     &&  ||  !"))
            .child(div().text_xs().child(
                "== 是比较，不能写成 =。字符串 '18' 与数字 18 不同；布尔 true 与字符串 'true' 也不同。JSONPath 区分字段名大小写。",
            ))
            .child(div().text_xs().child(
                "文件后端（Indexed / Sparse）支持上述选择器和标量过滤；过滤中的 @ / $ 操作数只能使用字段、数组下标组成的单节点路径。",
            ))
            .child(ui::button("close-jsonpath", "Close", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.jsonpath_help = false;
                    cx.notify();
                },
            )));
        ui::modal("jsonpath-modal", palette, 700.0, body).into_any_element()
    }

    fn colors_modal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.colors.active();
        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_xl().font_weight(FontWeight::BOLD).child("JSON colors"))
            .child(div().text_xs().child(
                "Built-in palettes include light and dark variants. Edits apply immediately; Save remembers custom colors.",
            ));
        let mut presets = div().flex().items_center().flex_wrap().gap_2();
        for preset in Preset::ALL {
            let selected = self.colors.preset == preset;
            presets = presets.child(
                ui::toggle(("preset", preset as usize), preset.name(), selected, palette).on_click(
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.colors.preset = preset;
                        this.save_colors();
                        cx.notify();
                    }),
                ),
            );
        }
        body = body.child(presets).child(
            div()
                .flex()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(ui::toggle("edit-dark", "Dark mode", self.colors.dark, palette).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.colors.dark = !this.colors.dark;
                        this.save_colors();
                        cx.notify();
                    }),
                ))
                .child(ui::toggle("edit-wrap-values", "Wrap values", self.colors.wrap_values, palette).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.colors.wrap_values = !this.colors.wrap_values;
                        this.save_colors();
                        cx.notify();
                    }),
                ))
                .child(ui::toggle("edit-wrap-results", "Wrap results", self.colors.wrap_results, palette).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.colors.wrap_results = !this.colors.wrap_results;
                        this.save_colors();
                        cx.notify();
                    }),
                )),
        );
        for (index, label) in COLOR_LABELS.iter().enumerate() {
            let rgb = palette_get(&palette, index);
            let selected = self.color_target == index;
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id(("color-swatch", index))
                            .w(px(16.0))
                            .h(px(16.0))
                            .rounded(px(3.0))
                            .bg(color(rgb))
                            .border_1()
                            .border_color(color(palette.punctuation))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.color_target = index;
                                let rgb = palette_get(&this.colors.active(), index);
                                let hex = format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]);
                                this.hex_input.update(cx, |input, cx| input.set_text(hex, cx));
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .w(px(150.0))
                            .text_xs()
                            .when(selected, |label| label.font_weight(FontWeight::BOLD))
                            .child(*label),
                    )
                    .child(div().text_xs().font_family(ui::mono_family()).child(format!(
                        "#{:02X}{:02X}{:02X}",
                        rgb[0], rgb[1], rgb[2]
                    ))),
            );
        }
        body = body
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().child("Hex"))
                    .child(ui::field(&self.hex_input, 100.0, palette)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(ui::button("save-colors", "Save settings", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.save_colors();
                            cx.notify();
                        }),
                    ))
                    .child(ui::button("use-custom", "Use as custom", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.colors.customize();
                            this.save_colors();
                            cx.notify();
                        }),
                    ))
                    .child(ui::button("restore-colors", "Restore Ferris defaults", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            let dark = this.colors.dark;
                            let wrap_values = this.colors.wrap_values;
                            let wrap_results = this.colors.wrap_results;
                            let low_memory_load = this.colors.low_memory_load;
                            let indexed_files = this.colors.indexed_files;
                            let live_search = this.colors.live_search;
                            this.colors = ColorSettings {
                                dark,
                                wrap_values,
                                wrap_results,
                                low_memory_load,
                                indexed_files,
                                live_search,
                                ..Default::default()
                            };
                            this.save_colors();
                            cx.notify();
                        }),
                    ))
                    .child(ui::button("close-colors", "Close", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.colors_open = false;
                            cx.notify();
                        }),
                    )),
            );
        if let Some(error) = self.error.clone() {
            body = body.child(div().text_xs().text_color(rgb(0xff8080)).child(error));
        }
        if let Ok(path) = crate::palette::settings_path() {
            body = body.child(div().text_xs().child(format!("Saved to {}", path.display())));
        }
        ui::modal("colors-modal", palette, 530.0, body).into_any_element()
    }

    fn paste_modal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let text = if self.paste.is_empty() {
            "Paste JSON with Ctrl+V, or use the button below.".to_string()
        } else {
            self.paste.clone()
        };
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_xl().font_weight(FontWeight::BOLD).child("Paste JSON"))
            .child(div().text_xs().child("Paste JSON below, then select Load. Ctrl+V also opens JSON when the tree has focus."))
            .child(
                div()
                    .id("paste-scroll")
                    .max_h(px(320.0))
                    .overflow_scroll()
                    .p_1()
                    .border_1()
                    .border_color(color(palette.selection))
                    .text_size(px(13.0))
                    .whitespace_nowrap()
                    .child(text),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(ui::button("paste-clipboard", "Paste from clipboard", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                                this.paste = text;
                            }
                            cx.notify();
                        }),
                    ))
                    .child(ui::button("load-paste", "Load JSON", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.load_paste(cx);
                            this.paste_open = false;
                            cx.notify();
                        }),
                    ))
                    .child(ui::button("clear-paste", "Clear", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.paste.clear();
                            cx.notify();
                        }),
                    ))
                    .child(ui::button("close-paste", "Cancel", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.paste_open = false;
                            cx.notify();
                        }),
                    )),
            );
        ui::modal("paste-modal", palette, 620.0, body).into_any_element()
    }

    fn large_copy_modal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_xl().font_weight(FontWeight::BOLD).child("Large clipboard copy"))
            .child(div().text_xs().child(
                "Formatted JSON exceeds 8 MiB. Copying it may require substantial memory and pause the interface. Export writes directly to a file.",
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(ui::button("copy-anyway", "Copy anyway", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            let Some(request) = this.large_copy.take() else {
                                return;
                            };
                            if matches!(&request.selection, Selection::Stored { .. }) {
                                if let Some(worker) = &mut this.path_worker
                                    && let Err(error) = worker.copy(request.selection, None)
                                {
                                    this.error = Some(error);
                                }
                            } else {
                                match request.selection.json(&request.doc, None) {
                                    Ok(json) => {
                                        cx.write_to_clipboard(ClipboardItem::new_string(json));
                                        this.notice = "JSON copied".into();
                                    }
                                    Err(error) => {
                                        this.error = Some(format!("Cannot copy: {error:?}"))
                                    }
                                }
                            }
                            cx.notify();
                        }),
                    ))
                    .child(ui::button("export-instead", "Export instead", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            if let Some(request) = this.large_copy.take() {
                                this.export_selection(request.doc, request.selection);
                            }
                            cx.notify();
                        }),
                    ))
                    .child(ui::button("cancel-copy", "Cancel", palette).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.large_copy = None;
                            cx.notify();
                        }),
                    )),
            );
        ui::modal("large-copy-modal", palette, 520.0, body).into_any_element()
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_focus(window, cx);
        self.tick(cx);
        self.apply_focus(window, cx);
        self.update_scroll(window);
        self.refresh_visible();
        let palette = self.palette();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(color(palette.background))
            .text_color(color(palette.punctuation))
            .text_size(px(13.0))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .can_drop(|value, _, _| value.is::<ExternalPaths>())
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if let Some(path) = paths.paths().first() {
                    this.load_file(path.clone(), cx);
                }
            }))
            .child(self.toolbar(cx))
            .child(self.search_bar(cx))
            .child(self.jsonpath_bar(cx))
            .child(self.content(window, cx))
            .child(self.status_bar(window, cx))
            .children(self.overlays(cx))
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.loading_cancelled.store(true, Ordering::Relaxed);
        self.export_cancelled.store(true, Ordering::Relaxed);
        self.cancel_operations();
        // The export owns its temporary-file cleanup. Do not wait on an OS I/O
        // syscall on the UI thread during window close.
        if let Some(thread) = self.export_thread.take() {
            crate::shutdown::defer(thread);
        }
    }
}

fn palette_get(palette: &Palette, index: usize) -> [u8; 3] {
    match index {
        0 => palette.string,
        1 => palette.number,
        2 => palette.boolean,
        3 => palette.null,
        4 => palette.object,
        5 => palette.array,
        6 => palette.punctuation,
        7 => palette.background,
        8 => palette.stripe,
        9 => palette.selection,
        _ => palette.search_match,
    }
}

const COLOR_LABELS: [&str; 11] = [
    "String",
    "Number",
    "Boolean",
    "Null",
    "Object",
    "Array",
    "Text / punctuation",
    "Background",
    "Alternating row",
    "Selected row",
    "Search match",
];

fn help_section(title: &'static str, rows: &'static [(&'static str, &'static str)], palette: Palette) -> Div {
    let mut grid = div().flex().flex_col().gap_1();
    for (key, action) in rows {
        grid = grid.child(
            div()
                .flex()
                .gap_3()
                .child(div().w(px(230.0)).text_xs().font_family(ui::mono_family()).child(*key))
                .child(div().text_xs().child(*action)),
        );
    }
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(color(palette.selection))
        .child(div().text_xs().font_weight(FontWeight::BOLD).text_color(rgb(0x50a0dc)).child(title))
        .child(grid)
}

fn help_groups() -> &'static [(&'static str, &'static [(&'static str, &'static str)])] {
    HELP_GROUPS
}

const HELP_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "NAVIGATION",
        &[
            ("hjkl / arrows", "Parent / down / up / child"),
            ("12j · 3l", "Repeat a movement with a count"),
            ("gg · G · 25G", "First / last / visible row 25"),
            ("H · M · L", "Top / middle / bottom of viewport"),
            ("Ctrl+d / u", "Half page down / up"),
            ("PageDown / Up · Ctrl+e / b", "Full page down / up"),
            ("[ / ]", "Back / forward through visited positions"),
        ],
    ),
    (
        "SEARCH",
        &[
            ("/ · ?", "Search forward / backward"),
            ("n · N · 3n", "Repeat search / reverse / count"),
            ("Enter · Esc", "Confirm search / return to tree"),
            ("F3 / Shift+F3", "Next / previous match"),
            ("Ctrl+F", "Focus search field"),
        ],
    ),
    (
        "TREE & COPY",
        &[
            ("Space · za", "Toggle selected container"),
            ("zo · zc", "Open / close selected container"),
            ("zR · zM", "Expand / collapse whole document"),
            ("yy / Ctrl+C", "Copy full selected JSON"),
            ("yp / Ctrl+P", "Copy selected path"),
        ],
    ),
    (
        "DOCUMENT",
        &[
            ("⌘O (macOS) / Ctrl+O (Windows)", "Open file"),
            ("Ctrl+V", "Load clipboard JSON"),
            ("F5", "Reload current file"),
            ("F1", "Toggle this cheatsheet"),
            ("Esc", "Cancel pending Vim keys / close dialog"),
        ],
    ),
];

fn jsonpath_groups() -> &'static [(&'static str, &'static [(&'static str, &'static str)])] {
    JSONPATH_GROUPS
}

const JSONPATH_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "基础与字段",
        &[
            ("$", "根节点：选择整份 JSON。"),
            ("$.users", "选择根对象的 users 字段；多层字段可写 $.data.users。"),
            ("$['users']", "方括号字段访问，等价于 $.users。"),
            ("$['user.name']", "访问键名本身含点号的字段；空格、中文或特殊字符键也可用引号。"),
            ("$.*", "根对象的所有直接子节点。"),
            ("$..name", "递归查找任意深度的 name 字段。"),
        ],
    ),
    (
        "数组、切片与多选",
        &[
            ("$.users[0]", "第一个用户；下标从 0 开始。"),
            ("$.users[-1]", "最后一个用户；负数从数组末尾计数。"),
            ("$.users[*]", "users 数组中的所有元素。"),
            ("$.users[*].name", "选取每个用户的 name 字段。"),
            ("$.users[0,2]", "按指定顺序选取下标 0、2；重复下标也会保留。"),
            ("$.users[0:10]", "前 10 项；切片格式为 [起点:终点:步长]，不包含终点。"),
            ("$.users[::2]", "从开头每隔一项选一项。"),
            ("$.users[::-1]", "按相反顺序选择数组元素。"),
            ("$.users[*]['name','age']", "选择每个用户的 name 和 age 节点；不会重组成对象或表格。"),
        ],
    ),
    (
        "条件过滤：@ 表示当前候选元素",
        &[
            ("$.users[?(@.age >= 18)]", "筛选 age 至少为 18 的用户。"),
            ("$.users[?(@.name == '张三')]", "字符串相等；字符串必须用单引号或双引号包围。"),
            ("$.users[?(@.active == true)]", "布尔值为真；true、false、null 不加引号。"),
            ("$.users[?(@.email)]", "字段存在即可命中，即使值是 false 或 null。"),
            ("$.users[?(!@.email)]", "字段不存在；! 表示逻辑非。"),
            ("$.users[?(@.email == null)]", "字段值为 null；字段不存在与值为 null 有区别。"),
            ("$.users[?(@.active == true && @.age >= 18)].name", "&& 表示同时满足；过滤后可继续选取 name。"),
            ("$.users[?(@.age < 18 || @.age >= 60)]", "|| 表示任一满足；可加括号组合条件。"),
        ],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formatting_matches_status_bar() {
        assert_eq!(format_duration(Duration::from_micros(1234)), "1.23 ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.50 s");
    }

    #[test]
    fn hex_editing_reaches_every_palette_slot() {
        assert_eq!(parse_hex("#1e7d41"), Some([0x1e, 0x7d, 0x41]));
        assert_eq!(parse_hex("1E7D41"), Some([0x1e, 0x7d, 0x41]));
        assert_eq!(parse_hex("#12345"), None);
        let mut palette = Palette::builtin(Preset::Ferris, true);
        for index in 0..COLOR_LABELS.len() {
            *palette_slot(&mut palette, index) = [index as u8, 2, 3];
            assert_eq!(palette_get(&palette, index), [index as u8, 2, 3]);
        }
    }
}

fn wait_for_old_document(
    old: Option<Arc<Document>>,
    waiting: &AtomicBool,
    cancelled: &AtomicBool,
    wake: &crate::repaint::Context,
) -> bool {
    crate::loading::release_previous(
        old,
        || cancelled.load(Ordering::Relaxed),
        |value| {
            waiting.store(value, Ordering::Relaxed);
            wake.request_repaint();
        },
    )
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() >= 1 {
        format!("{:.2} s", duration.as_secs_f64())
    } else {
        format!("{:.2} ms", duration.as_secs_f64() * 1000.0)
    }
}

fn parse_hex(text: &str) -> Option<[u8; 3]> {
    let digits = text.trim().trim_start_matches('#');
    if digits.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    Some([(value >> 16) as u8, (value >> 8) as u8, value as u8])
}

fn palette_slot(palette: &mut Palette, index: usize) -> &mut [u8; 3] {
    match index {
        0 => &mut palette.string,
        1 => &mut palette.number,
        2 => &mut palette.boolean,
        3 => &mut palette.null,
        4 => &mut palette.object,
        5 => &mut palette.array,
        6 => &mut palette.punctuation,
        7 => &mut palette.background,
        8 => &mut palette.stripe,
        9 => &mut palette.selection,
        _ => &mut palette.search_match,
    }
}
