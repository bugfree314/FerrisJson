use crate::{
    engine::{
        budget::{BudgetManager, Limits, Reservation, Resource},
        session::SessionStatus,
        tree::{Location, TreeFrame},
        tree_navigation::Navigation,
    },
    palette::{ColorSettings, Palette, Preset},
    tree_worker::{Backend, Task},
    vim::{Action, Vim},
};
use eframe::egui::{self, Key, RichText};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
struct LayoutCache {
    highlight: Option<crate::ui::SearchHighlight>,
    key: Option<(usize, Palette, u32, bool, u32)>,
    galley: Option<Arc<egui::Galley>>,
    _lease: Option<Reservation>,
    builds: u64,
    content: Option<Arc<crate::engine::tree::SelectionPreview>>,
}
impl LayoutCache {
    fn new() -> Self {
        Self {
            highlight: None,
            key: None,
            galley: None,
            _lease: None,
            builds: 0,
            content: None,
        }
    }
    fn set_highlight(&mut self, highlight: Option<&crate::ui::SearchHighlight>) {
        let same = match (&self.highlight, highlight) {
            (None, None) => true,
            (Some(old), Some(new)) => old.same_query(new),
            _ => false,
        };
        if !same {
            self.highlight = highlight.cloned();
            self.key = None;
        }
    }
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        frame: &TreeFrame,
        palette: Palette,
        wrap: bool,
        budget: &BudgetManager,
    ) {
        let width = ui.available_width().max(1.0);
        let key = (
            Arc::as_ptr(&frame.selection) as usize,
            palette,
            width.to_bits(),
            wrap,
            ui.ctx().pixels_per_point().to_bits(),
        );
        if self.key.as_ref() != Some(&key) {
            self.galley = None;
            self._lease = None;
            self.key = None;
            self.content = None;
            match budget.reserve(
                Resource::Memory,
                8 * 1024 * 1024,
                "bounded selected preview layout cache",
            ) {
                Ok(lease) => {
                    let text = &frame.selection.preview;
                    let mut n = text.len().min(16 * 1024);
                    while !text.is_char_boundary(n) {
                        n -= 1;
                    }
                    let mut job = crate::ui::json_job_highlighted(&text[..n], palette, self.highlight.as_ref());
                    job.wrap.max_width = if wrap { width } else { f32::INFINITY };
                    job.wrap.break_anywhere = wrap;
                    self.galley = Some(ui.fonts_mut(|fonts| fonts.layout_job(job)));
                    self.key = Some(key);
                    self.content = Some(frame.selection.clone());
                    self._lease = Some(lease);
                    self.builds += 1;
                }
                Err(e) => {
                    ui.label(e);
                    return;
                }
            }
        }
        let scroll = if wrap {
            egui::ScrollArea::vertical()
        } else {
            egui::ScrollArea::both()
        };
        scroll
            .id_salt("paged_preview")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(g) = &self.galley {
                    ui.add(egui::Label::new(g.clone()).selectable(true));
                }
            });
        if frame.selection.preview.len() > 16 * 1024 {
            ui.label("Display limited to 16 KiB; complete copy and export use the full value.");
        }
    }
}
pub struct Viewer {
    retiring: Option<std::thread::JoinHandle<()>>,
    pending_open: Option<PathBuf>,
    task: Option<Task>,
    backend: Backend,
    path: PathBuf,
    frame: Option<TreeFrame>,
    status: SessionStatus,
    query: String,
    row: String,
    notice: String,
    colors: ColorSettings,
    help: bool,
    vim: Vim,
    focus_path: bool,
    wanted: u64,
    wheel: f64,
    viewport_rows: u64,
    layout: LayoutCache,
    layout_budget: BudgetManager,
    frame_samples: Vec<f64>,
    sample_lease: Option<Reservation>,
    done: u64,
    total_bytes: u64,
    kind: Option<&'static str>,
    frame_epoch: u64,
    search_term: String,
    search_case: bool,
    search_regex: bool,
    search_scope: crate::document::SearchScope,
    search_reverse: bool,
    focus_search: bool,
    search_due: Option<Instant>,
    search_highlight: Option<crate::ui::SearchHighlight>,
    search: Option<crate::search_worker::Snapshot>,
    search_page: Option<crate::tree_worker::SearchPage>,
    search_start: u64,
    page_request: Option<(u64, u64, u64)>,
    jsonpath: String,
    query_snapshot: Option<crate::paged_query_worker::Snapshot>,
    query_page: Option<crate::tree_worker::ResultPage>,
    query_page_error: Option<String>,
    query_start: u64,
    query_page_request: Option<(u64, u64, u64)>,
    query_selected: Option<u64>,
}
impl Viewer {
    pub fn new(cc: &eframe::CreationContext<'_>, path: PathBuf, backend: Backend) -> Self {
        cc.egui_ctx.set_fonts(crate::fonts::definitions());
        let mut viewer = Self::empty(path.clone(), backend);
        viewer.colors = ColorSettings::load().unwrap_or_default();
        viewer
            .colors
            .active()
            .apply(&cc.egui_ctx, viewer.colors.dark);
        viewer.open(path, &cc.egui_ctx);
        viewer
    }
    fn empty(path: PathBuf, backend: Backend) -> Self {
        Self {
            retiring: None,
            pending_open: None,
            task: None,
            backend,
            path,
            frame: None,
            status: SessionStatus::Preparing,
            query: String::new(),
            row: "1".into(),
            notice: String::new(),
            colors: ColorSettings::default(),
            help: false,
            vim: Vim::default(),
            focus_path: false,
            wanted: 0,
            wheel: 0.0,
            viewport_rows: 24,
            layout: LayoutCache::new(),
            layout_budget: BudgetManager::new(Limits {
                memory: 256 * 1024 * 1024,
                disk: 0,
            }),
            frame_samples: Vec::new(),
            sample_lease: None,
            done: 0,
            total_bytes: 0,
            kind: None,
            frame_epoch: 0,
            search_term: String::new(),
            search_case: false,
            search_regex: false,
            search_scope: crate::document::SearchScope::All,
            search_reverse: false,
            focus_search: false,
            search_due: None,
            search_highlight: None,
            search: None,
            search_page: None,
            search_start: 0,
            page_request: None,
            jsonpath: String::new(),
            query_snapshot: None,
            query_page: None,
            query_page_error: None,
            query_start: 0,
            query_page_request: None,
            query_selected: None,
        }
    }
    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        if let Some(previous) = self.task.take() {
            self.retiring = Some(previous.retire());
        }
        self.frame = None;
        self.search = None;
        self.query_snapshot = None;
        self.query_page = None;
        self.query_page_error = None;
        self.query_start = 0;
        self.query_page_request = None;
        self.query_selected = None;
        self.search_page = None;
        self.search_due = None;
        self.search_highlight = None;
        self.search_start = 0;
        self.page_request = None;
        self.status = SessionStatus::Preparing;
        self.path = path.clone();
        self.notice.clear();
        self.wanted = 0;
        self.wheel = 0.0;
        self.vim.cancel();
        self.layout = LayoutCache::new();
        self.sample_lease = None;
        self.frame_samples = Vec::new();
        self.pending_open = Some(path);
        if self.retiring.is_none() {
            self.start_pending(ctx);
        }
    }
    fn start_pending(&mut self, ctx: &egui::Context) {
        let Some(path) = self.pending_open.take() else {
            return;
        };
        self.task = match Task::new(path, self.backend.clone(), ctx.clone()) {
            Ok(t) => {
                self.layout_budget = t.budget();
                self.sample_lease = None;
                self.frame_samples = Vec::new();
                Some(t)
            }
            Err(e) => {
                self.status = SessionStatus::Failed;
                self.notice = e;
                None
            }
        };
    }
    fn command(&mut self, command: Navigation) {
        self.page_request = None;
        self.query_page_request = None;
        if let Some(task) = &self.task
            && let Err(e) = task.command(command)
        {
            self.notice = e;
        }
    }
    fn output(&mut self, path: Option<PathBuf>, copy_path: bool) {
        if let (Some(task), Some(frame)) = (&self.task, &self.frame) {
            if !frame.exact || self.status != SessionStatus::Ready {
                self.notice = "Pending: complete validation required".into();
                return;
            }
            if let Err(e) = task.output(frame.selected.clone(), path, copy_path) {
                self.notice = e;
            }
        }
    }
    fn start_search(&mut self) {
        self.search_highlight = crate::ui::SearchHighlight::new(
            &self.search_term, self.search_case, self.search_regex, self.search_scope,
        );
        self.search_due = None;
        self.search_page = None;
        self.search_start = 0;
        self.page_request = None;
        if let Some(task) = &self.task {
            if self.search_term.is_empty() {
                task.search_clear(false);
                return;
            }
            let q = crate::engine::search::Query {
                term: self.search_term.clone(),
                case: self.search_case,
                regex: self.search_regex,
                scope: self.search_scope,
            };
            if let Err(e) = task.search_query(q, self.search_reverse) {
                self.search_highlight = None;
                self.notice = e;
            }
        }
    }
    fn start_query(&mut self) {
        self.query_page = None; self.query_page_error = None; self.query_page_request = None; self.query_start = 0; self.query_selected = None;
        if let Some(task) = &self.task {
            if self.jsonpath.trim().is_empty() { task.query_clear(true); self.query_snapshot = None; return; }
            if let Err(error) = task.query_request(self.jsonpath.clone()) { self.notice = error; }
            self.query_snapshot = Some(task.query.poll());
        }
    }
    fn query_results(&mut self, ui: &mut egui::Ui) {
        use crate::engine::results::Status;
        let Some(snapshot) = &self.query_snapshot else { return; };
        if snapshot.expression.is_empty() { return; }
        ui.heading("JSONPath results");
        ui.add(egui::Label::new(RichText::new(&snapshot.expression).monospace()).truncate()).on_hover_text(&snapshot.expression);
        let state = snapshot.state.as_ref();
        let count = state.map_or(0, |s| s.count);
        let complete = state.is_some_and(|s| s.status == Status::Completed);
        if snapshot.waiting { ui.spinner(); }
        if let Some(state) = state {
            ui.label(format!("{:?} · {}{} matches", state.status, if complete { "" } else { "≥" }, count));
            if !state.error.is_empty() { ui.colored_label(egui::Color32::LIGHT_RED, &state.error); }
        } else if snapshot.waiting { ui.label("Waiting for complete source validation / preparing query…"); }
        if !snapshot.error.is_empty() { ui.colored_label(egui::Color32::LIGHT_RED, &snapshot.error); }
        if let Some(elapsed) = snapshot.elapsed { ui.label(format!("{:.3} s", elapsed.as_secs_f64())); }
        let (mut clear, mut cancel, mut target, mut selected, mut output) = (false, false, None, None, None);
        ui.horizontal_wrapped(|ui| {
            if snapshot.waiting || state.is_some_and(|s| s.status == Status::Scanning) { cancel = ui.button("Cancel query").clicked(); }
            clear = ui.button("Clear query").clicked();
            if ui.add_enabled(self.query_start > 0, egui::Button::new("Previous page")).clicked() { target = Some(self.query_start.saturating_sub(16)); }
            if ui.add_enabled(self.query_start.saturating_add(16) < count, egui::Button::new("Next page")).clicked() { target = Some(self.query_start + 16); }
        });
        if let Some(page) = &self.query_page {
            ui.label(format!("{}–{} / {}{}", if page.rows.is_empty() { 0 } else { page.start + 1 }, page.start + page.rows.len() as u64, if complete { "" } else { "≥" }, count));
            egui::ScrollArea::vertical().id_salt("paged_query_values").max_height(200.0).show(ui, |ui| {
                for row in &page.rows {
                    if ui.selectable_label(self.query_selected == Some(row.index), format!("{} {}", row.index + 1, row.path)).clicked() { selected = Some(row.index); }
                    ui.add(egui::Label::new(crate::ui::json_job(&row.preview, self.colors.active())).truncate());
                }
            });
        } else if count > 0 && self.query_page_error.is_none() { ui.label("Loading result page…"); }
        if let Some(error) = &self.query_page_error { ui.colored_label(egui::Color32::LIGHT_RED, error); }
        if self.query_page_error.is_some() && ui.button("Retry page").clicked() { self.query_page_error = None; self.query_page_request = None; }
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(complete, egui::Button::new("Copy all")).clicked() { output = Some((false, false)); }
            if ui.add_enabled(complete, egui::Button::new("Export all")).clicked() { output = Some((true, false)); }
            if !complete {
                if ui.add_enabled(count > 0, egui::Button::new("Copy partial")).clicked() { output = Some((false, true)); }
                if ui.add_enabled(count > 0, egui::Button::new("Export partial")).clicked() { output = Some((true, true)); }
            }
        });
        ui.small("Copy is limited to 8 MiB; use Export for larger results.");
        if let Some(start) = target { self.query_start = start; self.query_page = None; self.query_page_error = None; self.query_page_request = None; }
        if let Some(task) = &self.task {
            if let Some(index) = selected {
                match task.query_select(index) { Ok(()) => self.query_selected = Some(index), Err(error) => self.notice = error }
            }
            if cancel { task.query_clear(false); self.query_page_request = None; }
            if clear { task.query_clear(true); self.jsonpath.clear(); self.query_snapshot = None; self.query_page = None; self.query_page_error = None; self.query_page_request = None; }
            if let Some((export, partial)) = output {
                let path = if export { rfd::FileDialog::new().add_filter("JSON", &["json"]).set_file_name(if partial { "query-partial.json" } else { "query-results.json" }).save_file() } else { None };
                if (!export || path.is_some()) && let Err(error) = task.query_output(path, partial.then_some(count)) { self.notice = error; }
            }
        }
    }
    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(Key::F1)) {
            self.help = !self.help;
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.vim.cancel();
            self.command(Navigation::CancelPending);
            self.help = false;
            ctx.memory_mut(|m| {
                m.surrender_focus(egui::Id::new("paged_path"));
                m.surrender_focus(egui::Id::new("paged_search"));
                m.surrender_focus(egui::Id::new("paged_jsonpath"));
            });
        }
        if self.help {
            return;
        }
        if ctx.input(|i| i.key_pressed(Key::F5)) {
            self.open(self.path.clone(), ctx);
            return;
        }
        if ctx.wants_keyboard_input() {
            return;
        }
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
            for c in text.chars() {
                if c == '.' {
                    self.focus_path = true;
                    ctx.input_mut(|i| {
                        i.events
                            .retain(|e| !matches!(e,egui::Event::Text(t) if t=="."))
                    });
                    continue;
                }
                if let Some(command) = self.vim.feed(c) {
                    let count = command.count as u64;
                    let navigation = match command.action {
                        Action::Move(d) => Some(Navigation::Move(d as i64 * count as i64)),
                        Action::Left => Some(Navigation::Left(count)),
                        Action::Right => Some(Navigation::Right(count)),
                        Action::First if command.explicit_count => {
                            Some(Navigation::Row(count.saturating_sub(1)))
                        }
                        Action::Last if command.explicit_count => {
                            Some(Navigation::Row(count.saturating_sub(1)))
                        }
                        Action::First => Some(Navigation::First),
                        Action::Last => Some(Navigation::Last),
                        Action::Toggle => Some(Navigation::Toggle),
                        Action::Open => Some(Navigation::Open),
                        Action::Close => Some(Navigation::Close),
                        Action::ExpandAll => Some(Navigation::ExpandAll),
                        Action::CollapseAll => Some(Navigation::CollapseAll),
                        Action::History(back) => {
                            for _ in 0..count {
                                self.command(Navigation::History(back));
                            }
                            None
                        }
                        Action::Screen(which) => Some(Navigation::Screen(which)),
                        Action::Copy(path) => {
                            self.output(None, path);
                            None
                        }
                        Action::Search(reverse) => {
                            self.search_reverse = reverse;
                            self.focus_search = true;
                            ctx.input_mut(|i| {
                                i.events.retain(
                                    |e| !matches!(e, egui::Event::Text(t) if t == "/" || t == "?"),
                                )
                            });
                            None
                        }
                        Action::Match(reverse) => {
                            if let Some(task) = &self.task {
                                task.search_match(!(reverse ^ self.search_reverse), count);
                            }
                            None
                        }
                    };
                    if let Some(n) = navigation {
                        self.command(n);
                    }
                }
            }
        }
        let keys = ctx.input(|i| {
            [
                Key::ArrowDown,
                Key::ArrowUp,
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::PageDown,
                Key::PageUp,
                Key::Space,
            ]
            .into_iter()
            .filter(|k| i.key_pressed(*k))
            .collect::<Vec<_>>()
        });
        for key in keys {
            let count = self.vim.take_count("navigation") as u64;
            let command = match key {
                Key::ArrowDown => Navigation::Move(count as i64),
                Key::ArrowUp => Navigation::Move(-(count as i64)),
                Key::ArrowLeft => Navigation::Left(count),
                Key::ArrowRight => Navigation::Right(count),
                Key::PageDown => Navigation::Move(self.viewport_rows as i64 * count as i64),
                Key::PageUp => Navigation::Move(-(self.viewport_rows as i64 * count as i64)),
                _ => Navigation::Toggle,
            };
            self.command(command);
        }
        let shortcuts = ctx.input(|i| {
            (
                i.modifiers.ctrl,
                i.key_pressed(Key::D),
                i.key_pressed(Key::U),
                i.key_pressed(Key::E),
                i.key_pressed(Key::B),
                i.key_pressed(Key::C),
                i.key_pressed(Key::P),
                i.key_pressed(Key::F),
            )
        });
        if shortcuts.0
            && (shortcuts.1
                || shortcuts.2
                || shortcuts.3
                || shortcuts.4
                || shortcuts.5
                || shortcuts.6
                || shortcuts.7)
        {
            let count = self.vim.take_count("shortcut") as i64;
            if shortcuts.1 {
                self.command(Navigation::Move(
                    (self.viewport_rows / 2).max(1) as i64 * count,
                ));
            }
            if shortcuts.2 {
                self.command(Navigation::Move(
                    -((self.viewport_rows / 2).max(1) as i64) * count,
                ));
            }
            if shortcuts.3 {
                self.command(Navigation::Move(self.viewport_rows as i64 * count));
            }
            if shortcuts.4 {
                self.command(Navigation::Move(-(self.viewport_rows as i64) * count));
            }
            if shortcuts.5 {
                self.output(None, false);
            }
            if shortcuts.6 {
                self.output(None, true);
            }
            if shortcuts.7 {
                self.focus_search = true;
            }
        }
    }
    fn draw(&mut self, ctx: &egui::Context) {
        let started = Instant::now();
        if self
            .retiring
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            let _ = self.retiring.take().unwrap().join();
            self.start_pending(ctx);
        }
        if let Some(task) = &self.task {
            let poll = task.poll();
            self.query_snapshot = Some(task.query.poll());
            if let Some(page) = poll.query_page
                && page.generation == task.query.generation() && page.start == self.query_start
                && self.query_page_request.is_none_or(|(_, _, count)| page.count >= count)
            { self.query_page = Some(page); self.query_page_error = None; }
            if let Some((generation, start, error)) = poll.query_page_error
                && generation == task.query.generation() && start == self.query_start
            { self.query_page_error = Some(error); }
            if let Some(snapshot) = &self.query_snapshot && let Some(state) = &snapshot.state {
                let key = (snapshot.generation, self.query_start, state.count.min(self.query_start.saturating_add(16)));
                if state.count > self.query_start && self.query_page_request != Some(key) {
                    match task.query_page(self.query_start) {
                        Ok(()) => self.query_page_request = Some(key),
                        Err(error) => self.notice = error,
                    }
                }
            }
            if let Some((generation, clip)) = poll.query_clipboard && generation == task.query.generation() { ctx.copy_text(clip.text); }
            let previous_cursor = self.search.as_ref().and_then(|s| s.cursor);
            self.search = Some(task.search.poll());
            if let Some(cursor) = self.search.as_ref().and_then(|s| s.cursor)
                && previous_cursor != Some(cursor)
                && (cursor < self.search_start || cursor >= self.search_start.saturating_add(16))
            {
                self.search_start = cursor / 16 * 16;
                self.search_page = None;
                self.page_request = None;
            }
            if let Some(page) = poll.search_page
                && page.generation == task.search.generation()
                && page.start == self.search_start
                && self
                    .page_request
                    .is_none_or(|(_, _, count)| page.count >= count)
            {
                self.search_page = Some(page);
            }
            if let Some(snapshot) = &self.search {
                if !snapshot.error.is_empty() {
                    self.notice = snapshot.error.clone();
                }
                if let Some(state) = &snapshot.state {
                    let key = (
                        snapshot.generation,
                        self.search_start,
                        state.count.min(self.search_start.saturating_add(16)),
                    );
                    if state.count > self.search_start && self.page_request != Some(key) {
                        task.search_page(self.search_start);
                        self.page_request = Some(key);
                    }
                }
            }
            self.status = poll.status.state;
            self.done = poll.status.done;
            self.total_bytes = poll.status.total;
            self.kind = poll.status.kind;
            if let Some((epoch, result)) = poll.frame
                && epoch == task.epoch.load(std::sync::atomic::Ordering::Relaxed)
            {
                match result {
                    Ok(frame) => {
                        if poll
                            .status
                            .session
                            .is_none_or(|id| id == frame.selected.node.reference.session)
                        {
                            self.wanted = frame.start;
                            self.frame_epoch = epoch;
                            self.frame = Some(frame);
                        }
                    }
                    Err(e) => self.notice = e,
                }
            }
            if !poll.status.message.is_empty() {
                self.notice = poll.status.message;
            }
            if !poll.notice.is_empty() {
                self.notice = poll.notice;
            }
            if let Some(clip) = poll.clipboard {
                ctx.copy_text(clip.text);
            }
            if let Some((text, _lease)) = poll.path_copy {
                ctx.copy_text(text);
            }
            if matches!(
                self.status,
                SessionStatus::Failed | SessionStatus::Cancelled
            ) {
                self.frame = None;
                self.layout = LayoutCache::new();
            }
        }
        if self.search_due.is_some_and(|due| Instant::now() >= due) {
            self.start_search();
        }
        if self.search_due.is_some()
            || self.query_snapshot.as_ref().is_some_and(|s| s.waiting)
            || self.search.as_ref().is_some_and(|s| {
                s.waiting
                    || s.state
                        .as_ref()
                        .is_some_and(|st| st.status == crate::engine::search::Status::Scanning)
            })
        {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        self.keyboard(ctx);
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()))
        {
            self.open(path, ctx);
        }
        egui::TopBottomPanel::top("paged_toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("FerrisJson · Tree");
                if ui.button("Open file").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("JSON", &["json"])
                        .pick_file()
                {
                    self.open(path, ctx);
                }
                if ui.button("Reload / Resume").clicked() {
                    self.open(self.path.clone(), ctx);
                }
                if ui.button("Cancel").clicked()
                    && let Some(task) = &self.task
                {
                    task.cancel();
                    self.status = SessionStatus::Cancelled;
                    self.frame = None;
                }
                if ui.checkbox(&mut self.colors.dark, "Dark").changed() {
                    self.colors.active().apply(ctx, self.colors.dark);
                }
                egui::ComboBox::from_id_salt("paged_colors")
                    .selected_text(self.colors.preset.name())
                    .show_ui(ui, |ui| {
                        for preset in Preset::ALL {
                            if ui
                                .selectable_value(&mut self.colors.preset, preset, preset.name())
                                .changed()
                            {
                                self.colors.active().apply(ctx, self.colors.dark);
                            }
                        }
                    });
                if ui.button("Help").clicked() {
                    self.help = !self.help;
                }
            });
            ui.add(egui::Label::new(self.path.display().to_string()).truncate());
            if self.status != SessionStatus::Ready {
                ui.label(format!(
                    "{:?} · {}: {} / {} bytes",
                    self.status,
                    self.kind.unwrap_or("root Pending"),
                    self.done,
                    self.total_bytes
                ));
            }
            ui.horizontal(|ui| {
                ui.label("Path");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .id(egui::Id::new("paged_path"))
                        .char_limit(4096),
                );
                if self.focus_path {
                    response.request_focus();
                    self.focus_path = false;
                }
                if (response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)))
                    || ui.button("Go").clicked()
                {
                    self.command(Navigation::Locate(self.query.clone()));
                }
                ui.label("Row");
                ui.add(
                    egui::TextEdit::singleline(&mut self.row)
                        .char_limit(20)
                        .desired_width(100.0),
                );
                if ui.button("Jump").clicked() {
                    match self.row.parse::<u64>() {
                        Ok(n) if n > 0 => self.command(Navigation::Row(n - 1)),
                        _ => self.notice = "Enter a positive integer row number".into(),
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("JSONPath");
                let response = ui.add(egui::TextEdit::singleline(&mut self.jsonpath)
                    .id(egui::Id::new("paged_jsonpath")).hint_text("$.items[?@.active]").desired_width(360.0));
                if (response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) || ui.button("Run query").clicked() { self.start_query(); }
                if ui.button("Clear query").clicked() {
                    self.jsonpath.clear(); self.start_query();
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(if self.search_reverse {
                    "Search ?"
                } else {
                    "Search /"
                });
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.search_term)
                        .id(egui::Id::new("paged_search"))
                        .char_limit(16 * 1024)
                        .desired_width(220.0),
                );
                if self.focus_search {
                    response.request_focus();
                    self.focus_search = false;
                }
                let mut changed = response.changed();
                changed |= ui.checkbox(&mut self.search_case, "Case").changed();
                changed |= ui.checkbox(&mut self.search_regex, "Regex").changed();
                egui::ComboBox::from_id_salt("paged_search_scope")
                    .selected_text(format!("{:?}", self.search_scope))
                    .show_ui(ui, |ui| {
                        for scope in [
                            crate::document::SearchScope::All,
                            crate::document::SearchScope::Keys,
                            crate::document::SearchScope::Values,
                        ] {
                            changed |= ui
                                .selectable_value(
                                    &mut self.search_scope,
                                    scope,
                                    format!("{scope:?}"),
                                )
                                .changed();
                        }
                    });
                if ui.checkbox(&mut self.colors.live_search, "Live").changed() {
                    changed = true;
                }
                if changed {
                    self.search_highlight = None;
                    if let Some(task) = &self.task {
                        task.search_clear(false);
                    }
                    self.search_page = None;
                    self.page_request = None;
                    self.search_due = if self.colors.live_search && !self.search_term.is_empty() {
                        Some(Instant::now() + Duration::from_millis(200))
                    } else {
                        None
                    };
                }
                if (response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)))
                    || ui.button("Search").clicked()
                {
                    self.start_search();
                }
                if ui.button("Clear").clicked() {
                    self.search_term.clear();
                    self.start_search();
                }
                if ui.button("Stop search").clicked()
                    && let Some(task) = &self.task
                {
                    task.search_clear(true);
                    self.page_request = None;
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Expand all · zR").clicked() {
                    self.command(Navigation::ExpandAll);
                }
                if ui.button("Collapse all · zM").clicked() {
                    self.command(Navigation::CollapseAll);
                }
                if ui.button("First · gg").clicked() {
                    self.command(Navigation::First);
                }
                if ui.button("Last · G").clicked() {
                    self.command(Navigation::Last);
                }
            });
        });
        egui::TopBottomPanel::bottom("paged_status").show(ctx,|ui|{ui.horizontal_wrapped(|ui|{ui.label(format!("{:?}",self.status));if let Some(frame)=&self.frame{ui.monospace(format!("Ln {}/{}{} · {}",frame.selected_rank+1,if frame.exact{""}else{"≥"},frame.total,self.vim.display()));}ui.label(&self.notice);});if self.status==SessionStatus::Validating{ui.label("Browsing committed content; final counts and complete output are pending validation.");}});
        if self
            .search
            .as_ref()
            .is_some_and(|s| s.state.is_some() || s.waiting)
        {
            egui::SidePanel::left("paged_search_results")
                .default_width(260.0)
                .resizable(true)
                .show(ctx, |ui| {
                    ui.heading("Search results");
                    if let Some(snapshot) = &self.search {
                        if let Some(state) = &snapshot.state {
                            ui.label(format!(
                                "{:?} · {}{} matches · {} scanned · queue {}/4",
                                state.status,
                                if state.status == crate::engine::search::Status::Completed {
                                    ""
                                } else {
                                    "≥"
                                },
                                state.count,
                                state.scanned,
                                snapshot.queue_peak
                            ));
                            if !state.error.is_empty() {
                                ui.label(&state.error);
                            }
                            ui.horizontal(|ui| {
                                if ui.button("Prev").clicked() {
                                    self.search_start = self.search_start.saturating_sub(16);
                                    self.page_request = None;
                                    self.search_page = None;
                                }
                                if ui
                                    .add_enabled(
                                        self.search_start.saturating_add(16) < state.count,
                                        egui::Button::new("Next"),
                                    )
                                    .clicked()
                                {
                                    self.search_start += 16;
                                    self.page_request = None;
                                    self.search_page = None;
                                }
                            });
                        } else {
                            ui.label("Preparing search / waiting for source validation…");
                        }
                    }
                    if let Some(page) = &self.search_page {
                        ui.label(format!(
                            "From {} · {} committed",
                            page.start + 1,
                            page.count
                        ));
                        egui::ScrollArea::vertical()
                            .max_height((ui.available_height() - 110.0).max(40.0))
                            .show(ui, |ui| {
                                for row in &page.rows {
                                    if ui
                                        .selectable_label(
                                            self.search.as_ref().and_then(|s| s.cursor)
                                                == Some(row.index),
                                            if let Some(highlight) = &self.search_highlight {
                                                highlight.text_job(
                                                    &format!("{} {}", row.index + 1, row.path),
                                                    ui.visuals().text_color(), self.colors.active(), true,
                                                ).into()
                                            } else {
                                                egui::WidgetText::from(format!("{} {}", row.index + 1, row.path))
                                            },
                                        )
                                        .clicked()
                                        && let Some(task) = &self.task
                                    {
                                        task.search_select(row.index);
                                    }
                                    ui.add(egui::Label::new(crate::ui::json_job_highlighted(
                                        &row.preview, self.colors.active(), self.search_highlight.as_ref(),
                                    )).truncate());
                                }
                            });
                    }
                    for (label, paths, partial) in [
                        ("Export all values", false, false),
                        ("Export all paths", true, false),
                        ("Export partial values", false, true),
                        ("Export partial paths", true, true),
                    ] {
                        if ui.button(label).clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("JSON", &["json"])
                                .save_file()
                            && let Some(task) = &self.task
                            && let Err(e) = task.search_export(path, paths, partial)
                        {
                            self.notice = e;
                        }
                    }
                });
        }
        egui::SidePanel::right("paged_inspector")
            .resizable(true)
            .default_width(420.0)
            .show(ctx, |ui| {
                self.query_results(ui);
                ui.separator();
                let mut copy = false;
                let mut path = false;
                let mut export = false;
                ui.horizontal(|ui| {
                    ui.heading("Value preview");
                    ui.checkbox(&mut self.colors.wrap_values, "Wrap");
                });
                if let Some(frame) = &self.frame {
                    ui.add(
                        egui::Label::new(RichText::new(&frame.selection.path).monospace())
                            .truncate(),
                    )
                    .on_hover_text(&frame.selection.path);
                    ui.horizontal(|ui| {
                        let ready = self.status == SessionStatus::Ready && frame.exact;
                        copy = ui
                            .add_enabled(ready, egui::Button::new("Copy JSON"))
                            .clicked();
                        path = ui
                            .add_enabled(ready, egui::Button::new("Copy path"))
                            .clicked();
                        export = ui.add_enabled(ready, egui::Button::new("Export")).clicked();
                    });
                    self.layout.set_highlight(self.search_highlight.as_ref());
                    self.layout.show(
                        ui,
                        frame,
                        self.colors.active(),
                        self.colors.wrap_values,
                        &self.layout_budget,
                    );
                } else {
                    ui.label("Waiting for a committed page…");
                }
                if copy {
                    self.output(None, false);
                }
                if path {
                    self.output(None, true);
                }
                if export
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("JSON", &["json"])
                        .save_file()
                {
                    self.output(Some(path), false);
                }
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            let rect = ui.available_rect_before_wrap();
            let rows = (rect.height() / 24.0).floor().clamp(1.0, 64.0) as u64;
            if rows != self.viewport_rows {
                self.viewport_rows = rows;
                self.command(Navigation::ViewportRows(rows));
            }
            let mut select: Option<Location> = None;
            let mut toggle: Option<Location> = None;
            if let Some(frame) = &self.frame {
                for row in frame.rows.iter().take(rows as usize) {
                    ui.horizontal(|ui| {
                        ui.add_space((row.location.ancestors.len().min(32) * 12) as f32);
                        ui.monospace(format!("{}", row.rank + 1));
                        if row.location.node.container() {
                            if ui
                                .small_button(if row.expanded { "▾" } else { "▸" })
                                .clicked()
                            {
                                toggle = Some(row.location.clone());
                            }
                        } else {
                            ui.add_space(18.0);
                        }
                        let palette = self.colors.active();
                        let foreground = palette.kind(row.location.node.kind);
                        let mut text = if let Some(highlight) = &self.search_highlight {
                            highlight.text_job(&row.label, foreground, palette, true)
                        } else {
                            let mut job = egui::text::LayoutJob::default();
                            job.append(&row.label, 0.0, egui::TextFormat {
                                font_id: egui::FontId::monospace(13.0), color: foreground, ..Default::default()
                            });
                            job
                        };
                        text.append("  ", 0.0, egui::TextFormat::default());
                        let preview = crate::ui::json_job_highlighted(&row.preview, palette, self.search_highlight.as_ref());
                        let offset = text.text.len();
                        text.text.push_str(&preview.text);
                        text.sections.extend(preview.sections.into_iter().map(|mut section| {
                            section.byte_range = section.byte_range.start + offset..section.byte_range.end + offset;
                            section
                        }));
                        if ui
                            .add(
                                egui::Button::selectable(
                                    row.location.node.reference == frame.selected.node.reference,
                                    text,
                                )
                                .truncate(),
                            )
                            .clicked()
                        {
                            select = Some(row.location.clone());
                        }
                    });
                }
                ui.label(format!(
                    "Window {}..{}; {}{} visible rows",
                    frame.start + 1,
                    frame.start + frame.rows.len() as u64,
                    if frame.exact { "" } else { "at least " },
                    frame.total
                ));
            } else {
                ui.label("Preparing snapshot / waiting for a complete safe page…");
            }
            if let Some(location) = select {
                self.command(Navigation::Select(location));
            }
            if let Some(location) = toggle {
                self.command(Navigation::Select(location));
                self.command(Navigation::Toggle);
            }
            if !self.help
                && !ctx.wants_keyboard_input()
                && ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| rect.contains(p)))
            {
                self.wheel += ctx.input(|i| i.smooth_scroll_delta.y) as f64 / 24.0;
                let delta = self.wheel.trunc() as i64;
                self.wheel -= delta as f64;
                if delta != 0 {
                    self.wanted = if delta > 0 {
                        self.wanted.saturating_sub(delta as u64)
                    } else {
                        self.wanted.saturating_add(delta.unsigned_abs())
                    };
                    self.command(Navigation::Anchor(self.wanted));
                }
            }
        });
        if self.help {
            egui::Window::new("Paged tree shortcuts").open(&mut self.help).show(ctx, |ui| {
                for line in [
                    "hjkl / arrows: move, parent, expand or child; digits repeat",
                    "gg / G: first / last; number + gg or G: exact integer row",
                    "H M L: viewport top / middle / bottom",
                    "PageUp / PageDown: page; Ctrl+U / D: half page; Ctrl+B / E: page",
                    "za / zo / zc: toggle / open / close; zR / zM: expand / collapse all",
                    "[ / ]: history; yy / Ctrl+C: copy JSON; yp / Ctrl+P: copy path",
                    ".: focus Path (JSON Pointer, e.g. /items/0)",
                    "F5: reload; F1: help; Esc: cancel pending jump and key sequence",
                    "Search: / ? or Ctrl+F; n / N with counts. JSONPath: Run query / Enter; results have independent paging, cancel and output.",
                    "During validation, only committed content is browsable; complete output waits for Ready.",
                ] { ui.label(line); }
            });
        }
        if self.sample_lease.is_none() && self.retiring.is_none() {
            self.sample_lease = self
                .layout_budget
                .reserve(Resource::Memory, 8192 * 8, "bounded GUI frame samples")
                .ok();
            if self.sample_lease.is_some() {
                self.frame_samples = Vec::with_capacity(8192);
            }
        }
        if self.frame_samples.len() < self.frame_samples.capacity() {
            self.frame_samples
                .push(started.elapsed().as_secs_f64() * 1000.0);
        }
        ctx.request_repaint_after(Duration::from_millis(
            if matches!(
                self.status,
                SessionStatus::Preparing | SessionStatus::Validating
            ) {
                16
            } else {
                250
            },
        ));
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        if let Some(retiring) = self.retiring.take() {
            crate::shutdown::defer(retiring);
        }
    }
}
impl eframe::App for Viewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}

