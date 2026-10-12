use crate::document::{Document, SearchScope};
use eframe::egui::{self, Color32, Key, RichText};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

const DEMO: &str = include_str!("../examples/sample.json");
type LoadResult = Result<(Document, String, Option<PathBuf>), String>;
struct PathResults {
    expression: String,
    ids: Vec<usize>,
    preview: String,
    error: Option<String>,
}

pub struct Viewer {
    doc: Option<Document>,
    source: String,
    file: Option<PathBuf>,
    pending: Option<Receiver<LoadResult>>,
    load_started: Option<Instant>,
    open_duration: Option<Duration>,
    query_duration: Option<(&'static str, Duration)>,
    expanded: Vec<bool>,
    selected: usize,
    rows: Vec<usize>,
    matches: Vec<usize>,
    match_index: usize,
    term: String,
    case: bool,
    regex: bool,
    scope: SearchScope,
    query: String,
    path_results: Option<PathResults>,
    history: Vec<usize>,
    history_index: usize,
    last_g: Option<Instant>,
    error: Option<String>,
    search_error: Option<String>,
    notice: String,
    paste_open: bool,
    paste: String,
    help: bool,
    focus_search: bool,
    scroll_to: bool,
    detail: String,
    dark: bool,
}

impl Viewer {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        file: Option<PathBuf>,
        text: Option<String>,
    ) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        cc.egui_ctx.set_fonts(crate::fonts::definitions());
        let mut app = Self::empty();
        if let Some(text) = text {
            app.load_text(text, "Standard input".into(), &cc.egui_ctx);
        } else if let Some(file) = file {
            app.load_file(file, &cc.egui_ctx);
        }
        app
    }
    fn empty() -> Self {
        Self {
            doc: None,
            source: "No document".into(),
            file: None,
            pending: None,
            load_started: None,
            open_duration: None,
            query_duration: None,
            expanded: Vec::new(),
            selected: 0,
            rows: Vec::new(),
            matches: Vec::new(),
            match_index: 0,
            term: String::new(),
            case: false,
            regex: false,
            scope: SearchScope::All,
            query: "$".into(),
            path_results: None,
            history: Vec::new(),
            history_index: 0,
            last_g: None,
            error: None,
            search_error: None,
            notice: String::new(),
            paste_open: false,
            paste: String::new(),
            help: false,
            focus_search: false,
            scroll_to: false,
            detail: String::new(),
            dark: true,
        }
    }
    fn load_file(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.pending.is_some() {
            return;
        }
        self.load_started = Some(Instant::now());
        self.open_duration = None;
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = std::fs::read_to_string(&path)
                .map_err(|e| format!("Cannot open {}: {e}", path.display()))
                .and_then(|text| Document::parse(&text))
                .map(|doc| (doc, path.display().to_string(), Some(path)));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn load_text(&mut self, text: String, source: String, ctx: &egui::Context) {
        if self.pending.is_some() {
            return;
        }
        self.load_started = Some(Instant::now());
        self.open_duration = None;
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Document::parse(&text).map(|doc| (doc, source, None)));
            ctx.request_repaint();
        });
    }
    fn poll(&mut self) {
        let Some(rx) = &self.pending else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                match result {
                    Ok((doc, source, file)) => {
                        self.expanded = doc.nodes.iter().map(|n| n.depth < 2).collect();
                        self.doc = Some(doc);
                        self.source = source;
                        self.file = file;
                        self.selected = 0;
                        self.term.clear();
                        self.matches.clear();
                        self.search_error = None;
                        self.query_duration = None;
                        self.path_results = None;
                        self.query = "$".into();
                        self.history.clear();
                        self.history_index = 0;
                        self.last_g = None;
                        self.notice = "Document loaded".into();
                        self.select(0);
                        self.rebuild_rows();
                    }
                    Err(error) => self.error = Some(error),
                }
                self.open_duration = self.load_started.take().map(|start| start.elapsed());
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                self.error = Some("Loading worker stopped".into());
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
        if let Some(doc) = &self.doc {
            self.rows = doc.visible(&self.expanded);
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
        self.selected = id;
        self.detail = doc.preview(id);
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
    fn vim_navigation(&mut self, ctx: &egui::Context) {
        let texts = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|event| {
                    if let egui::Event::Text(text) = event {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        });
        for text in texts {
            match text.as_str() {
                "g" => {
                    let now = Instant::now();
                    if self
                        .last_g
                        .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
                    {
                        self.reveal(0);
                        self.last_g = None;
                    } else {
                        self.last_g = Some(now);
                    }
                }
                "G" => {
                    self.last_g = None;
                    if let Some(&id) = self.rows.last() {
                        self.reveal(id);
                    }
                }
                "[" => {
                    self.last_g = None;
                    self.history_move(true);
                }
                "]" => {
                    self.last_g = None;
                    self.history_move(false);
                }
                _ => self.last_g = None,
            }
        }
    }
    fn reveal(&mut self, id: usize) {
        if let Some(doc) = &self.doc {
            let mut parent = doc.nodes[id].parent;
            while let Some(p) = parent {
                self.expanded[p] = true;
                parent = doc.nodes[p].parent;
            }
        }
        self.select(id);
        self.rebuild_rows();
        self.scroll_to = true;
    }
    fn search(&mut self) {
        let started = Instant::now();
        self.matches.clear();
        self.search_error = None;
        self.match_index = 0;
        if let Some(doc) = &self.doc {
            match doc.search(&self.term, self.case, self.regex, self.scope) {
                Ok(ids) => self.matches = ids,
                Err(error) => self.search_error = Some(error),
            }
        }
        if let Some(&id) = self.matches.first() {
            self.reveal(id);
        }
        self.query_duration = if self.doc.is_some() && !self.term.is_empty() {
            Some(("Search", started.elapsed()))
        } else {
            None
        };
    }
    fn go_to_path(&mut self) {
        if self.query.trim().is_empty() {
            self.path_results = None;
            self.query_duration = None;
            return;
        }
        let Some(doc) = &self.doc else {
            return;
        };
        let started = Instant::now();
        match doc.query_path(&self.query) {
            Ok(ids) => {
                self.error = None;
                let first = ids.first().copied();
                self.path_results = Some(PathResults {
                    expression: self.query.clone(),
                    preview: doc.results_preview(&ids),
                    ids,
                    error: None,
                });
                if let Some(id) = first {
                    self.reveal(id);
                }
            }
            Err(error) => {
                self.path_results = Some(PathResults {
                    expression: self.query.clone(),
                    ids: Vec::new(),
                    preview: String::new(),
                    error: Some(error),
                })
            }
        }
        self.query_duration = Some(("Path", started.elapsed()));
    }
    fn next_match(&mut self, backwards: bool) {
        if self.matches.is_empty() {
            return;
        }
        self.match_index = if backwards {
            (self.match_index + self.matches.len() - 1) % self.matches.len()
        } else {
            (self.match_index + 1) % self.matches.len()
        };
        self.reveal(self.matches[self.match_index]);
    }
    fn copy(&mut self, ctx: &egui::Context, path: bool) {
        if let Some(doc) = &self.doc {
            ctx.copy_text(if path {
                doc.nodes[self.selected].path.clone()
            } else {
                serde_json::to_string_pretty(doc.at(self.selected)).unwrap()
            });
            self.notice = if path { "Path copied" } else { "JSON copied" }.into();
        }
    }
    fn export(&mut self) {
        let Some(doc) = &self.doc else {
            return;
        };
        let json = serde_json::to_string_pretty(doc.at(self.selected)).unwrap();
        self.export_json(json);
    }
    fn export_json(&mut self, json: String) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name("selection.json")
            .save_file()
        {
            match std::fs::write(&path, json) {
                Ok(()) => self.notice = format!("Exported {}", path.display()),
                Err(error) => self.error = Some(format!("Cannot export: {error}")),
            }
        }
    }
    fn toggle(&mut self, id: usize) {
        self.expanded[id] = !self.expanded[id];
        if let Some(doc) = &self.doc
            && !self.expanded[id]
            && self.selected > id
            && self.selected < doc.nodes[id].end
        {
            self.select(id);
        }
        self.rebuild_rows();
    }
    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::O)) {
            self.open(ctx);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::F)) {
            self.focus_search = true;
        }
        if ctx.input(|i| i.key_pressed(Key::F1)) {
            self.help = !self.help;
        }
        if ctx.input(|i| i.key_pressed(Key::F3)) {
            self.next_match(ctx.input(|i| i.modifiers.shift));
        }
        if ctx.input(|i| i.key_pressed(Key::F5))
            && let Some(file) = self.file.clone()
        {
            self.load_file(file, ctx);
        }
        if ctx.wants_keyboard_input() || self.paste_open || self.help {
            self.last_g = None;
            return;
        }
        let paste = ctx.input(|i| {
            i.events.iter().find_map(|e| {
                if let egui::Event::Paste(t) = e {
                    Some(t.clone())
                } else {
                    None
                }
            })
        });
        if let Some(text) = paste {
            self.load_text(text, "Clipboard".into(), ctx);
        }
        if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
            self.copy(ctx, false);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::P)) {
            self.copy(ctx, true);
        }
        if ctx.input(|i| i.key_pressed(Key::Slash)) {
            self.focus_search = true;
        }
        if self.doc.is_none() {
            return;
        }
        self.vim_navigation(ctx);
        if ctx.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::Key { key, pressed: true, .. } if *key != Key::G))
        }) {
            self.last_g = None;
        }
        let position = self
            .rows
            .iter()
            .position(|&id| id == self.selected)
            .unwrap_or(0);
        if ctx.input(|i| i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::J)) {
            self.reveal(self.rows[(position + 1).min(self.rows.len() - 1)]);
        }
        if ctx.input(|i| i.key_pressed(Key::ArrowUp) || i.key_pressed(Key::K)) {
            self.reveal(self.rows[position.saturating_sub(1)]);
        }
        if ctx.input(|i| i.key_pressed(Key::Home)) {
            self.reveal(0);
        }
        if ctx.input(|i| i.key_pressed(Key::End)) {
            self.reveal(*self.rows.last().unwrap());
        }
        if ctx.input(|i| i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::L)) {
            let doc = self.doc.as_ref().unwrap();
            if doc.nodes[self.selected].container && !self.expanded[self.selected] {
                self.toggle(self.selected);
            } else if doc.nodes[self.selected].end > self.selected + 1 {
                self.reveal(self.selected + 1);
            }
        }
        if ctx.input(|i| i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::H)) {
            let doc = self.doc.as_ref().unwrap();
            if doc.nodes[self.selected].container && self.expanded[self.selected] {
                self.toggle(self.selected);
            } else if let Some(parent) = doc.nodes[self.selected].parent {
                self.reveal(parent);
            }
        }
        if ctx.input(|i| i.key_pressed(Key::Space)) {
            self.toggle(self.selected);
        }
        if ctx.input(|i| i.key_pressed(Key::N)) {
            self.next_match(ctx.input(|i| i.modifiers.shift));
        }
    }
}

