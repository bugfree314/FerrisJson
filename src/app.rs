use crate::engine::budget::{Reservation, Resource};
use crate::engine::file_source::{Cancellation, Priority, with_io_context};
#[path = "recursive_ui.rs"]
mod recursive_ui;
use crate::preview_worker::{Output as PreviewOutput, PreviewWorker, ROW_BATCH};
use crate::{
    document::{Document, LoadProgress, NodeIds, SearchScope},
    export::{self, COPY_LIMIT, CopyError, Selection},
    palette::{ColorSettings, Preset, color},
    row_cache::RowCache,
    ui::{json_preview, kind_color},
    vim::{Action, Command, Vim},
    worker::{Operation, Output, Worker},
};
use eframe::egui::{self, Color32, Key, RichText};
use std::borrow::Cow;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

#[cfg(test)]
const DEMO: &str = include_str!("../examples/sample.json");
type LoadResult = Result<(Document, String, Option<PathBuf>), String>;
struct PathResults {
    expression: String,
    ids: Arc<NodeIds>,
    preview: String,
    error: Option<String>,
}
struct CopyRequest {
    doc: Arc<Document>,
    selection: Selection,
}

pub struct Viewer {
    progressive: recursive_ui::View,
    discovery: Option<crate::engine::top_level::Discovery>,
    discovery_start: Option<Receiver<crate::engine::top_level::Discovery>>,
    top_page: Option<Arc<crate::engine::top_level::Page>>,
    top_selected: u64,
    top_start: u64,
    top_expanded: bool,
    top_source: String,
    doc: Option<Arc<Document>>,
    preview_worker: Option<PreviewWorker>,
    requested_rows: Vec<usize>,
    search_worker: Option<Worker>,
    path_worker: Option<Worker>,
    search_due: Option<Instant>,
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
    query_duration: Option<(&'static str, Duration)>,
    expanded: Vec<bool>,
    expanded_resource: Option<Reservation>,
    preview_resource: Option<Reservation>,
    selected: usize,
    rows: NodeIds,
    matches: NodeIds,
    match_index: usize,
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
    viewport_first: usize,
    page_rows: usize,
    error: Option<String>,
    search_error: Option<String>,
    notice: String,
    paste_open: bool,
    paste: String,
    paste_recovery: Option<Receiver<String>>,
    help: bool,
    focus_search: bool,
    scroll_to: bool,
    detail: String,
    dark: bool,
    colors: ColorSettings,
    memory_settings: recursive_ui::MemorySettings,
    colors_open: bool,
    logo: Option<egui::TextureHandle>,
}

impl Viewer {
    fn discovering(&self) -> bool {
        self.pending.is_some() && self.discovery.is_some() && self.top_page.is_some()
    }
    fn clear_top(&mut self) {
        self.top_page = None;
        if let Some(discovery) = self.discovery.take() {
            crate::shutdown::defer(discovery.retire());
        }
        if let Some(rx) = self.discovery_start.take()
            && let Ok(discovery) = rx.try_recv()
        {
            crate::shutdown::defer(discovery.retire());
        }
        self.top_selected = 0;
        self.top_start = 0;
        self.top_expanded = true;
    }
    fn poll_top(&mut self) {
        if let Some(rx) = &self.discovery_start {
            match rx.try_recv() {
                Ok(discovery) => {
                    self.discovery = Some(discovery);
                    self.discovery_start = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.discovery_start = None,
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        if let Some(page) = self.discovery.as_ref().and_then(|d| d.poll())
            && page.start == self.top_start
            && page.selected == self.top_selected
        {
            if self.top_page.is_none() {
                self.cancel_operations();
                self.large_copy = None;
            }
            self.top_page = Some(page);
        }
    }
    fn top_tree(&mut self, ui: &mut egui::Ui) {
        let page = self.top_page.as_ref().unwrap().clone();
        let count = if self.top_expanded {
            self.discovery.as_ref().unwrap().statistics().rows
        } else {
            1
        };
        ui.label(RichText::new(&self.top_source).strong());
        ui.separator();
        let mut wanted = self.top_start;
        let mut area = egui::ScrollArea::both()
            .id_salt("tree")
            .auto_shrink([false, false]);
        if self.scroll_to {
            area = area.vertical_scroll_offset(
                (self.top_selected as f32 * (25.0 + ui.spacing().item_spacing.y)
                    - ui.available_height() / 2.0)
                    .max(0.0),
            );
            self.scroll_to = false;
        }
        area.show_rows(ui, 25.0, count as usize, |ui, range| {
            wanted = range.start as u64;
            for index in range {
                let Some(row) = page.rows.iter().find(|row| row.index == index as u64) else {
                    ui.label("Loading...");
                    continue;
                };
                let palette = self.colors.active();
                let fill = if row.index == self.top_selected {
                    color(palette.selection)
                } else if index % 2 == 0 {
                    color(palette.stripe)
                } else {
                    Color32::TRANSPARENT
                };
                egui::Frame::new().fill(fill).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.set_height(25.0);
                        ui.add_space(if row.index == 0 { 0.0 } else { 18.0 });
                        if row.kind.container() {
                            if ui
                                .add_enabled(
                                    row.index == 0,
                                    egui::Button::new(if row.index == 0 && self.top_expanded {
                                        "▾"
                                    } else {
                                        "▸"
                                    })
                                    .frame(false),
                                )
                                .clicked()
                            {
                                self.top_expanded = !self.top_expanded;
                                if !self.top_expanded {
                                    self.top_selected = 0;
                                    wanted = 0;
                                }
                            }
                        } else {
                            ui.add_space(26.0);
                        }
                        let c = kind_color(row.kind.name(), palette);
                        if ui
                            .add(
                                egui::Label::new(RichText::new(&row.label).monospace().color(c))
                                    .sense(egui::Sense::click()),
                            )
                            .clicked()
                        {
                            self.top_selected = row.index;
                        }
                        if ui
                            .add(
                                egui::Label::new(RichText::new(&row.preview).monospace().color(c))
                                    .sense(egui::Sense::click()),
                            )
                            .clicked()
                        {
                            self.top_selected = row.index;
                        }
                    });
                });
            }
        });
        self.top_start = wanted;
        if let Some(discovery) = &self.discovery {
            discovery.request(wanted, self.top_selected);
        }
    }
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        file: Option<PathBuf>,
        text: Option<String>,
        indexed: bool,
    ) -> Self {
        cc.egui_ctx.set_fonts(crate::fonts::definitions());
        let mut app = Self::empty();
        if let Ok(icon) =
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/FerrisJson.png"))
        {
            app.logo = Some(cc.egui_ctx.load_texture(
                "FerrisJson logo",
                egui::ColorImage::from_rgba_unmultiplied(
                    [icon.width as usize, icon.height as usize],
                    &icon.rgba,
                ),
                egui::TextureOptions::LINEAR,
            ));
        }
        match ColorSettings::load() {
            Ok(settings) => {
                app.dark = settings.dark;
                app.colors = settings;
            }
            Err(error) => app.error = Some(error),
        }
        app.memory_settings.percent = app.colors.memory_percent;
        app.colors.active().apply(&cc.egui_ctx, app.dark);
        if indexed {
            app.colors.indexed_files = true;
        }
        if let Some(text) = text {
            app.load_text(text, "Standard input".into(), &cc.egui_ctx);
        } else if let Some(file) = file {
            app.load_file(file, &cc.egui_ctx);
        }
        app
    }
    fn empty() -> Self {
        Self {
            progressive: Default::default(),
            discovery: None,
            discovery_start: None,
            top_page: None,
            top_selected: 0,
            top_start: 0,
            top_expanded: true,
            top_source: String::new(),
            doc: None,
            preview_worker: None,
            requested_rows: Vec::new(),
            search_worker: None,
            path_worker: None,
            search_due: None,
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
            query_duration: None,
            expanded: Vec::new(),
            expanded_resource: None,
            preview_resource: None,
            selected: 0,
            rows: NodeIds::default(),
            matches: NodeIds::default(),
            match_index: 0,
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
            viewport_first: 0,
            page_rows: 20,
            error: None,
            search_error: None,
            notice: String::new(),
            paste_open: false,
            paste: String::new(),
            paste_recovery: None,
            help: false,
            focus_search: false,
            scroll_to: false,
            detail: String::new(),
            dark: true,
            colors: ColorSettings::default(),
            memory_settings: Default::default(),
            colors_open: false,
            logo: None,
        }
    }
    fn cursor_location(&self) -> Option<(String, String)> {
        if let Some(page) = &self.progressive.page {
            return Some((String::new(), page.selection.path.clone()));
        }
        if self.discovering() {
            return self
                .top_page
                .as_ref()
                .map(|p| (String::new(), p.selection.path.clone()));
        }
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
        Some((
            format!("Ln {row}/{total} {progress}"),
            self.selected_path.clone(),
        ))
    }
    fn save_colors(&mut self) {
        match self.colors.save() {
            Ok(()) => self.notice = "Color settings saved".into(),
            Err(error) => self.error = Some(error),
        }
    }
    fn load_file(&mut self, path: PathBuf, ctx: &egui::Context) {
        if cfg!(windows) {
            self.load_progressive(path, ctx);
            return;
        }
        if self.pending.is_some() {
            return;
        }
        self.clear_top();
        self.top_source = path.display().to_string();
        let (top_tx, top_rx) = mpsc::sync_channel(1);
        self.discovery_start = Some(top_rx);
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
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if !wait_for_old_document(old, &waiting, &cancelled, &ctx) {
                return;
            }
            let result = with_io_context(
                Priority::Background,
                Cancellation::Flag(Arc::clone(&cancelled)),
                || {
                    let stable = if cfg!(windows) {
                        Some(crate::engine::top_level::Source::open(&path)?)
                    } else {
                        None
                    };
                    if let Some(source) = &stable
                        && let Ok(discovery) = crate::engine::top_level::Discovery::start(
                            source.clone(),
                            cancelled.clone(),
                            ctx.clone(),
                        )
                    {
                        discovery.wait_first(Duration::from_millis(20));
                        let _ = top_tx.send(discovery);
                        ctx.request_repaint();
                    }
                    let result = if indexed {
                        if let Some(source) = &stable {
                            Document::open_indexed_file(
                                source.duplicate_file()?,
                                || cancelled.load(Ordering::Relaxed),
                                &progress,
                            )
                        } else {
                            Document::open_indexed(
                                &path,
                                || cancelled.load(Ordering::Relaxed),
                                &progress,
                            )
                        }
                    } else {
                        let text = if let Some(source) = &stable {
                            source.read_text(&|| cancelled.load(Ordering::Relaxed))
                        } else {
                            read_file_cancellable(&path, &cancelled)
                        };
                        text.and_then(|text| {
                            if cancelled.load(Ordering::Relaxed) {
                                Err("Loading cancelled".into())
                            } else {
                                Document::parse_owned(text)
                            }
                        })
                    };
                    drop(stable);
                    if cancelled.load(Ordering::Relaxed) {
                        Err("Loading cancelled".into())
                    } else {
                        result
                    }
                },
            )
            .map(|doc| (doc, path.display().to_string(), Some(path)));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn load_text(&mut self, text: String, source: String, ctx: &egui::Context) {
        self.clear_progressive();
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
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if !wait_for_old_document(old, &waiting, &cancelled, &ctx) {
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
            ctx.request_repaint();
        });
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
        self.matches = NodeIds::default();
        self.path_results = None;
        self.row_labels.clear();
        self.detail = String::new();
        self.selected_path = String::new();
        self.history = Vec::new();
        self.history_index = 0;
        self.selected = 0;
        self.match_index = 0;
        self.source.clear();
        self.file = None;
        self.term.clear();
        self.query = "$".into();
        self.pending_expression.clear();
        self.query_duration = None;
        self.search_error = None;
        self.vim.cancel();
        self.notice =
            "Low memory load: previous document closed; waiting for background work".into();
        old
    }
    fn poll(&mut self) {
        self.poll_progressive();
        self.poll_top();
        let Some(rx) = &self.pending else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                let top_pointer = self
                    .top_page
                    .as_ref()
                    .and_then(|p| p.selection.pointer.clone());
                self.pending = None;
                self.clear_top();
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
                        self.matches = NodeIds::default();
                        self.search_error = None;
                        self.query_duration = None;
                        self.path_results = None;
                        self.query = "$".into();
                        self.history.clear();
                        self.history_index = 0;
                        self.vim.cancel();
                        self.notice = "Document loaded".into();
                        let selected = top_pointer
                            .and_then(|p| self.doc.as_ref().unwrap().find_path(&p).ok())
                            .unwrap_or(0);
                        self.select(selected);
                        self.rebuild_rows();
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
                self.clear_top();
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
    fn open(&mut self, ctx: &egui::Context) {
        if self.pending.is_none()
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            self.load_file(path, ctx);
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
    fn execute_vim(&mut self, command: Command, ctx: &egui::Context) {
        let count = command.count;
        match command.action {
            Action::Search(reverse) => self.start_search(reverse),
            Action::Copy(path) => self.copy(ctx, path),
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
                    'M' => self.page_rows / 2,
                    'L' => self.page_rows.saturating_sub(1),
                    _ => 0,
                };
                self.reveal(
                    self.rows[(self.viewport_first + offset).min(self.rows.len() - 1)] as usize,
                );
            }
        }
    }
    fn vim_navigation(&mut self, ctx: &egui::Context) {
        let texts = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| {
                    if let egui::Event::Text(t) = e {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        });
        for text in texts {
            for key in text.chars() {
                if let Some(command) = self.vim.feed(key) {
                    self.execute_vim(command, ctx);
                }
            }
        }
        if self.focus_search {
            // The trigger belongs to Normal mode, never to the search field.
            ctx.input_mut(|i| {
                i.events
                    .retain(|e| !matches!(e, egui::Event::Text(t) if t == "/" || t == "?"))
            });
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
    fn schedule_search(&mut self, ctx: &egui::Context) {
        self.progressive_cancel_operation();
        self.progressive.matches = None;
        if let Some(worker) = &mut self.search_worker {
            worker.cancel();
        }
        self.matches = NodeIds::default();
        self.search_error = None;
        self.query_duration = None;
        if self.term.is_empty() || !self.colors.live_search {
            self.search_due = None;
        } else {
            self.search_due = Some(Instant::now() + Duration::from_millis(200));
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
    fn search(&mut self) {
        if self.progressive.session.is_some() {
            self.progressive_search();
            return;
        }
        if self.discovering() {
            self.search_due = None;
            return;
        }
        self.search_due = None;
        self.matches = NodeIds::default();
        self.search_error = None;
        self.match_index = 0;
        self.query_duration = None;
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
                Operation::Search {
                    term: self.term.clone(),
                    case: self.case,
                    regex: self.regex,
                    scope: self.scope,
                },
            );
            if let Err(error) = result {
                self.search_error = Some(error);
            }
        } else {
            // Headless callers can use the same completion path without an egui context.
            let started = Instant::now();
            let result = doc.search(&self.term, self.case, self.regex, self.scope);
            self.finish_search(result, started.elapsed());
        }
    }
    fn finish_search(&mut self, result: Result<NodeIds, String>, elapsed: Duration) {
        let started = Instant::now();
        match result {
            Ok(ids) => self.matches = ids,
            Err(error) => self.search_error = Some(error),
        }
        if !self.matches.is_empty() {
            self.match_index = if self.search_reverse {
                self.matches
                    .iter()
                    .rposition(|&id| (id as usize) < self.search_origin)
                    .unwrap_or(self.matches.len() - 1)
            } else {
                self.matches
                    .iter()
                    .position(|&id| id as usize > self.search_origin)
                    .unwrap_or(0)
            };
            self.reveal(self.matches[self.match_index] as usize);
        }
        self.query_duration = Some(("Search", elapsed + started.elapsed()));
    }
    fn go_to_path(&mut self) {
        if self.progressive.session.is_some() {
            self.progressive_query();
            return;
        }
        if self.discovering() {
            return;
        }
        if let Some(worker) = &mut self.path_worker {
            worker.cancel();
        }
        if self.query.trim().is_empty() {
            self.path_results = None;
            self.query_duration = None;
            return;
        }
        let Some(doc) = &self.doc else {
            return;
        };
        self.pending_expression = self.query.clone();
        if let Some(worker) = &mut self.path_worker {
            if let Err(error) = worker.submit(Arc::clone(doc), Operation::Query(self.query.clone()))
            {
                self.finish_path(Err(error), Duration::ZERO);
            }
        } else {
            let started = Instant::now();
            let result = doc.query_path(&self.query).map(|ids| {
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
                self.error = None;
                let first = ids.first().copied();
                self.path_results = Some(PathResults {
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
                    expression: self.pending_expression.clone(),
                    ids: Arc::new(NodeIds::default()),
                    preview: String::new(),
                    error: Some(error),
                });
            }
        }
        self.query_duration = Some(("Path", elapsed + started.elapsed()));
    }
    fn poll_operations(&mut self, ctx: &egui::Context) {
        if self.search_due.is_some_and(|due| Instant::now() >= due) {
            self.search();
        }
        if let Some(completed) = self.search_worker.as_mut().and_then(Worker::poll) {
            self.finish_search(
                completed.result.map(|output| match output {
                    Output::Search(ids) => ids,
                    _ => unreachable!(),
                }),
                completed.elapsed,
            );
        }
        if let Some(completed) = self.path_worker.as_mut().and_then(Worker::poll) {
            let result = completed.result.map(|output| match output {
                Output::Query {
                    expression,
                    ids,
                    preview,
                } => {
                    self.pending_expression = expression;
                    (ids, preview)
                }
                _ => unreachable!(),
            });
            self.finish_path(result, completed.elapsed);
        }
        if self.search_due.is_some()
            || self.search_worker.as_ref().is_some_and(|w| w.busy)
            || self.path_worker.as_ref().is_some_and(|w| w.busy)
        {
            ctx.request_repaint_after(Duration::from_millis(50));
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
                Ok(PreviewOutput::Selection {
                    reference,
                    preview,
                    path,
                }) => {
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
    fn next_match(&mut self, backwards: bool) {
        self.match_step(backwards, 1);
    }
    fn match_step(&mut self, backwards: bool, count: usize) {
        if self.progressive.session.is_some() {
            self.progressive_match(backwards, count);
            return;
        }
        if self.matches.is_empty() {
            return;
        }
        let count = count % self.matches.len();
        self.match_index = if backwards {
            (self.match_index + self.matches.len() - count) % self.matches.len()
        } else {
            (self.match_index + count) % self.matches.len()
        };
        self.reveal(self.matches[self.match_index] as usize);
    }
    fn copy(&mut self, ctx: &egui::Context, path: bool) {
        if self.progressive.session.is_some() {
            self.progressive_copy(ctx, path);
            return;
        }
        if self.discovering() {
            if path
                && let Some(page) = &self.top_page
                && page.selection.pointer.is_some()
            {
                ctx.copy_text(page.selection.path.clone());
            }
            return;
        }
        if path {
            if let Some(doc) = &self.doc {
                ctx.copy_text(doc.path(self.selected));
                self.notice = "Path copied".into();
            }
        } else if let Some(doc) = &self.doc {
            self.copy_selection(Arc::clone(doc), Selection::Node(self.selected), ctx);
        }
    }
    fn copy_selection(&mut self, doc: Arc<Document>, selection: Selection, ctx: &egui::Context) {
        match selection.json(&doc, Some(COPY_LIMIT)) {
            Ok(json) => {
                ctx.copy_text(json);
                self.notice = "JSON copied".into();
            }
            Err(CopyError::TooLarge) => {
                self.large_copy = Some(CopyRequest { doc, selection });
            }
            Err(CopyError::Serialization(error)) => self.error = Some(error),
        }
    }
    fn export(&mut self) {
        if self.progressive.session.is_some() {
            self.progressive_export(false);
            return;
        }
        if self.discovering() {
            return;
        }
        if let Some(doc) = &self.doc {
            self.export_selection(Arc::clone(doc), Selection::Node(self.selected));
        }
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
                        selection.write(&doc, writer)
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
    fn poll_export(&mut self, ctx: &egui::Context) {
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
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(50))
                }
            }
        }
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
    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.large_copy = None;
            self.help = false;
            self.paste_open = false;
            self.colors_open = false;
            self.vim.record("Esc");
            if let Some(id) = ctx.memory(|m| m.focused()) {
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
            return;
        }
        if ctx.input(|i| i.key_pressed(Key::F1)) {
            self.help = !self.help;
            self.vim.cancel();
            return;
        }
        if self.help || self.paste_open || self.colors_open || self.large_copy.is_some() {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::O)) {
            self.open(ctx);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::F)) {
            self.start_search(false);
        }
        if ctx.input(|i| i.key_pressed(Key::F3)) {
            self.next_match(ctx.input(|i| i.modifiers.shift));
        }
        if ctx.input(|i| i.key_pressed(Key::F5))
            && let Some(file) = self.file.clone()
        {
            self.load_file(file, ctx);
        }
        if self.progressive.session.is_some() {
            self.progressive_keyboard(ctx);
            return;
        }
        if self.discovering() && !ctx.wants_keyboard_input() {
            let count = if self.top_expanded {
                self.discovery.as_ref().unwrap().statistics().rows
            } else {
                1
            };
            let before = self.top_selected;
            if ctx.input(|i| i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::J)) {
                self.top_selected = self
                    .top_selected
                    .saturating_add(1)
                    .min(count.saturating_sub(1));
            }
            if ctx.input(|i| i.key_pressed(Key::ArrowUp) || i.key_pressed(Key::K)) {
                self.top_selected = self.top_selected.saturating_sub(1);
            }
            if ctx.input(|i| i.key_pressed(Key::Home)) {
                self.top_selected = 0;
            }
            if ctx.input(|i| i.key_pressed(Key::End)) {
                self.top_selected = count.saturating_sub(1);
            }
            if ctx.input(|i| i.key_pressed(Key::PageDown)) {
                self.top_selected = self
                    .top_selected
                    .saturating_add(self.page_rows as u64)
                    .min(count.saturating_sub(1));
            }
            if ctx.input(|i| i.key_pressed(Key::PageUp)) {
                self.top_selected = self.top_selected.saturating_sub(self.page_rows as u64);
            }
            if self.top_selected != before {
                self.scroll_to = true;
            }
            if let Some(discovery) = &self.discovery {
                discovery.request(self.top_start, self.top_selected);
            }
            return;
        }
        if ctx.wants_keyboard_input() {
            self.vim.cancel();
            return;
        }
        if !ctx.input(|i| i.modifiers.ctrl || i.modifiers.alt || i.modifiers.command)
            && ctx.input_mut(|i| {
                let pressed = i
                    .events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Text(t) if t == "."));
                if pressed {
                    i.events
                        .retain(|e| !matches!(e, egui::Event::Text(t) if t == "."));
                }
                pressed
            })
        {
            self.vim.record(".");
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("query_field")));
            return;
        }
        if let Some(text) = ctx.input_mut(|i| {
            i.events.iter_mut().find_map(|e| {
                if let egui::Event::Paste(t) = e {
                    Some(std::mem::take(t))
                } else {
                    None
                }
            })
        }) {
            self.load_text(text, "Clipboard".into(), ctx);
        }
        if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
            self.copy(ctx, false);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::P)) {
            self.copy(ctx, true);
        }
        for (key, label, direction, half) in [
            (Key::D, "Ctrl+d", 1, true),
            (Key::U, "Ctrl+u", -1, true),
            (Key::B, "Ctrl+b", -1, false),
            (Key::E, "Ctrl+e", 1, false),
        ] {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, key)) {
                let count = self.vim.take_count(label);
                self.move_rows(
                    direction,
                    count
                        * if half {
                            (self.page_rows / 2).max(1)
                        } else {
                            self.page_rows
                        },
                );
            }
        }
        // Printable normal-mode commands use text events to preserve Shift (N, G, H...).
        if !ctx.input(|i| i.modifiers.ctrl || i.modifiers.alt || i.modifiers.command) {
            self.vim_navigation(ctx);
        }
        if self.doc.is_none() {
            return;
        }
        for (key, label, action) in [
            (Key::ArrowDown, "↓", Action::Move(1)),
            (Key::ArrowUp, "↑", Action::Move(-1)),
            (Key::ArrowLeft, "←", Action::Left),
            (Key::ArrowRight, "→", Action::Right),
            (Key::Home, "Home", Action::First),
            (Key::End, "End", Action::Last),
            (Key::Space, "Space", Action::Toggle),
        ] {
            if ctx.input(|i| i.key_pressed(key)) {
                let count = self.vim.take_count(label);
                self.execute_vim(
                    Command {
                        action,
                        count,
                        explicit_count: false,
                    },
                    ctx,
                );
            }
        }
        for (key, label, direction) in [(Key::PageDown, "PageDown", 1), (Key::PageUp, "PageUp", -1)]
        {
            if ctx.input(|i| i.key_pressed(key)) {
                self.vim.record(label);
                self.move_rows(direction, self.page_rows);
            }
        }
    }
}