/// Development harness: measures CPU UI preparation, not native/GPU FPS.
#[allow(
    dead_code,
    reason = "Called by the tree_bench example which reuses this renderer"
)]
pub fn benchmark_search(path: PathBuf, expected: u64) -> Result<serde_json::Value, String> {
    let ctx = egui::Context::default();
    ctx.set_fonts(crate::fonts::definitions());
    let mut viewer = Viewer::empty(
        path.clone(),
        Backend::Sparse(crate::engine::sparse::OpenOptions::default()),
    );
    viewer.open(path.clone(), &ctx);
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1280.0, 900.0),
        )),
        ..Default::default()
    };
    let started = Instant::now();
    let deadline = started + Duration::from_secs(600);
    while viewer.status != SessionStatus::Ready || viewer.frame.as_ref().is_none_or(|f| !f.exact) {
        let output = ctx.run(input(), |ctx| viewer.draw(ctx));
        let _ = ctx.tessellate(output.shapes, output.pixels_per_point);
        if viewer.status == SessionStatus::Failed {
            return Err(viewer.notice.clone());
        }
        if Instant::now() > deadline {
            return Err("Search benchmark build timeout".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let ready_ms = started.elapsed().as_secs_f64() * 1000.0;
    let budget = viewer.task.as_ref().unwrap().budget();
    let mut reports = Vec::new();
    viewer.search_scope = crate::document::SearchScope::Values;
    viewer.search_case = true;
    for (term, matches) in [("NEEDLE", 3), ("x", expected)] {
        viewer.search_term = term.into();
        let start = Instant::now();
        viewer.start_search();
        let mut first = None;
        let mut frames = Vec::new();
        let mut ticks = 0;
        loop {
            let tick = Instant::now();
            let output = ctx.run(input(), |ctx| viewer.draw(ctx));
            let _ = ctx.tessellate(output.shapes, output.pixels_per_point);
            if frames.len() < 8192 {
                frames.push(tick.elapsed().as_secs_f64() * 1000.0);
            }
            ticks += 1;
            if ticks % 20 == 0 {
                viewer.command(Navigation::First);
            }
            if let Some(s) = &viewer.search
                && let Some(state) = &s.state
            {
                if state.count > 0 && first.is_none() {
                    first = Some(start.elapsed().as_secs_f64() * 1000.0);
                }
                if state.status != crate::engine::search::Status::Scanning {
                    if state.status != crate::engine::search::Status::Completed
                        || state.count != matches
                    {
                        return Err(format!(
                            "Unexpected search result {state:?}; expected {matches}"
                        ));
                    }
                    let store = viewer
                        .task
                        .as_ref()
                        .unwrap()
                        .search
                        .store()
                        .ok_or("Missing completed result store")?;
                    for hit in [0, matches / 2, matches - 1] {
                        let loc = store.location(hit, &|| false)?;
                        let expected_ordinal = if term == "NEEDLE" {
                            [0, expected / 2, expected - 1][hit as usize]
                        } else {
                            hit
                        };
                        if loc
                            .ancestors
                            .first()
                            .is_none_or(|e| e.ordinal != expected_ordinal)
                        {
                            return Err("First/middle/last result location mismatch".into());
                        }
                    }
                    frames.sort_by(f64::total_cmp);
                    let p95 = frames[(frames.len() - 1) * 95 / 100];
                    reports.push(serde_json::json!({"term":term,"matches":state.count,"scanned":state.scanned,"first_observed_ms":first,"complete_observed_ms":start.elapsed().as_secs_f64()*1000.0,"gui_cpu_p95_ms":p95,"gui_cpu_max_ms":frames.last(),"gui_frames":frames.len(),"queue_peak":s.queue_peak,"budget":budget.usage()}));
                    break;
                }
            }
            if Instant::now() > deadline {
                return Err("Search benchmark timeout".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    viewer.search_page = None;
    viewer.frame = None;
    viewer.layout = LayoutCache::new();
    viewer
        .task
        .take()
        .unwrap()
        .retire()
        .join()
        .map_err(|_| "Search task join failed")?;
    drop(viewer);
    let after = budget.usage();
    if after.disk != 0 {
        return Err(format!("Search files leaked: {after:?}"));
    }
    Ok(
        serde_json::json!({"file":path.display().to_string(),"bytes":std::fs::metadata(path).map_err(|e|e.to_string())?.len(),"ready_ms":ready_ms,"searches":reports,"after_retirement":after,"scope":"headless egui CPU+tessellation; no native window/GPU/RSS claim"}),
    )
}
#[allow(dead_code, reason = "Standalone CPU benchmark entry")]
pub fn benchmark_live(
    path: PathBuf,
    expected: u64,
    nested: bool,
) -> Result<serde_json::Value, String> {
    let ctx = egui::Context::default();
    ctx.set_fonts(crate::fonts::definitions());
    let mut viewer = Viewer::empty(
        path.clone(),
        Backend::Sparse(crate::engine::sparse::OpenOptions::default()),
    );
    viewer.open(path, &ctx);
    let budget = viewer
        .task
        .as_ref()
        .ok_or("Cannot start tree task")?
        .budget();
    let mut validating = Vec::new();
    let mut ready_times = Vec::new();
    let started = Instant::now();
    let mut first_page = None;
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1120.0, 760.0),
        )),
        ..Default::default()
    };
    while viewer.status != SessionStatus::Ready || viewer.frame.as_ref().is_none_or(|f| !f.exact) {
        let frame_started = Instant::now();
        let output = ctx.run(input(), |ctx| viewer.draw(ctx));
        let _ = ctx.tessellate(output.shapes, output.pixels_per_point);
        let elapsed = frame_started.elapsed().as_secs_f64() * 1000.0;
        if viewer.status == SessionStatus::Failed {
            return Err(viewer.notice.clone());
        }
        if viewer.status == SessionStatus::Validating && validating.len() < 8192 {
            validating.push(elapsed);
        }
        if first_page.is_none() && viewer.frame.as_ref().is_some_and(|f| f.rows.len() > 1) {
            first_page = Some(started.elapsed().as_secs_f64() * 1000.0);
        }
        if started.elapsed() > Duration::from_secs(1800) {
            return Err("Tree build deadline exceeded".into());
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    let ready_ms = started.elapsed().as_secs_f64() * 1000.0;
    if viewer.frame.as_ref().unwrap().total != expected + 1 {
        return Err("Unexpected initial row count".into());
    }
    let wait =
        |viewer: &mut Viewer, predicate: &dyn Fn(&TreeFrame) -> bool| -> Result<f64, String> {
            let start = Instant::now();
            loop {
                let _ = ctx.run(input(), |ctx| viewer.draw(ctx));
                if viewer.frame_epoch
                    == viewer
                        .task
                        .as_ref()
                        .unwrap()
                        .epoch
                        .load(std::sync::atomic::Ordering::Relaxed)
                    && viewer.frame.as_ref().is_some_and(predicate)
                {
                    return Ok(start.elapsed().as_secs_f64() * 1000.0);
                }
                if start.elapsed() > Duration::from_secs(60) {
                    return Err(format!("Navigation deadline exceeded: {}", viewer.notice));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        };
    let mut probes = Vec::new();
    for ordinal in [0, expected / 2, expected - 1] {
        let pointer = if nested {
            format!("/{ordinal}/id")
        } else {
            format!("/{ordinal}")
        };
        let path = if nested {
            format!("$[{ordinal}][\"id\"]")
        } else {
            format!("$[{ordinal}]")
        };
        viewer.command(Navigation::Locate(pointer));
        let elapsed = wait(&mut viewer, &|f| f.selection.path == path)?;
        let text = &viewer.frame.as_ref().unwrap().selection.preview;
        if nested {
            if text != &ordinal.to_string() {
                return Err("Nested id mismatch".into());
            }
        } else {
            let prefix = format!("\"{ordinal} ");
            if text.len() != 999
                || !text.starts_with(&prefix)
                || !text.ends_with('"')
                || !text.as_bytes()[prefix.len()..text.len() - 1]
                    .iter()
                    .all(|b| *b == b'x')
            {
                return Err("Scalar full-value probe mismatch".into());
            }
        }
        probes.push(serde_json::json!({"ordinal":ordinal,"locate_ms":elapsed}));
    }
    viewer.command(Navigation::ExpandAll);
    let full_rows = expected * (if nested { 8 } else { 1 }) + 1;
    let expand_ms = wait(&mut viewer, &|f| f.total == full_rows)?;
    viewer.command(Navigation::Last);
    let end_ms = wait(&mut viewer, &|f| f.selected_rank == full_rows - 1)?;
    let last_path = &viewer.frame.as_ref().unwrap().selection.path;
    let wanted = if nested {
        format!("$[{}][\"payload\"]", expected - 1)
    } else {
        format!("$[{}]", expected - 1)
    };
    if last_path != &wanted {
        return Err("Full expansion end path mismatch".into());
    }
    viewer.command(Navigation::CollapseAll);
    let collapse_ms = wait(&mut viewer, &|f| f.total == 1)?;
    viewer.command(Navigation::ExpandAll);
    wait(&mut viewer, &|f| f.total == full_rows)?;
    let mut interactions = Vec::with_capacity(120);
    for operation in 0..120 {
        let start = Instant::now();
        let command = match operation % 12 {
            0 => Navigation::CollapseAll,
            1 => Navigation::ExpandAll,
            2 => Navigation::Last,
            3 => Navigation::First,
            _ => Navigation::Anchor(
                ((operation as u64 * 7919) % (full_rows.max(1))).min(full_rows - 1),
            ),
        };
        viewer.command(command);
        wait(&mut viewer, &|_| true)?;
        interactions.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    for frame_number in 0..600 {
        if frame_number % 20 == 0 {
            let anchor = ((frame_number as u64 / 20) * (full_rows / 30)).min(full_rows - 1);
            viewer.command(Navigation::Anchor(anchor));
        }
        let before = Instant::now();
        let output = ctx.run(input(), |ctx| viewer.draw(ctx));
        let _ = ctx.tessellate(output.shapes, output.pixels_per_point);
        ready_times.push(before.elapsed().as_secs_f64() * 1000.0);
        if viewer.frame.as_ref().is_some_and(|f| f.rows.len() > 64) {
            return Err("UI window exceeded 64 rows".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    fn stats(mut values: Vec<f64>) -> serde_json::Value {
        values.sort_by(f64::total_cmp);
        if values.is_empty() {
            return serde_json::json!({"samples":0});
        }
        serde_json::json!({"samples":values.len(),"p50_ms":values[values.len()/2],"p95_ms":values[(values.len()-1)*95/100],"max_ms":values.last().unwrap()})
    }
    let result = serde_json::json!({"package_version":env!("CARGO_PKG_VERSION"),"first_page_ms":first_page,"ready_ms":ready_ms,"full_rows":full_rows,"expand_ms":expand_ms,"collapse_ms":collapse_ms,"last_ms":end_ms,"probes":probes,"validating_cpu_frames":stats(validating),"ready_cpu_frames":stats(ready_times),"interaction_latency":stats(interactions),"layout_builds":viewer.layout.builds,"managed":budget.usage(),"ui_window_max":64,"scope":"headless egui CPU, with actual background building; native/GPU FPS unverified"});
    drop(viewer);
    crate::shutdown::finish();
    if budget.usage().memory != 0 || budget.usage().disk != 0 {
        return Err("Tree task retained budget after close".into());
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{
        budget::Limits,
        tree::{LegacyProvider, PagedTree},
    };
    #[test]
    fn preview_layout_reuses_frames_and_invalidates_inputs() {
        let b = BudgetManager::new(Limits {
            memory: 256 * 1024 * 1024,
            disk: 1024 * 1024 * 1024,
        });
        let d =
            crate::document::Document::parse_owned_recover_with_budget("[1,2]".into(), b.clone())
                .map_err(|(e, _)| e)
                .unwrap();
        let mut tree = PagedTree::new(Box::new(LegacyProvider(Arc::new(d))), &|| false).unwrap();
        let root = tree.root(&|| false).unwrap();
        let mut f = tree.frame(0, 64, &root, &|| false).unwrap();
        let ctx = egui::Context::default();
        let mut cache = LayoutCache::new();
        let draw =
            |cache: &mut LayoutCache, f: &TreeFrame, palette: Palette, wrap: bool, width: f32| {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 400.0),
                    )),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| cache.show(ui, f, palette, wrap, &b));
                });
            };
        let palette = ColorSettings::default().active();
        draw(&mut cache, &f, palette, true, 600.0);
        draw(&mut cache, &f, palette, true, 600.0);
        assert_eq!(cache.builds, 1);
        draw(&mut cache, &f, palette, false, 600.0);
        assert_eq!(cache.builds, 2);
        draw(&mut cache, &f, palette, false, 500.0);
        assert_eq!(cache.builds, 3);
        let light = ColorSettings {
            dark: false,
            ..Default::default()
        }
        .active();
        draw(&mut cache, &f, light, false, 500.0);
        assert_eq!(cache.builds, 4);
        ctx.set_pixels_per_point(2.0);
        draw(&mut cache, &f, light, false, 500.0);
        assert_eq!(cache.builds, 5);
        let selected = tree.row(1, &|| false).unwrap();
        f = tree.frame(0, 64, &selected, &|| false).unwrap();
        draw(&mut cache, &f, light, false, 500.0);
        assert_eq!(cache.builds, 6);
        drop(cache);
        drop(f);
        drop(tree);
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    #[test]
    fn repeated_open_keeps_only_latest_request_while_old_threads_retire() {
        let ctx = egui::Context::default();
        let mut v = Viewer::empty(PathBuf::new(), Backend::Legacy(false));
        let (tx, rx) = std::sync::mpsc::channel();
        v.retiring = Some(std::thread::spawn(move || {
            rx.recv().unwrap();
        }));
        for n in 0..100 {
            v.open(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("target/nonexistent-s5-open-{n}.tmp")),
                &ctx,
            );
            assert!(v.task.is_none());
            assert!(v.retiring.is_some());
        }
        assert!(
            v.pending_open
                .as_ref()
                .unwrap()
                .ends_with("nonexistent-s5-open-99.tmp")
        );
        tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while v
            .retiring
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        while v.status != SessionStatus::Failed {
            let _ = ctx.run(Default::default(), |ctx| v.draw(ctx));
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(v.pending_open.is_none());
        assert!(v.retiring.is_none());
        assert!(v.frame.is_none());
    }
    #[test]
    fn dot_preserves_query_and_help_blocks_shortcut() {
        let ctx = egui::Context::default();
        let mut v = Viewer::empty(PathBuf::new(), Backend::Legacy(false));
        v.query = "/existing".into();
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Text(".".into())],
                ..Default::default()
            },
            |ctx| v.keyboard(ctx),
        );
        assert!(v.focus_path);
        assert_eq!(v.query, "/existing");
        v.focus_path = false;
        v.help = true;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Text(".".into())],
                ..Default::default()
            },
            |ctx| v.keyboard(ctx),
        );
        assert!(!v.focus_path);
        v.help = false;
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut v.query).id(egui::Id::new("paged_path")))
                    .request_focus();
            });
        });
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Text(".".into())],
                ..Default::default()
            },
            |ctx| {
                v.keyboard(ctx);
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut v.query).id(egui::Id::new("paged_path")),
                    );
                });
            },
        );
        assert!(!v.focus_path);
        assert!(v.query.contains('.'));
    }
    #[test]
    fn search_triggers_preserve_term_consume_trigger_and_reverse_direction() {
        let ctx = egui::Context::default();
        let mut v = Viewer::empty(PathBuf::new(), Backend::Legacy(false));
        v.search_term = "existing".into();
        for (text, reverse) in [("/", false), ("?", true)] {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Text(text.into())],
                    ..Default::default()
                },
                |ctx| {
                    v.keyboard(ctx);
                    assert!(ctx.input(|i| {
                        i.events
                            .iter()
                            .all(|e| !matches!(e, egui::Event::Text(t) if t == text))
                    }));
                },
            );
            assert!(v.focus_search);
            assert_eq!(v.search_reverse, reverse);
            assert_eq!(v.search_term, "existing");
            v.focus_search = false;
        }
        v.help = true;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Text("/".into())],
                ..Default::default()
            },
            |ctx| v.keyboard(ctx),
        );
        assert!(!v.focus_search);
    }
}