impl eframe::App for Viewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}
impl Viewer {
    fn draw(&mut self, ctx: &egui::Context) {
        self.poll();
        self.keyboard(ctx);
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()))
        {
            self.load_file(path, ctx);
        }
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("{ }  JSON VIEWER")
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
                });
                if ui.button("Sample").clicked() {
                    self.load_text(DEMO.into(), "sample.json".into(), ctx);
                }
                if ui
                    .button(if self.dark { "Light" } else { "Dark" })
                    .clicked()
                {
                    self.dark = !self.dark;
                    ctx.set_visuals(if self.dark {
                        egui::Visuals::dark()
                    } else {
                        egui::Visuals::light()
                    });
                }
                if ui.button("Help").clicked() {
                    self.help = true;
                }
                if self.pending.is_some() {
                    ui.spinner();
                    ui.label("Loading...");
                }
            });
            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                let mut changed = ui
                    .toggle_value(&mut self.case, "Aa")
                    .on_hover_text("Case sensitive")
                    .changed();
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
                        .hint_text("Search keys and values...")
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
                    self.next_match(ui.input(|i| i.modifiers.shift));
                }
                if changed {
                    self.search();
                }
                ui.label(if self.matches.is_empty() {
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
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("$.users[*].name or $.users[?(@.active)]")
                        .desired_width((ui.available_width() - 270.0).max(100.0)),
                );
                if response.changed() && self.query.trim().is_empty() {
                    self.path_results = None;
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
                    self.query_duration = None;
                }
                if ui.button("Expand all").clicked() {
                    self.expanded.fill(true);
                    self.rebuild_rows();
                }
                if ui.button("Collapse all").clicked() {
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
                    if self.pending.is_some() {
                        ui.label("Opening...");
                    } else if let Some(duration) = self.open_duration {
                        ui.label(format!("Open: {}", format_duration(duration))).on_hover_text("Includes file read (when applicable), JSON parse, node index and initial view preparation; excludes file picker and painting.");
                    }
                    ui.separator();
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if let Some(doc) = &self.doc {
                    ui.label(format!(
                        "{} nodes  |  {:.1} KB",
                        doc.nodes.len(),
                        doc.bytes as f64 / 1024.0
                    ));
                    ui.separator();
                }
                ui.add(egui::Label::new(&self.notice).truncate());
                ui.separator();
                ui.label("F1 Help");
                    });
                });
            });
        });
        if self.doc.is_some() {
            egui::SidePanel::right("inspector")
                .default_width(300.0)
                .width_range(200.0..=600.0)
                .resizable(true)
                .show(ctx, |ui| {
                    ui.add_space(8.0);
                    if let Some(results) = &self.path_results {
                        ui.heading("JSONPath results");
                        ui.label(RichText::new(&results.expression).monospace());
                        ui.label(format!("{} matches", results.ids.len()));
                        let mut copy = false;
                        let mut export = false;
                        let mut reset = false;
                        ui.horizontal_wrapped(|ui| {
                            ui.add_enabled_ui(results.error.is_none(), |ui| {
                                copy = ui.button("Copy results").clicked();
                                export = ui.button("Export results").clicked();
                            });
                            reset = ui.button("Selection preview").clicked();
                        });
                        ui.separator();
                        if let Some(error) = &results.error {
                            ui.colored_label(Color32::LIGHT_RED, error);
                        } else {
                            egui::ScrollArea::both()
                                .id_salt("query_results")
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::TextEdit::multiline(&mut results.preview.as_str())
                                            .font(egui::TextStyle::Monospace)
                                            .desired_width(f32::INFINITY),
                                    );
                                });
                        }
                        if copy || export {
                            let json = self.doc.as_ref().unwrap().results_json(&results.ids);
                            if copy {
                                ctx.copy_text(json);
                                self.notice = "JSONPath results copied".into();
                            } else {
                                self.export_json(json);
                            }
                        }
                        if reset {
                            self.path_results = None;
                            self.query.clear();
                            self.query_duration = None;
                        }
                        return;
                    }
                    ui.heading("Selection");
                    let node = &self.doc.as_ref().unwrap().nodes[self.selected];
                    ui.label(RichText::new(node.kind).color(kind_color(node.kind)));
                    ui.label(RichText::new(&node.path).monospace());
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
                    egui::ScrollArea::both().id_salt("detail").show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.detail.as_str())
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
        }
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
                    if ui.button("Try sample document").clicked() {
                        self.load_text(DEMO.into(), "sample.json".into(), ctx);
                    }
                });
                return;
            }
            ui.label(RichText::new(&self.source).strong());
            ui.separator();
            let row_height = 25.0;
            let row_stride = row_height + ui.spacing().item_spacing.y;
            let mut area = egui::ScrollArea::both()
                .id_salt("tree")
                .auto_shrink([false, false]);
            if self.scroll_to
                && let Some(pos) = self.rows.iter().position(|&id| id == self.selected)
            {
                area = area.vertical_scroll_offset(
                    (pos as f32 * row_stride - ui.available_height() / 2.0).max(0.0),
                );
                self.scroll_to = false;
            }
            let mut select = None;
            let mut toggle = None;
            area.show_rows(ui, row_height, self.rows.len(), |ui, range| {
                for row in range {
                    let id = self.rows[row];
                    let node = &self.doc.as_ref().unwrap().nodes[id];
                    let is_match = self.matches.binary_search(&id).is_ok();
                    let fill = if id == self.selected {
                        if self.dark {
                            Color32::from_rgb(30, 65, 95)
                        } else {
                            Color32::from_rgb(205, 230, 250)
                        }
                    } else if row % 2 == 0 {
                        ui.visuals().faint_bg_color
                    } else {
                        Color32::TRANSPARENT
                    };
                    egui::Frame::new().fill(fill).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.set_height(row_height);
                            ui.add_space(node.depth as f32 * 18.0);
                            if node.container {
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
                            let key = RichText::new(&node.key).monospace().color(if is_match {
                                Color32::from_rgb(220, 160, 50)
                            } else {
                                ui.visuals().text_color()
                            });
                            let response =
                                ui.add(egui::Label::new(key).sense(egui::Sense::click()));
                            if response.clicked() {
                                select = Some(id);
                            }
                            if response.double_clicked() && node.container {
                                toggle = Some(id);
                            }
                            ui.label(":");
                            let response = ui.add(
                                egui::Label::new(
                                    RichText::new(&node.preview)
                                        .monospace()
                                        .color(kind_color(node.kind)),
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
                self.load_text(self.paste.clone(), "Pasted JSON".into(), ctx);
                self.paste_open = false;
            }
        }
        egui::Window::new("Keyboard & paths").open(&mut self.help).show(ctx, |ui| {
            ui.label("Ctrl+O  Open file\nCtrl+V  Open clipboard (outside text fields)\nCtrl+F or /  Search\nF3 / Shift+F3 or n / N  Next / previous match\nArrows or h j k l  Navigate and expand/collapse\nSpace  Toggle node\nHome / End  First / last visible node\nCtrl+C  Copy selected JSON\nCtrl+P  Copy selected path\nF5  Reload source file\nEscape  Leave search field");
            ui.separator();
            ui.label("JSONPath: $.users[*].name, $..email, $.users[0:5], $.users[?(@.active == true)].name\nAlso supports quoted keys, negative indices and JSON Pointer. Query runs on Go / Enter; Reset restores selection preview.\ngg / G: first / last visible node. [ / ]: navigation history back / forward.");
        });
    }
}
fn kind_color(kind: &str) -> Color32 {
    match kind {
        "string" => Color32::from_rgb(85, 170, 120),
        "number" => Color32::from_rgb(80, 155, 225),
        "boolean" => Color32::from_rgb(175, 120, 220),
        "null" => Color32::from_rgb(170, 145, 115),
        _ => Color32::from_rgb(135, 145, 155),
    }
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
    fn loaded() -> Viewer {
        let mut app = Viewer::empty();
        let doc = Document::parse(DEMO).unwrap();
        app.expanded = vec![false; doc.nodes.len()];
        app.doc = Some(doc);
        app.select(0);
        app.rebuild_rows();
        app
    }
    #[test]
    fn search_reveals_match_and_collapsing_preserves_visible_selection() {
        let mut app = loaded();
        app.term = "email".into();
        app.scope = SearchScope::Keys;
        app.search();
        assert_eq!(app.matches.len(), 2);
        let first = app.selected;
        assert!(app.rows.contains(&first));
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
        assert_eq!(app.doc.as_ref().unwrap().at(app.selected), "张三");
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
        assert_eq!(app.detail, "\"Rust JSON Viewer\"");
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
        assert_eq!(app.selected, *app.rows.last().unwrap());
        frame(&mut app, "[");
        assert_eq!(app.selected, 0);
        frame(&mut app, "]");
        assert_eq!(app.selected, *app.rows.last().unwrap());
        let selected = app.selected;
        ctx.memory_mut(|m| m.request_focus(egui::Id::new("query_field")));
        frame(&mut app, "g");
        frame(&mut app, "g");
        assert_eq!(app.selected, selected);
    }
}