impl eframe::App for Viewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}
impl Viewer {
    fn selection_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.progressive.page.is_some() {
            self.progressive_selection(ui, ctx);
            return;
        }
        if self.discovering() {
            let page = self.top_page.as_ref().unwrap().clone();
            let mut wrap_changed = false;
            ui.horizontal_wrapped(|ui| {
                ui.heading("Value preview");
                ui.colored_label(
                    kind_color(page.selection.kind.name(), self.colors.active()),
                    page.selection.kind.name(),
                );
                wrap_changed = ui.checkbox(&mut self.colors.wrap_values, "Wrap").changed();
            });
            if wrap_changed {
                self.save_colors();
            }
            let path = &page.selection.path;
            ui.add(egui::Label::new(RichText::new(path).monospace()).truncate());
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled(false, egui::Button::new("Copy JSON"));
                if ui
                    .add_enabled(
                        page.selection.pointer.is_some(),
                        egui::Button::new("Copy path"),
                    )
                    .clicked()
                {
                    self.copy(ctx, true);
                }
                ui.add_enabled(false, egui::Button::new("Export"));
            });
            ui.separator();
            json_preview(
                ui,
                &page.detail,
                self.colors.active(),
                "value_preview",
                self.colors.wrap_values,
            );
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.heading("Value preview");
            let node = self.doc.as_ref().unwrap().node_info(self.selected);
            ui.colored_label(kind_color(node.kind(), self.colors.active()), node.kind());
            if ui
                .checkbox(&mut self.colors.wrap_values, "Wrap")
                .on_hover_text("Automatically wrap long values")
                .changed()
            {
                self.save_colors();
            }
        });
        let path = &self.selected_path;
        ui.add(egui::Label::new(RichText::new(path).monospace()).truncate())
            .on_hover_text(path);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Copy JSON").clicked() {
                self.copy(ctx, false);
            }
            if ui.button("Copy path").clicked() {
                self.copy(ctx, true);
            }
            if ui.button("Export").clicked() {
                self.export();
            }
        });
        ui.separator();
        json_preview(
            ui,
            &self.detail,
            self.colors.active(),
            "value_preview",
            self.colors.wrap_values,
        );
    }
    fn results_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.progressive.session.is_some() {
            self.progressive_results(ui, ctx);
            return;
        }
        if self.discovering() {
            ui.heading("JSONPath results");
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.heading("JSONPath results");
            if ui
                .checkbox(&mut self.colors.wrap_results, "Wrap")
                .on_hover_text("Automatically wrap query results")
                .changed()
            {
                self.save_colors();
            }
        });
        let Some(results) = &self.path_results else {
            ui.add_space(12.0);
            ui.label("Run a JSONPath query to explore matching values.");
            ui.monospace("$.users[*].name");
            ui.monospace("$..email");
            return;
        };
        ui.add(egui::Label::new(RichText::new(&results.expression).monospace()).truncate())
            .on_hover_text(&results.expression);
        let mut copy = false;
        let mut export = false;
        let mut reset = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} matches", results.ids.len()));
            ui.add_enabled_ui(results.error.is_none(), |ui| {
                copy = ui.button("Copy").clicked();
                export = ui.button("Export").clicked();
            });
            reset = ui.button("Clear").clicked();
        });
        ui.separator();
        if let Some(error) = &results.error {
            ui.colored_label(Color32::LIGHT_RED, error);
        } else {
            json_preview(
                ui,
                &results.preview,
                self.colors.active(),
                "path_results",
                self.colors.wrap_results,
            );
        }
        if copy || export {
            let doc = Arc::clone(self.doc.as_ref().unwrap());
            let selection = Selection::Results(Arc::clone(&results.ids));
            if copy {
                self.copy_selection(doc, selection, ctx);
            } else {
                self.export_selection(doc, selection);
            }
        }
        if reset {
            self.path_results = None;
            if let Some(worker) = &mut self.path_worker {
                worker.cancel();
            }
            self.query.clear();
            self.query_duration = None;
        }
    }
    fn draw(&mut self, ctx: &egui::Context) {
        if self.preview_worker.is_none() {
            self.preview_worker = Some(PreviewWorker::new(ctx.clone()));
        }
        self.poll_previews();
        if self
            .preview_worker
            .as_ref()
            .is_some_and(|worker| worker.selection_busy || worker.rows_busy)
        {
            ctx.request_repaint_after(Duration::from_millis(25));
        }
        self.search_worker
            .get_or_insert_with(|| Worker::new(ctx.clone()));
        self.path_worker
            .get_or_insert_with(|| Worker::new(ctx.clone()));
        self.poll();
        self.refresh_memory_budget(ctx, false);
        if let Some(text) = self.progressive.clipboard.take() {
            ctx.copy_text(text);
        }
        self.poll_export(ctx);
        self.poll_operations(ctx);
        self.keyboard(ctx);
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()))
        {
            self.load_file(path, ctx);
        }
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if let Some(logo) = &self.logo {
                    ui.image((logo.id(), egui::vec2(24.0, 24.0)));
                }
                ui.label(
                    RichText::new("FerrisJson")
                        .strong()
                        .size(18.0)
                        .color(Color32::from_rgb(96, 180, 250)),
                );
                ui.separator();
                ui.add_enabled_ui(self.pending.is_none(), |ui| {
                    if ui.button("Open file").clicked() {
                        self.open(ctx);
                    }
                    if ui.button("Paste JSON").clicked() {
                        self.paste_open = true;
                    }
                    if ui.checkbox(&mut self.colors.low_memory_load, "Low memory load")
                        .on_hover_text("Release the previous document and wait for its background work before loading. If loading fails, reopen the previous document.").changed() {
                        self.save_colors();
                    }
                    if ui.checkbox(&mut self.colors.indexed_files, "Indexed files")
                        .on_hover_text("Windows files use resident tree and search metadata. This setting applies to legacy file paths on other platforms; paste/stdin keep their memory backend.").changed() { self.save_colors(); }
                });
                if ui.button("Help").clicked() {
                    self.help = true;
                }
                ui.menu_button("Settings", |ui| self.memory_settings_menu(ui, ctx));
                if self.pending.is_some() || (self.progressive.opening() && self.progressive.page.is_none()) {
                    ui.spinner();
                    if (self.indexing || self.progressive.opening()) && ui.button("Cancel open").clicked() {
                        self.loading_cancelled.store(true, Ordering::Relaxed);
                        self.notice = "Cancelling indexed opening...".into();
                    }
                }
                if self.pending_export.is_some() || self.progressive.export_busy() {
                    ui.spinner();
                    if ui.button("Cancel export").clicked() {
                        self.export_cancelled.store(true, Ordering::Relaxed);
                        self.progressive_cancel_export();
                        self.notice = "Cancelling export...".into();
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Colors").clicked() {
                        self.vim.cancel();
                        self.colors_open = true;
                    }
                    let old_preset = self.colors.preset;
                    egui::ComboBox::from_id_salt("json_palette")
                        .width(95.0)
                        .selected_text(self.colors.preset.name())
                        .show_ui(ui, |ui| {
                            for preset in Preset::ALL {
                                ui.selectable_value(&mut self.colors.preset, preset, preset.name());
                            }
                        });
                    if self.colors.preset != old_preset {
                        self.colors.active().apply(ctx, self.dark);
                        self.save_colors();
                    }
                    if ui
                        .button(if self.dark { "Light" } else { "Dark" })
                        .clicked()
                    {
                        self.dark = !self.dark;
                        self.colors.dark = self.dark;
                        self.colors.active().apply(ctx, self.dark);
                        self.save_colors();
                    }
                });
            });
            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                let live_changed = ui.checkbox(&mut self.colors.live_search, "Live search")
                    .on_hover_text("Search after typing pauses for 200 ms. When off, press Enter to search.")
                    .changed();
                if live_changed {
                    self.save_colors();
                }
                let mut changed = ui
                    .toggle_value(&mut self.case, "Aa")
                    .on_hover_text("Case sensitive")
                    .changed();
                changed |= live_changed;
                changed |= ui
                    .toggle_value(&mut self.regex, ".*")
                    .on_hover_text("Regular expression")
                    .changed();
                egui::ComboBox::from_id_salt("scope")
                    .selected_text(match self.scope {
                        SearchScope::All => "All",
                        SearchScope::Keys => "Keys",
                        SearchScope::Values => "Values",
                    })
                    .show_ui(ui, |ui| {
                        for (scope, label) in [
                            (SearchScope::All, "All"),
                            (SearchScope::Keys, "Keys"),
                            (SearchScope::Values, "Values"),
                        ] {
                            changed |= ui.selectable_value(&mut self.scope, scope, label).changed();
                        }
                    });
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.term)
                        .hint_text(if self.search_reverse {
                            "? Search backward..."
                        } else {
                            "/ Search forward..."
                        })
                        .desired_width((ui.available_width() - 185.0).max(140.0)),
                );
                if self.focus_search {
                    response.request_focus();
                    self.focus_search = false;
                }
                changed |= response.changed();
                if response.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    response.surrender_focus();
                }
                if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    response.surrender_focus();
                    self.search();
                    changed = false;
                }
                if changed {
                    self.schedule_search(ctx);
                }
                if self.search_due.is_some() || self.search_worker.as_ref().is_some_and(|w| w.busy) || self.progressive.search_busy()
                {
                    ui.spinner();
                }
                ui.label(if self.progressive.session.is_some() {
                    self.progressive.match_label()
                } else if self.matches.is_empty() {
                    "0 / 0".into()
                } else {
                    format!("{} / {}", self.match_index + 1, self.matches.len())
                });
                if ui.button("<").clicked() {
                    self.next_match(true);
                }
                if ui.button(">").clicked() {
                    self.next_match(false);
                }
                if ui.button("Clear").clicked() {
                    self.term.clear();
                    self.search();
                }
            });
            if let Some(error) = &self.search_error {
                ui.colored_label(Color32::LIGHT_RED, error);
            }
            ui.horizontal(|ui| {
                ui.label("JSONPath");
                if self.path_worker.as_ref().is_some_and(|w| w.busy) || self.progressive.query_busy() {
                    ui.spinner();
                }
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .id(egui::Id::new("query_field"))
                        .font(egui::TextStyle::Monospace)
                        .hint_text("$.users[*].name or $.users[?(@.active)]")
                        .desired_width((ui.available_width() - 270.0).max(100.0)),
                );
                if response.changed() && self.query.trim().is_empty() {
                    self.path_results = None;
                    self.progressive_clear_query();
                    if let Some(worker) = &mut self.path_worker {
                        worker.cancel();
                    }
                    self.query_duration = None;
                }
                if ui.button("Go").clicked()
                    || (response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)))
                {
                    self.go_to_path();
                }
                if ui
                    .button("Reset")
                    .on_hover_text("Clear JSONPath results and show selected value")
                    .clicked()
                {
                    self.query.clear();
                    self.path_results = None;
                    self.progressive_clear_query();
                    if let Some(worker) = &mut self.path_worker {
                        worker.cancel();
                    }
                    self.query_duration = None;
                }
                if ui.add_enabled(!self.discovering(), egui::Button::new("Expand all")).clicked() {
                    if let Some(s) = &self.progressive.session { s.action(crate::engine::recursive::Action::All(true)); }
                    self.expanded.fill(true);
                    self.rebuild_rows();
                }
                if ui.add_enabled(!self.discovering(), egui::Button::new("Collapse all")).clicked() {
                    if let Some(s) = &self.progressive.session { s.action(crate::engine::recursive::Action::All(false)); }
                    self.expanded.fill(false);
                    self.select(0);
                    self.rebuild_rows();
                }
            });
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some((label, duration)) = self.query_duration {
                        ui.label(format!("{label}: {}", format_duration(duration))).on_hover_text("Includes query, node reveal and selection preview; excludes painting.");
                    }
                    if self.pending.is_some() && !self.discovering() {
                        if self.indexing { ui.label(self.indexed_progress.status()); }
                        ui.label(if self.waiting_for_old.load(Ordering::Relaxed) {
                            "Waiting for previous document..."
                        } else { "Opening..." });
                    } else if let Some(duration) = self.open_duration {
                        ui.label(format!("Open: {}", format_duration(duration))).on_hover_text("Includes file read (when applicable), JSON parse, node index and initial view preparation; excludes file picker and painting.");
                    }
                    ui.separator();
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.label(RichText::new(if ctx.wants_keyboard_input() { "INPUT" } else { "NORMAL" }).monospace().strong());
                        ui.label(RichText::new(self.vim.display()).monospace().color(Color32::from_rgb(100, 180, 235)));
                        if let Some(doc) = &self.doc && !self.discovering() {
                            ui.label(doc.backend_name());
                            ui.label(format!("{} nodes", doc.node_count())).on_hover_text(format!("{:.1} KB", doc.bytes as f64 / 1024.0));
                        }
                        if let Some((position, path)) = self.cursor_location() {
                            ui.label(RichText::new(position).monospace()).on_hover_text("Current visible tree row / total visible rows");
                            let width = (ui.available_width() * 0.6).min(360.0);
                            ui.add_sized([width, 18.0], egui::Label::new(RichText::new(&path).monospace()).truncate()).on_hover_text(path);
                        }
                        ui.add(egui::Label::new(&self.notice).truncate());
                    });
                });
            });
        });
        if self.doc.is_some() || self.discovering() || self.progressive.page.is_some() {
            egui::SidePanel::right("inspector")
                .default_width(360.0)
                .width_range(220.0..=700.0)
                .resizable(true)
                .show(ctx, |ui| {
                    let rect = ui.available_rect_before_wrap();
                    let divider_height = 10.0;
                    let usable = (rect.height() - divider_height).max(1.0);
                    let min = (80.0 / usable).min(0.45);
                    self.inspector_ratio = self.inspector_ratio.clamp(min, 1.0 - min);
                    let split_y = rect.top() + usable * self.inspector_ratio;
                    let top = egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), split_y));
                    let divider = egui::Rect::from_min_max(
                        egui::pos2(rect.left(), split_y),
                        egui::pos2(rect.right(), split_y + divider_height),
                    );
                    let bottom = egui::Rect::from_min_max(
                        egui::pos2(rect.left(), split_y + divider_height),
                        rect.max,
                    );
                    ui.allocate_rect(rect, egui::Sense::hover());
                    let response = ui
                        .interact(
                            divider,
                            ui.id().with("vertical-divider"),
                            egui::Sense::drag(),
                        )
                        .on_hover_cursor(egui::CursorIcon::ResizeVertical);
                    if response.dragged()
                        && let Some(pos) = response.interact_pointer_pos()
                    {
                        self.inspector_ratio =
                            ((pos.y - rect.top()) / usable).clamp(min, 1.0 - min);
                        ctx.request_repaint();
                    }
                    ui.painter().hline(
                        divider.x_range(),
                        divider.center().y,
                        ui.visuals().widgets.noninteractive.bg_stroke,
                    );
                    for x in [-8.0, 0.0, 8.0] {
                        ui.painter().circle_filled(
                            divider.center() + egui::vec2(x, 0.0),
                            1.5,
                            ui.visuals().text_color(),
                        );
                    }
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(top)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                        |ui| {
                            ui.set_clip_rect(top);
                            self.selection_panel(ui, ctx);
                        },
                    );
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(bottom)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                        |ui| {
                            ui.set_clip_rect(bottom);
                            self.results_panel(ui, ctx);
                        },
                    );
                });
        }
        let mut missing_rows = Vec::with_capacity(ROW_BATCH);
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(error) = self.error.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(Color32::LIGHT_RED, error);
                    if ui.small_button("Dismiss").clicked() {
                        self.error = None;
                    }
                });
                ui.separator();
            }
            if self.progressive.page.is_some() {
                self.progressive_tree(ui);
                return;
            }
            if self.discovering() {
                self.top_tree(ui);
                return;
            }
            if self.doc.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.label(
                        RichText::new("{ }")
                            .size(64.0)
                            .color(Color32::from_rgb(96, 180, 250)),
                    );
                    ui.heading("Explore your JSON");
                    ui.label("Open, paste or drag a JSON file here.");
                    ui.add_space(20.0);
                    if ui.button("Open JSON file").clicked() {
                        self.open(ctx);
                    }
                });
                return;
            }
            ui.label(RichText::new(&self.source).strong());
            ui.separator();
            let row_height = 25.0;
            let row_stride = row_height + ui.spacing().item_spacing.y;
            self.page_rows = (ui.available_height() / row_stride).floor().max(1.0) as usize;
            let mut area = egui::ScrollArea::both()
                .id_salt("tree")
                .auto_shrink([false, false]);
            if self.scroll_to
                && let Some(pos) = self
                    .rows
                    .iter()
                    .position(|&id| id as usize == self.selected)
            {
                area = area.vertical_scroll_offset(
                    (pos as f32 * row_stride - ui.available_height() / 2.0).max(0.0),
                );
                self.scroll_to = false;
            }
            let mut select = None;
            let mut toggle = None;
            area.show_rows(ui, row_height, self.rows.len(), |ui, range| {
                self.viewport_first = range.start;
                for row in range {
                    let id = self.rows[row] as usize;
                    let doc = self.doc.as_ref().unwrap();
                    let node = &doc.node_info(id);
                    let labels = if let Some(labels) = self.row_labels.get(id) {
                        Cow::Borrowed(labels)
                    } else {
                        if missing_rows.len() < ROW_BATCH {
                            missing_rows.push(id);
                        }
                        Cow::Owned((doc.key(id).chars().take(160).collect(), "Loading...".into()))
                    };
                    let (row_key, row_preview) = labels.as_ref();
                    let is_match = self.matches.binary_search(&(id as u32)).is_ok();
                    let palette = self.colors.active();
                    let fill = if id == self.selected {
                        color(palette.selection)
                    } else if row % 2 == 0 {
                        color(palette.stripe)
                    } else {
                        Color32::TRANSPARENT
                    };
                    egui::Frame::new().fill(fill).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.set_height(row_height);
                            ui.add_space(node.depth as f32 * 18.0);
                            if node.container() {
                                if ui
                                    .add_sized(
                                        [18.0, 18.0],
                                        egui::Button::new(if self.expanded[id] {
                                            "▾"
                                        } else {
                                            "▸"
                                        })
                                        .frame(false),
                                    )
                                    .clicked()
                                {
                                    toggle = Some(id);
                                }
                            } else {
                                ui.add_space(26.0);
                            }
                            let mut key = RichText::new(row_key.as_str())
                                .monospace()
                                .color(kind_color(node.kind(), self.colors.active()));
                            if is_match {
                                key = key.background_color(color(palette.search_match));
                            }
                            let response =
                                ui.add(egui::Label::new(key).sense(egui::Sense::click()));
                            if response.clicked() {
                                select = Some(id);
                            }
                            if response.double_clicked() && node.container() {
                                toggle = Some(id);
                            }
                            ui.label(":");
                            let response = ui.add(
                                egui::Label::new(
                                    RichText::new(row_preview.as_str())
                                        .monospace()
                                        .color(kind_color(node.kind(), self.colors.active())),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if response.clicked() {
                                select = Some(id);
                            }
                        });
                    });
                }
            });
            if let Some(id) = select {
                self.select(id);
            }
            if let Some(id) = toggle {
                self.toggle(id);
            }
        });
        if missing_rows.is_empty() && !self.requested_rows.is_empty() {
            if let Some(worker) = &mut self.preview_worker {
                worker.cancel_rows();
            }
            self.requested_rows.clear();
        }
        if !missing_rows.is_empty()
            && missing_rows != self.requested_rows
            && let (Some(doc), Some(worker)) = (&self.doc, &mut self.preview_worker)
        {
            let references = missing_rows
                .iter()
                .map(|&id| doc.node_ref(id).unwrap())
                .collect();
            match worker.rows(Arc::clone(doc), references) {
                Ok(()) => self.requested_rows = missing_rows,
                Err(error) => self.error = Some(error),
            }
        }
        if self.paste_open {
            let mut open = true;
            let mut load = false;
            egui::Window::new("Paste JSON").open(&mut open).default_size([620.0, 400.0]).show(ctx, |ui| {
                ui.label("Paste JSON below, then select Load. Ctrl+V also opens JSON when the tree has focus.");
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut self.paste).font(egui::TextStyle::Monospace).desired_rows(16).desired_width(f32::INFINITY));
                });
                load = ui.add_enabled(self.pending.is_none(), egui::Button::new("Load JSON")).clicked();
            });
            self.paste_open = open;
            if load {
                self.load_paste(ctx);
                self.paste_open = false;
            }
        }
        crate::ui::cheatsheet(ctx, &mut self.help, self.colors.active());
        let changed = crate::ui::color_editor(
            ctx,
            &mut self.colors_open,
            &mut self.colors,
            &mut self.notice,
            &mut self.error,
        );
        if changed {
            self.dark = self.colors.dark;
            self.colors.active().apply(ctx, self.dark);
        }
        if self.large_copy.is_some() {
            let mut action = 0;
            let mut open = true;
            egui::Window::new("Large clipboard copy").open(&mut open).collapsible(false).show(ctx, |ui| {
                ui.label("Formatted JSON exceeds 8 MiB. Copying it may require substantial memory and pause the interface. Export writes directly to a file.");
                ui.horizontal(|ui| {
                    if ui.button("Copy anyway").clicked() { action = 1; }
                    if ui.button("Export instead").clicked() { action = 2; }
                    if ui.button("Cancel").clicked() { action = 3; }
                });
            });
            if !open {
                action = 3;
            }
            if action != 0 {
                let request = self.large_copy.take().unwrap();
                match action {
                    1 => match request.selection.json(&request.doc, None) {
                        Ok(json) => {
                            ctx.copy_text(json);
                            self.notice = "JSON copied".into();
                        }
                        Err(error) => self.error = Some(format!("Cannot copy: {error:?}")),
                    },
                    2 => self.export_selection(request.doc, request.selection),
                    _ => (),
                }
            }
        }
    }
}
impl Viewer {
    fn load_paste(&mut self, ctx: &egui::Context) {
        if self.pending.is_none() {
            let text = std::mem::take(&mut self.paste);
            self.load_text(text, "Pasted JSON".into(), ctx);
        }
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        self.clear_progressive();
        self.clear_top();
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

#[allow(dead_code, reason = "Progressive desktop benchmark entry")]
pub fn benchmark_recursive(
    path: PathBuf,
    indexed: bool,
    search_cases: bool,
) -> Result<serde_json::Value, String> {
    recursive_ui::benchmark(path, indexed, search_cases)
}

#[allow(dead_code, reason = "Streaming search desktop benchmark entry")]
pub fn benchmark_stream_search(
    path: PathBuf,
    indexed: bool,
    term: String,
) -> Result<serde_json::Value, String> {
    recursive_ui::benchmark_stream_search(path, indexed, term)
}

#[allow(dead_code, reason = "Original-UI top-level discovery benchmark entry")]
pub fn benchmark_top_level(path: PathBuf, indexed: bool) -> Result<serde_json::Value, String> {
    if cfg!(windows) {
        return recursive_ui::benchmark(path, indexed, false);
    }
    let ctx = egui::Context::default();
    let mut app = Viewer::empty();
    app.colors.indexed_files = indexed;
    let started = Instant::now();
    app.load_file(path, &ctx);
    let until = Instant::now() + Duration::from_secs(180);
    let (stage, statistics, budget, labels) = loop {
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1120.0, 760.0),
                )),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );
        let _ = ctx.tessellate(output.shapes, output.pixels_per_point);
        if app.discovering()
            && app
                .top_page
                .as_ref()
                .is_some_and(|p| p.rows.iter().any(|r| r.index > 0))
        {
            let discovery = app.discovery.as_ref().unwrap();
            break (
                "top-level",
                Some(discovery.statistics()),
                Some(discovery.budget()),
                app.top_page
                    .as_ref()
                    .unwrap()
                    .rows
                    .iter()
                    .take(4)
                    .map(|r| r.label.clone())
                    .collect::<Vec<_>>(),
            );
        }
        if app.pending.is_none() {
            if let Some(error) = &app.error {
                return Err(error.clone());
            }
            if app.doc.is_some() {
                break ("complete-document", None, None, Vec::new());
            }
        }
        if Instant::now() >= until {
            return Err("Top-level display timeout".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let first_frame_us = started.elapsed().as_micros() as u64;
    let cancel_started = Instant::now();
    app.loading_cancelled.store(true, Ordering::Relaxed);
    app.clear_top();
    while app.pending.is_some() {
        app.poll();
        if Instant::now() >= until {
            return Err("Full loader cancellation timeout".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(app);
    crate::shutdown::finish();
    let usage = budget.map(|b| b.usage());
    if usage.is_some_and(|u| u.memory != 0 || u.disk != 0) {
        return Err(format!("Discovery resources retained: {usage:?}"));
    }
    let legacy_usage = crate::engine::budget::BudgetManager::default().usage();
    if legacy_usage.memory != 0 || legacy_usage.disk != 0 {
        return Err(format!(
            "Legacy loader resources retained: {legacy_usage:?}"
        ));
    }
    Ok(
        serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"scope":"headless original UI submission; includes normal full loader contention, not native presentation", "indexed":indexed,"stage":stage,"first_frame_us":first_frame_us,"discovery":statistics,"labels":labels,"cleanup_us":cancel_started.elapsed().as_micros() as u64,"discovery_resources_after_retire":usage,"legacy_resources_after_retire":legacy_usage}),
    )
}
fn read_file_cancellable(path: &std::path::Path, cancelled: &AtomicBool) -> Result<String, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("Loading cancelled".into());
    }
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let len = usize::try_from(file.metadata().map_err(|e| e.to_string())?.len())
        .map_err(|_| "Input size overflow")?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|e| e.to_string())?;
    let mut block = [0u8; 64 * 1024];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Loading cancelled".into());
        }
        let n = file.read(&mut block).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&block[..n]);
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}
fn wait_for_old_document(
    old: Option<Arc<Document>>,
    waiting: &AtomicBool,
    cancelled: &AtomicBool,
    ctx: &egui::Context,
) -> bool {
    crate::loading::release_previous(
        old,
        || cancelled.load(Ordering::Relaxed),
        |value| {
            waiting.store(value, Ordering::Relaxed);
            ctx.request_repaint();
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
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    struct TopInput(PathBuf);
    #[cfg(windows)]
    impl TopInput {
        fn new(text: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "FerrisJson-top-ui-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, text).unwrap();
            Self(path)
        }
    }
    #[cfg(windows)]
    impl Drop for TopInput {
        fn drop(&mut self) {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                if std::fs::remove_file(&self.0).is_ok() {
                    break;
                }
                if Instant::now() >= until {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
    #[cfg(windows)]
    fn attach_top(
        app: &mut Viewer,
        input: &TopInput,
        ctx: &egui::Context,
    ) -> mpsc::Sender<LoadResult> {
        let (tx, rx) = mpsc::channel();
        app.pending = Some(rx);
        app.top_source = input.0.display().to_string();
        app.discovery = Some(
            crate::engine::top_level::Discovery::start(
                crate::engine::top_level::Source::open(&input.0).unwrap(),
                Arc::new(AtomicBool::new(false)),
                ctx.clone(),
            )
            .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            app.poll_top();
            if app.top_page.as_ref().is_some_and(|p| p.rows.len() > 1) {
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
        tx
    }
    #[cfg(windows)]
    #[test]
    fn top_fields_use_original_panels_and_selection_transfers_to_final_document() {
        let text = r#"{"a/b":{"nested":1},"last":2}"#;
        let input = TopInput::new(text);
        let ctx = egui::Context::default();
        let mut app = Viewer::empty();
        let tx = attach_top(&mut app, &input, &ctx);
        app.top_selected = 1;
        app.discovery.as_ref().unwrap().request(0, 1);
        let until = Instant::now() + Duration::from_secs(5);
        while app.top_page.as_ref().unwrap().selected != 1 {
            app.poll_top();
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
        let output = with_io_context(
            Priority::Interactive,
            Cancellation::Flag(Arc::new(AtomicBool::new(true))),
            || {
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1120.0, 760.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| app.draw(ctx),
                )
            },
        );
        fn texts(shape: &egui::Shape, found: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(t) => found.push(t.galley.text().into()),
                egui::Shape::Vec(v) => {
                    for s in v {
                        texts(s, found);
                    }
                }
                _ => (),
            }
        }
        let mut found = Vec::new();
        for shape in output.shapes {
            texts(&shape.shape, &mut found);
        }
        for label in ["Value preview", "JSONPath results", "a/b"] {
            assert!(
                found.iter().any(|s| s == label),
                "Missing {label}: {found:?}"
            );
        }
        assert!(!found.iter().any(|s| s.contains("Opening settings")
            || s.contains("scanned")
            || s.contains("FerrisJson · Tree")));
        assert_eq!(app.inspector_ratio, 0.5);
        assert!(app.doc.is_none());
        app.copy(&ctx, false);
        app.export();
        assert!(app.large_copy.is_none() && app.pending_export.is_none());
        tx.send(Ok((
            Document::parse(text).unwrap(),
            input.0.display().to_string(),
            Some(input.0.clone()),
        )))
        .unwrap();
        app.poll();
        assert!(!app.discovering() && app.discovery.is_none());
        let doc = app.doc.as_ref().unwrap();
        assert_eq!(app.selected, doc.find_path("/a~1b").unwrap());
    }
    #[cfg(windows)]
    #[test]
    fn full_validation_failure_retracts_provisional_fields_and_restores_old_document() {
        let input = TopInput::new(r#"{"new":1} trailing"#);
        let ctx = egui::Context::default();
        let mut app = loaded();
        let old = app.doc.as_ref().unwrap().clone();
        let tx = attach_top(&mut app, &input, &ctx);
        assert!(app.discovering());
        tx.send(Err("Invalid JSON: trailing characters".into()))
            .unwrap();
        app.poll();
        assert!(app.discovery.is_none() && app.top_page.is_none());
        assert!(Arc::ptr_eq(app.doc.as_ref().unwrap(), &old));
        assert!(app.error.as_ref().unwrap().contains("trailing"));
    }
    #[test]
    fn cancelled_file_read_does_not_begin_loading() {
        let flag = AtomicBool::new(true);
        assert_eq!(
            read_file_cancellable(std::path::Path::new("not-opened.json"), &flag).unwrap_err(),
            "Loading cancelled"
        );
    }
    #[test]
    fn paste_moves_the_buffer_and_restores_it_on_syntax_error() {
        let ctx = egui::Context::default();
        let mut app = Viewer::empty();
        app.paste = "{bad}".into();
        let allocation = app.paste.as_ptr();
        app.load_paste(&ctx);
        assert_eq!(app.paste.capacity(), 0);
        wait_load(&mut app);
        assert_eq!(app.paste, "{bad}");
        assert_eq!(app.paste.as_ptr(), allocation);
        assert!(app.paste_open);
        app.paste = r#"{"fixed":42}"#.into();
        app.load_paste(&ctx);
        wait_load(&mut app);
        assert!(app.paste.is_empty());
        assert_eq!(
            *app.doc.as_ref().unwrap().value().unwrap(),
            serde_json::json!({"fixed":42})
        );
    }
    #[test]
    fn large_copy_requires_a_choice_and_does_not_silently_truncate() {
        let ctx = egui::Context::default();
        let mut app = Viewer::empty();
        let doc = Arc::new(
            Document::parse(&serde_json::json!("x".repeat(COPY_LIMIT + 1)).to_string()).unwrap(),
        );
        app.doc = Some(Arc::clone(&doc));
        app.copy(&ctx, false);
        assert!(app.large_copy.is_some());
        assert!(Arc::ptr_eq(&app.large_copy.as_ref().unwrap().doc, &doc));
        app.prepare_load();
        assert!(app.large_copy.is_none());
    }
    fn wait_load(app: &mut Viewer) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.pending.is_some()
            || app.progressive.opening()
            || app
                .progressive
                .session
                .as_ref()
                .is_some_and(|s| !s.statistics().ready)
            || (app.progressive.session.is_some()
                && app.progressive.page.as_ref().is_none_or(|p| !p.ready))
        {
            app.poll();
            assert!(Instant::now() < deadline, "Loading did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn indexed_file_loading_and_late_cancel_preserve_document_state() {
        let ctx = egui::Context::default();
        let mut app = loaded();
        app.colors.indexed_files = true;
        app.load_file(PathBuf::from("examples/sample.json"), &ctx);
        wait_load(&mut app);
        assert!(app.error.is_none());
        if cfg!(windows) {
            assert!(app.doc.is_none());
            assert_eq!(app.expanded.capacity(), 0);
            assert_eq!(app.history.capacity(), 0);
            assert!(app.progressive.page.as_ref().unwrap().ready);
            assert!(!app.progressive.page.as_ref().unwrap().detail.is_empty());
            app.clear_progressive();
            app = loaded();
        } else {
            assert!(app.doc.as_ref().unwrap().value().is_none());
            assert!(!app.detail.is_empty());
        }
        let old = Arc::clone(app.doc.as_ref().unwrap());
        let (tx, rx) = mpsc::channel();
        app.pending = Some(rx);
        app.indexing = true;
        app.loading_cancelled.store(true, Ordering::Relaxed);
        tx.send(Ok((Document::parse("[]").unwrap(), "late".into(), None)))
            .unwrap();
        app.poll();
        assert!(app.pending.is_none());
        assert!(Arc::ptr_eq(app.doc.as_ref().unwrap(), &old));
        assert!(app.notice.contains("cancelled"));
    }
    #[test]
    fn low_memory_loading_waits_releases_state_and_reports_failure() {
        let mut app = loaded();
        let ctx = egui::Context::default();
        let owner = Arc::clone(app.doc.as_ref().unwrap());
        let weak = Arc::downgrade(&owner);
        app.colors.low_memory_load = true;
        app.search_worker = Some(Worker::new(ctx.clone()));
        app.term = "name".into();
        app.search();
        app.load_text("{bad}".into(), "invalid".into(), &ctx);
        assert!(app.doc.is_none());
        assert_eq!(app.rows.capacity(), 0);
        assert_eq!(app.expanded.capacity(), 0);
        assert_eq!(app.matches.capacity(), 0);
        assert!(app.path_results.is_none());
        assert!(!app.search_worker.as_ref().unwrap().busy);
        assert!(app.waiting_for_old.load(Ordering::Relaxed));
        assert!(matches!(
            app.pending.as_ref().unwrap().try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(owner);
        wait_load(&mut app);
        assert!(weak.upgrade().is_none());
        assert!(app.doc.is_none());
        assert!(app.error.as_ref().unwrap().contains("Invalid JSON"));
        assert!(app.notice.contains("must be reopened"));
        assert!(!app.waiting_for_old.load(Ordering::Relaxed));
        app.load_text(r#"{"fresh":42}"#.into(), "valid".into(), &ctx);
        wait_load(&mut app);
        assert_eq!(
            *app.doc.as_ref().unwrap().value().unwrap(),
            serde_json::json!({"fresh":42})
        );
        assert_eq!(app.selected_path, "$");
        assert!(app.error.is_none());
    }
    #[test]
    fn normal_loading_preserves_previous_document_on_failure() {
        let ctx = egui::Context::default();
        let mut app = loaded();
        let old = Arc::downgrade(app.doc.as_ref().unwrap());
        app.load_text("{bad}".into(), "invalid".into(), &ctx);
        assert!(app.doc.is_some());
        wait_load(&mut app);
        assert!(app.error.is_some());
        assert!(Arc::ptr_eq(
            &old.upgrade().unwrap(),
            app.doc.as_ref().unwrap()
        ));
    }
    #[test]
    fn low_memory_file_failure_clears_previous_document() {
        let ctx = egui::Context::default();
        let mut app = loaded();
        app.colors.low_memory_load = true;
        app.load_file(
            PathBuf::from("target/nonexistent-load-test-directory/missing.json"),
            &ctx,
        );
        wait_load(&mut app);
        assert!(app.doc.is_none());
        assert!(app.error.as_ref().unwrap().contains("Cannot open"));
        assert!(app.notice.contains("must be reopened"));
    }
    fn loaded() -> Viewer {
        let mut app = Viewer::empty();
        let doc = Document::parse(DEMO).unwrap();
        app.expanded = vec![false; doc.node_count()];
        app.doc = Some(Arc::new(doc));
        app.select(0);
        app.rebuild_rows();
        app
    }
    fn wait_operations(app: &mut Viewer, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.poll_operations(ctx);
            if app.search_due.is_none()
                && !app.search_worker.as_ref().is_some_and(|w| w.busy)
                && !app.path_worker.as_ref().is_some_and(|w| w.busy)
            {
                return;
            }
            assert!(Instant::now() < deadline, "operations did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn indexed_cache_misses_do_not_read_files_on_the_ui_thread() {
        let path =
            std::env::temp_dir().join(format!("FerrisJson-S2-widget-{}.tmp", std::process::id()));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let input = Cleanup(path);
        std::fs::write(&input.0, r#"{"value":"on disk"}"#).unwrap();
        let mut app = Viewer::empty();
        let ctx = egui::Context::default();
        let doc =
            Arc::new(Document::open_indexed(&input.0, || false, &LoadProgress::default()).unwrap());
        app.expanded = vec![true; doc.node_count()];
        app.doc = Some(doc);
        app.preview_worker = Some(PreviewWorker::new(ctx.clone()));
        app.select(0);
        app.rebuild_rows();
        with_io_context(
            Priority::Interactive,
            Cancellation::Flag(Arc::new(AtomicBool::new(true))),
            || {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1120.0, 760.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| app.draw(ctx),
                );
            },
        );
        // The draw only submitted requests; synchronous row reading would cache an error here.
        assert!(app.row_labels.get(1).is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        while app
            .preview_worker
            .as_ref()
            .is_some_and(|worker| worker.selection_busy || worker.rows_busy)
        {
            app.poll_previews();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(app.row_labels.get(1).unwrap().1, "\"on disk\"");
        assert!(!app.detail.contains("cancelled"));
    }
    #[test]
    fn selections_are_asynchronous_and_latest_selection_wins() {
        let mut app = loaded();
        app.preview_worker = Some(PreviewWorker::new(egui::Context::default()));
        let first = app
            .doc
            .as_ref()
            .unwrap()
            .find_path("$.users[0].name")
            .unwrap();
        let last = app
            .doc
            .as_ref()
            .unwrap()
            .find_path("$.users[1].name")
            .unwrap();
        app.select(first);
        assert_eq!(app.detail, "Loading preview...");
        app.select(last);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.preview_worker.as_ref().unwrap().selection_busy {
            app.poll_previews();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(app.detail, "\"张三\"");
        assert_eq!(app.selected_path, "$.users[1].name");
        app.select(first);
        app.cancel_operations();
        app.doc = Some(Arc::new(Document::parse("null").unwrap()));
        app.detail = "new document".into();
        app.poll_previews();
        assert_eq!(app.detail, "new document");
    }
    #[test]
    fn background_operations_update_navigation_and_loading_invalidates_old_results() {
        let mut app = loaded();
        let ctx = egui::Context::default();
        app.search_worker = Some(Worker::new(ctx.clone()));
        app.path_worker = Some(Worker::new(ctx.clone()));
        app.term = "email".into();
        app.scope = SearchScope::Keys;
        app.search();
        assert!(app.matches.is_empty());
        wait_operations(&mut app, &ctx);
        assert_eq!(app.matches.len(), 2);
        assert!(app.rows.contains(&(app.selected as u32)));
        app.query = "$.users[*].name".into();
        app.go_to_path();
        wait_operations(&mut app, &ctx);
        assert_eq!(app.path_results.as_ref().unwrap().ids.len(), 2);
        app.search();
        app.go_to_path();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok((
            Document::parse(r#"{"fresh":true}"#).unwrap(),
            "new".into(),
            None,
        )))
        .unwrap();
        app.pending = Some(rx);
        app.poll();
        app.poll_operations(&ctx);
        assert!(app.matches.is_empty());
        assert!(app.path_results.is_none());
        assert_eq!(app.selected_path, "$");
        assert_eq!(
            *app.doc.as_ref().unwrap().value().unwrap(),
            serde_json::json!({"fresh":true})
        );
    }
    #[test]
    fn search_input_is_debounced_and_clearing_cancels_pending_work() {
        let mut app = loaded();
        app.colors.live_search = true;
        let ctx = egui::Context::default();
        app.search_worker = Some(Worker::new(ctx.clone()));
        app.term = "email".into();
        app.schedule_search(&ctx);
        assert!(app.search_due.unwrap() > Instant::now());
        assert!(!app.search_worker.as_ref().unwrap().busy);
        app.colors.live_search = false;
        app.schedule_search(&ctx);
        assert!(app.search_due.is_none());
        assert!(!app.search_worker.as_ref().unwrap().busy);
        app.colors.live_search = true;
        app.term.clear();
        app.schedule_search(&ctx);
        assert!(app.search_due.is_none());
        app.term = "email".into();
        app.scope = SearchScope::Keys;
        app.schedule_search(&ctx);
        app.search_due = Some(Instant::now());
        wait_operations(&mut app, &ctx);
        assert_eq!(app.matches.len(), 2);
    }
    #[test]
    fn manual_search_waits_for_execution_and_edits_cancel_old_results() {
        let mut app = loaded();
        let ctx = egui::Context::default();
        app.search_worker = Some(Worker::new(ctx.clone()));
        assert!(!app.colors.live_search);
        app.term = "email".into();
        app.scope = SearchScope::Keys;
        app.schedule_search(&ctx);
        wait_operations(&mut app, &ctx);
        assert!(app.matches.is_empty());
        assert!(app.query_duration.is_none());
        app.search();
        wait_operations(&mut app, &ctx);
        assert_eq!(app.matches.len(), 2);
        app.term = "name".into();
        app.schedule_search(&ctx);
        wait_operations(&mut app, &ctx);
        assert!(app.matches.is_empty());
        assert!(app.query_duration.is_none());
    }
    #[test]
    fn search_reveals_match_and_collapsing_preserves_visible_selection() {
        let mut app = loaded();
        app.term = "email".into();
        app.scope = SearchScope::Keys;
        app.search();
        assert_eq!(app.matches.len(), 2);
        let first = app.selected;
        assert!(app.rows.contains(&(first as u32)));
        app.next_match(false);
        assert_ne!(first, app.selected);
        app.next_match(false);
        assert_eq!(first, app.selected);
        app.next_match(true);
        assert_ne!(first, app.selected);
        app.toggle(0);
        assert_eq!(app.selected, 0);
        assert_eq!(app.rows, vec![0]);
        app.regex = true;
        app.term = "[".into();
        app.search();
        assert!(app.search_error.is_some());
        assert!(app.matches.is_empty());
    }
    #[test]
    fn ui_frames_render_empty_loaded_and_dialog_states() {
        let ctx = egui::Context::default();
        let mut app = Viewer::empty();
        for state in 0..6 {
            if state == 1 {
                app = loaded();
            }
            if state == 2 {
                app.help = true;
            }
            if state == 3 {
                app.help = false;
                app.paste_open = true;
            }
            if state == 4 {
                app.paste_open = false;
                app.query = "$.users[*].name".into();
                app.go_to_path();
            }
            if state == 5 {
                app.query = "$[?".into();
                app.go_to_path();
            }
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1120.0, 760.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            );
            assert!(!output.shapes.is_empty());
        }
    }
    #[test]
    fn operation_timings_cover_search_path_and_loading() {
        let mut app = loaded();
        app.term = "email".into();
        app.search();
        assert_eq!(app.query_duration.unwrap().0, "Search");
        app.query = "$.users[1].name".into();
        app.go_to_path();
        assert_eq!(app.query_duration.unwrap().0, "Path");
        assert_eq!(app.doc.as_ref().unwrap().preview(app.selected), "\"张三\"");
        app.query = "$.missing".into();
        app.go_to_path();
        assert!(app.path_results.as_ref().unwrap().ids.is_empty());
        assert!(app.query_duration.is_some());
        let (tx, rx) = mpsc::channel();
        app.pending = Some(rx);
        app.load_started = Some(Instant::now());
        tx.send(Ok((Document::parse(DEMO).unwrap(), "test".into(), None)))
            .unwrap();
        app.poll();
        assert!(app.open_duration.is_some());
        assert!(app.query_duration.is_none());
        assert!(app.load_started.is_none());
        assert_eq!(format_duration(Duration::from_micros(1234)), "1.23 ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.50 s");
    }
    #[test]
    fn timings_render_at_bottom_right() {
        let ctx = egui::Context::default();
        let mut app = loaded();
        app.open_duration = Some(Duration::from_millis(150));
        app.query_duration = Some(("Search", Duration::from_micros(250)));
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1120.0, 760.0),
                )),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );
        let mut timings = Vec::new();
        fn collect(shape: &egui::Shape, found: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::Shape::Text(text)
                    if text.galley.text().starts_with("Open:")
                        || text.galley.text().starts_with("Search:") =>
                {
                    found.push((text.galley.text().into(), text.pos));
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, found);
                    }
                }
                _ => (),
            }
        }
        for shape in output.shapes {
            collect(&shape.shape, &mut timings);
        }
        assert_eq!(timings.len(), 2);
        for (text, pos) in timings {
            assert!(pos.x > 800.0 && pos.y > 700.0, "{text} is at {pos:?}");
        }
    }
    #[test]
    fn jsonpath_results_survive_selection_and_invalid_queries_clear_old_results() {
        let mut app = loaded();
        app.query = "$.users[*].name".into();
        app.go_to_path();
        assert_eq!(app.path_results.as_ref().unwrap().ids.len(), 2);
        assert_eq!(app.query, "$.users[*].name");
        assert!(app.path_results.as_ref().unwrap().preview.contains("张三"));
        app.select(0);
        assert!(app.path_results.is_some());
        app.query = "$[?".into();
        app.go_to_path();
        assert!(app.path_results.as_ref().unwrap().error.is_some());
        assert!(app.path_results.as_ref().unwrap().ids.is_empty());
        app.path_results = None;
        app.select(1);
        assert_eq!(app.detail, "\"FerrisJson\"");
    }
    #[test]
    fn history_replays_positions_and_branches_after_new_navigation() {
        let mut app = loaded();
        app.reveal(2);
        app.reveal(3);
        app.history_move(true);
        assert_eq!(app.selected, 2);
        app.history_move(true);
        assert_eq!(app.selected, 0);
        app.history_move(true);
        assert_eq!(app.selected, 0);
        app.history_move(false);
        assert_eq!(app.selected, 2);
        app.reveal(4);
        app.history_move(false);
        assert_eq!(app.selected, 4);
        assert_eq!(app.history, vec![0, 2, 4]);
    }
    #[test]
    fn vim_keys_jump_and_replay_history_without_affecting_text_fields() {
        let ctx = egui::Context::default();
        let mut app = loaded();
        app.expanded.fill(true);
        app.rebuild_rows();
        app.reveal(3);
        let frame = |app: &mut Viewer, text: &str| {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Text(text.into())],
                    ..Default::default()
                },
                |ctx| app.keyboard(ctx),
            );
        };
        frame(&mut app, "g");
        assert_eq!(app.selected, 3);
        frame(&mut app, "g");
        assert_eq!(app.selected, 0);
        frame(&mut app, "G");
        assert_eq!(app.selected, *app.rows.last().unwrap() as usize);
        frame(&mut app, "[");
        assert_eq!(app.selected, 0);
        frame(&mut app, "]");
        assert_eq!(app.selected, *app.rows.last().unwrap() as usize);
        let selected = app.selected;
        ctx.memory_mut(|m| m.request_focus(egui::Id::new("query_field")));
        frame(&mut app, "g");
        frame(&mut app, "g");
        assert_eq!(app.selected, selected);
    }
    #[test]
    fn counted_movement_and_reverse_search_follow_vim_direction() {
        let ctx = egui::Context::default();
        let mut app = loaded();
        app.expanded.fill(true);
        app.rebuild_rows();
        let feed = |app: &mut Viewer, text: &str| {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Text(text.into())],
                    ..Default::default()
                },
                |ctx| app.keyboard(ctx),
            );
        };
        feed(&mut app, "5j");
        assert_eq!(app.selected, app.rows[5] as usize);
        assert_eq!(app.vim.display(), "5j");
        feed(&mut app, "2k");
        assert_eq!(app.selected, app.rows[3] as usize);
        feed(&mut app, "4G");
        assert_eq!(app.selected, app.rows[3] as usize);
        app.term = "email".into();
        app.start_search(true);
        app.search();
        let first = app.selected;
        assert_eq!(first, *app.matches.last().unwrap() as usize);
        app.execute_vim(
            Command {
                action: Action::Match(false),
                count: 1,
                explicit_count: false,
            },
            &ctx,
        );
        assert_eq!(app.selected, app.matches[0] as usize);
        app.execute_vim(
            Command {
                action: Action::Match(true),
                count: 1,
                explicit_count: false,
            },
            &ctx,
        );
        assert_eq!(app.selected, first);
        app.execute_vim(
            Command {
                action: Action::Match(false),
                count: 2,
                explicit_count: true,
            },
            &ctx,
        );
        assert_eq!(app.selected, first);
    }
    #[test]
    fn dot_focuses_jsonpath_without_inserting_and_preserves_normal_typing() {
        let ctx = egui::Context::default();
        let mut app = loaded();
        app.query = "$.users".into();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 760.0));
        let frame = |app: &mut Viewer, events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            );
        };
        frame(&mut app, vec![]);
        app.vim.feed('3');
        frame(&mut app, vec![egui::Event::Text(".".into())]);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(egui::Id::new("query_field"))
        );
        assert_eq!(app.query, "$.users");
        assert!(app.vim.pending.is_empty());
        // Once focused, the same character must go through TextEdit normally.
        frame(&mut app, vec![egui::Event::Text(".".into())]);
        assert_eq!(app.query.matches('.').count(), 2);
        assert_eq!(app.query.len(), "$.users".len() + 1);
        ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("query_field")));
        app.help = true;
        frame(&mut app, vec![egui::Event::Text(".".into())]);
        assert_ne!(
            ctx.memory(|m| m.focused()),
            Some(egui::Id::new("query_field"))
        );
    }
    #[test]
    fn search_triggers_do_not_become_search_text_and_escape_closes_help() {
        let ctx = egui::Context::default();
        let mut app = loaded();
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Text("?".into())],
                ..Default::default()
            },
            |ctx| {
                app.keyboard(ctx);
                assert!(app.search_reverse && app.focus_search);
                assert!(!ctx.input(|i| {
                    i.events
                        .iter()
                        .any(|e| matches!(e, egui::Event::Text(t) if t == "?"))
                }));
            },
        );
        app.help = true;
        app.vim.feed('3');
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| app.keyboard(ctx),
        );
        assert!(!app.help);
        assert!(app.vim.pending.is_empty());
    }
    #[test]
    fn inspector_keeps_both_previews_visible_and_divider_is_draggable() {
        let ctx = egui::Context::default();
        let mut app = loaded();
        app.query = "$.users[*].name".into();
        app.go_to_path();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 760.0));
        let mut run = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            )
        };
        let output = run(vec![]);
        let mut positions = Vec::new();
        for shape in &output.shapes {
            if let egui::Shape::Text(text) = &shape.shape
                && matches!(text.galley.text(), "Value preview" | "JSONPath results")
            {
                positions.push((text.galley.text().to_owned(), text.pos));
            }
        }
        assert_eq!(positions.len(), 2);
        assert!(positions[0].1.y < positions[1].1.y);
        // The lower header starts just below the divider; drag its center upward.
        let divider = egui::pos2(1000.0, positions[1].1.y - 5.0);
        run(vec![
            egui::Event::PointerMoved(divider),
            egui::Event::PointerButton {
                pos: divider,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        let target = divider - egui::vec2(0.0, 80.0);
        run(vec![egui::Event::PointerMoved(target)]);
        run(vec![egui::Event::PointerButton {
            pos: target,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!(app.inspector_ratio < 0.5);
    }
    #[test]
    fn cursor_location_tracks_visible_rows_and_json_path() {
        let mut app = loaded();
        assert_eq!(
            app.cursor_location().unwrap(),
            ("Ln 1/1 All".into(), "$".into())
        );
        app.expanded.fill(true);
        app.rebuild_rows();
        assert!(app.cursor_location().unwrap().0.ends_with("Top"));
        let middle = app.rows[app.rows.len() / 2] as usize;
        app.reveal(middle);
        let (position, path) = app.cursor_location().unwrap();
        assert!(position.ends_with('%'));
        assert_eq!(path, app.doc.as_ref().unwrap().path(middle));
        app.reveal(*app.rows.last().unwrap() as usize);
        assert!(app.cursor_location().unwrap().0.ends_with("Bot"));
        app.toggle(0);
        assert_eq!(app.cursor_location().unwrap().0, "Ln 1/1 All");
    }
}
