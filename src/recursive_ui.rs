use super::*;
use crate::engine::recursive::{
    Action as FileAction, Operation as FileOperation, Output as FileOutput, Page, Session,
};
#[derive(Default)]
pub(super) struct View {
    pub session: Option<Session>,
    pub page: Option<Arc<Page>>,
    opening: Option<Receiver<Result<(Session, PathBuf), String>>>,
    pub clipboard: Option<String>,
    pub matches: Option<crate::engine::recursive::Hits>,
    match_index: u64,
    results: Option<(String, Vec<u64>, String)>,
    querying: bool,
    work_label: &'static str,
    searching: bool,
    search_revealed: bool,
    busy: bool,
    exporting: bool,
}
impl View {
    pub(super) fn match_label(&self) -> String {
        let n = self.matches.as_ref().map_or(0, |h| h.len());
        format!(
            "{} / {}{}",
            if n == 0 { 0 } else { self.match_index + 1 },
            n,
            if self.searching { " (searching)" } else { "" }
        )
    }
    pub(super) fn search_busy(&self) -> bool {
        self.searching
    }
    pub(super) fn query_busy(&self) -> bool {
        self.busy && self.work_label == "Path"
    }
    pub(super) fn export_busy(&self) -> bool {
        self.exporting
    }
    pub(super) fn opening(&self) -> bool {
        self.opening.is_some()
    }
}
impl Viewer {
    pub(super) fn clear_progressive(&mut self) {
        self.progressive.page = None;
        if let Some(s) = self.progressive.session.take() {
            crate::shutdown::defer(s.retire());
        }
        if let Some(rx) = self.progressive.opening.take()
            && let Ok(Ok((s, _))) = rx.try_recv()
        {
            crate::shutdown::defer(s.retire());
        }
        self.progressive = Default::default();
    }
    pub(super) fn load_progressive(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.loading_cancelled.store(true, Ordering::Relaxed);
        self.clear_progressive();
        self.pending = None;
        self.clear_top();
        let old = self.prepare_load();
        self.loading_cancelled = Arc::new(AtomicBool::new(false));
        self.load_started = Some(Instant::now());
        self.open_duration = None;
        self.error = None;
        self.search_error = None;
        self.path_results = None;
        self.progressive.matches = None;
        self.file = Some(path.clone());
        self.source = path.display().to_string();
        let cancel = self.loading_cancelled.clone();
        let ctx = ctx.clone();
        let waiting = self.waiting_for_old.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        self.progressive.opening = Some(rx);
        crate::shutdown::defer(std::thread::spawn(move || {
            if !wait_for_old_document(old, &waiting, &cancel, &ctx) {
                return;
            }
            let result = Session::open(&path, cancel, ctx.clone()).map(|s| (s, path));
            let _ = tx.send(result);
            ctx.request_repaint();
        }));
    }
    pub(super) fn poll_progressive(&mut self) {
        if let Some(rx) = &self.progressive.opening {
            match rx.try_recv() {
                Ok(Ok((s, path))) => {
                    self.progressive.session = Some(s);
                    self.progressive.opening = None;
                    self.file = Some(path);
                }
                Ok(Err(e)) => {
                    self.error = Some(e);
                    self.progressive.opening = None;
                    if self.doc.is_none() && self.colors.low_memory_load {
                        self.notice =
                            "Loading failed. The previous document must be reopened.".into();
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.progressive.opening = None;
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        if self.loading_cancelled.load(Ordering::Relaxed) && self.progressive.session.is_some() {
            self.clear_progressive();
            self.notice = "Opening cancelled".into();
            return;
        }
        let Some(s) = &self.progressive.session else {
            return;
        };
        if let Some(e) = s.error() {
            self.error = Some(e);
            self.clear_progressive();
            return;
        }
        if let Some(page) = s.poll() {
            if self.progressive.page.is_none() {
                self.cancel_operations();
                self.large_copy = None;
                if let Some(doc) = self.doc.take() {
                    crate::shutdown::defer(std::thread::spawn(move || drop(doc)));
                }
                self.expanded = Vec::new();
                self.rows = NodeIds::default();
                self.history = Vec::new();
                self.row_labels.clear();
                self.expanded_resource = None;
                self.preview_resource = None;
                self.term.clear();
                self.query = "$".into();
                self.notice.clear();
            }
            if self
                .progressive
                .page
                .as_ref()
                .is_some_and(|old| old.selected != page.selected)
            {
                self.scroll_to = true;
            }
            self.progressive.page = Some(page);
        }
        let Some(s) = &self.progressive.session else {
            return;
        };
        if s.statistics().ready && self.open_duration.is_none() {
            self.open_duration = self.load_started.take().map(|t| t.elapsed());
        }
        if let Some(update) = s.search_update() {
            self.progressive.searching = !update.complete;
            if update.complete {
                self.query_duration = Some(("Search", update.elapsed));
            }
            match update.result {
                Ok(hits) => {
                    self.progressive.match_index = self
                        .progressive
                        .match_index
                        .min(hits.len().saturating_sub(1));
                    if !self.progressive.search_revealed && !hits.is_empty() {
                        self.progressive.search_revealed = true;
                        self.progressive.match_index = if self.search_reverse {
                            hits.len() - 1
                        } else {
                            0
                        };
                        s.action(FileAction::Hit(hits.clone(), self.progressive.match_index));
                    }
                    self.progressive.matches = Some(hits);
                    self.search_error = None;
                }
                Err(e) => {
                    self.progressive.matches = None;
                    self.search_error = Some(e);
                }
            }
        }
        if let Some(completed) = s.completion() {
            if completed.export {
                self.progressive.exporting = false;
            } else {
                self.progressive.busy = false;
            }
            if !completed.export && matches!(self.progressive.work_label, "Search" | "Path") {
                self.query_duration = Some((
                    if self.progressive.querying {
                        "Path"
                    } else {
                        "Search"
                    },
                    completed.elapsed,
                ));
            }
            match completed.result {
                Ok(FileOutput::Search(ids)) => {
                    self.progressive.matches = Some(ids);
                    self.progressive.match_index = if self.search_reverse {
                        self.progressive
                            .matches
                            .as_ref()
                            .map_or(0, |h| h.len().saturating_sub(1))
                    } else {
                        0
                    };
                    if let Some(hits) = &self.progressive.matches
                        && !hits.is_empty()
                    {
                        s.action(FileAction::Hit(hits.clone(), self.progressive.match_index));
                    }
                    self.search_error = None;
                }
                Ok(FileOutput::Query { ids, preview }) => {
                    if let Some(id) = ids.first() {
                        s.action(FileAction::Select(*id));
                    }
                    self.progressive.results =
                        Some((self.pending_expression.clone(), ids, preview));
                    self.search_error = None;
                }
                Ok(FileOutput::Copy(text)) => {
                    s.cancel_operation();
                    self.progressive.busy = false;
                    self.progressive.clipboard = Some(text);
                    self.notice = "Copied".into();
                }
                Ok(FileOutput::Export(path)) => {
                    self.notice = format!("Exported {}", path.display())
                }
                Err(e) => {
                    self.search_error = Some(e);
                }
            }
        }
    }
    pub(super) fn progressive_tree(&mut self, ui: &mut egui::Ui) {
        let Some(page) = self.progressive.page.clone() else {
            return;
        };
        ui.label(RichText::new(&self.source).strong())
            .on_hover_ui(|ui| {
                if let Some(session) = &self.progressive.session {
                    let budget = session.budget();
                    let usage = budget.usage();
                    let limit = budget.limits().memory;
                    let mib = 1024.0 * 1024.0;
                    ui.label(format!(
                        "Managed memory: {:.1} / {:.1} MiB",
                        usage.memory as f64 / mib,
                        limit as f64 / mib,
                    ));
                    ui.label(format!(
                        "Peak managed memory: {:.1} MiB",
                        usage.peak_memory as f64 / mib
                    ));
                    ui.label("Includes reserved workspaces; this is not process RSS.");
                    ui.label("Set FERRISJSON_MEMORY_MIB before starting the app (64–32768 MiB).");
                }
            });
        ui.separator();
        let mut wanted = page.start;
        let mut actions = Vec::new();
        let stride = 25.0 + ui.spacing().item_spacing.y;
        self.page_rows = (ui.available_height() / stride).floor().max(1.0) as usize;
        let mut area = egui::ScrollArea::both()
            .id_salt("tree")
            .auto_shrink([false, false]);
        if self.scroll_to {
            area = area.vertical_scroll_offset(
                (page.selection.index as f32 * stride - ui.available_height() / 2.0).max(0.0),
            );
            self.scroll_to = false;
        }
        area.show_rows(ui, 25.0, page.count as usize, |ui, range| {
            wanted = range.start as u64;
            self.viewport_first = range.start;
            for n in range {
                let Some(row) = page.rows.iter().find(|r| r.index == n as u64) else {
                    ui.label("Loading...");
                    continue;
                };
                let p = self.colors.active();
                let fill = if row.id == page.selected {
                    color(p.selection)
                } else if n % 2 == 0 {
                    color(p.stripe)
                } else {
                    Color32::TRANSPARENT
                };
                egui::Frame::new().fill(fill).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.set_height(25.0);
                        ui.add_space(row.depth as f32 * 18.0);
                        if row.kind.container() {
                            if ui
                                .add(
                                    egui::Button::new(if row.expanded { "▾" } else { "▸" })
                                        .frame(false),
                                )
                                .clicked()
                            {
                                actions.push(FileAction::Toggle(row.id));
                            }
                        } else {
                            ui.add_space(26.0);
                        }
                        let c = kind_color(row.kind.name(), p);
                        for text in [&row.label, &row.preview] {
                            if ui
                                .add(
                                    egui::Label::new(RichText::new(text).monospace().color(c))
                                        .sense(egui::Sense::click()),
                                )
                                .clicked()
                            {
                                actions.push(FileAction::Select(row.id));
                            }
                        }
                    });
                });
            }
        });
        if let Some(s) = &self.progressive.session {
            s.request(wanted);
            for a in actions {
                s.action(a);
            }
        }
    }
    pub(super) fn progressive_selection(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let p = self.progressive.page.as_ref().unwrap().clone();
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.heading("Value preview");
            ui.colored_label(
                kind_color(p.selection.kind.name(), self.colors.active()),
                p.selection.kind.name(),
            );
            changed = ui.checkbox(&mut self.colors.wrap_values, "Wrap").changed();
        });
        if changed {
            self.save_colors();
        }
        ui.add(egui::Label::new(RichText::new(&p.selection.path).monospace()).truncate())
            .on_hover_text(&p.selection.path);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(p.ready, egui::Button::new("Copy JSON"))
                .clicked()
            {
                self.progressive_copy(ctx, false);
            }
            if ui.button("Copy path").clicked() {
                self.progressive_copy(ctx, true);
            }
            if ui
                .add_enabled(
                    p.ready && !self.progressive.busy,
                    egui::Button::new("Export"),
                )
                .clicked()
            {
                self.progressive_export(false);
            }
        });
        ui.separator();
        json_preview(
            ui,
            &p.detail,
            self.colors.active(),
            "value_preview",
            self.colors.wrap_values,
        );
    }
    pub(super) fn progressive_results(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal_wrapped(|ui| {
            ui.heading("JSONPath results");
            if ui.checkbox(&mut self.colors.wrap_results, "Wrap").changed() {
                self.save_colors();
            }
        });
        let Some((expression, ids, preview)) = self.progressive.results.clone() else {
            ui.add_space(12.0);
            ui.label("Run a JSONPath query to explore matching values.");
            ui.monospace("$.users[*].name");
            ui.monospace("$..email");
            return;
        };
        ui.monospace(expression);
        let mut copy = false;
        let mut export = false;
        let mut clear = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} matches", ids.len()));
            copy = ui.button("Copy").clicked();
            export = ui.button("Export").clicked();
            clear = ui.button("Clear").clicked();
        });
        ui.separator();
        json_preview(
            ui,
            &preview,
            self.colors.active(),
            "path_results",
            self.colors.wrap_results,
        );
        if copy && let Some(s) = &self.progressive.session {
            s.submit(FileOperation::Copy { ids, array: true });
            self.progressive.busy = true;
        }
        if export {
            self.progressive_export(true);
        }
        if clear {
            self.progressive.results = None;
            self.query.clear();
            if let Some(s) = &self.progressive.session {
                s.cancel_operation();
            }
        }
        let _ = ctx;
    }
    pub(super) fn progressive_search(&mut self) {
        self.search_due = None;
        self.progressive.matches = None;
        self.progressive.match_index = 0;
        self.progressive.search_revealed = false;
        self.progressive.searching = false;
        self.search_error = None;
        if self.term.is_empty() {
            if let Some(s) = &self.progressive.session {
                s.clear_search();
            }
            return;
        }
        if let Some(s) = &self.progressive.session {
            s.submit(FileOperation::Search(crate::engine::search::Query {
                term: self.term.clone(),
                case: self.case,
                regex: self.regex,
                scope: self.scope,
            }));
            self.progressive.searching = true;
        }
    }
    pub(super) fn progressive_cancel_export(&mut self) {
        if let Some(s) = &self.progressive.session {
            s.cancel_export();
        }
        self.progressive.exporting = false;
    }
    pub(super) fn progressive_clear_query(&mut self) {
        self.progressive.results = None;
        if let Some(s) = &self.progressive.session {
            s.cancel_operation();
        }
        self.progressive.busy = false;
    }
    pub(super) fn progressive_cancel_operation(&mut self) {
        if let Some(s) = &self.progressive.session {
            s.cancel_operation();
            s.clear_search();
        }
        self.progressive.busy = false;
        self.progressive.searching = false;
    }
    pub(super) fn progressive_query(&mut self) {
        self.progressive.results = None;
        if self.query.trim().is_empty() {
            if let Some(s) = &self.progressive.session {
                s.cancel_operation();
            }
            return;
        }
        self.progressive.querying = true;
        self.progressive.work_label = "Path";
        self.pending_expression = self.query.clone();
        if let Some(s) = &self.progressive.session {
            s.submit(FileOperation::Query(self.query.clone()));
            self.progressive.busy = true;
        }
    }
    pub(super) fn progressive_match(&mut self, backwards: bool, count: usize) {
        let Some(hits) = &self.progressive.matches else {
            return;
        };
        let len = hits.len();
        if len == 0 {
            return;
        }
        let n = count as u64;
        self.progressive.match_index = if self.progressive.searching {
            if backwards {
                self.progressive.match_index.saturating_sub(n)
            } else {
                self.progressive.match_index.saturating_add(n).min(len - 1)
            }
        } else if backwards {
            (self.progressive.match_index + len - n % len) % len
        } else {
            (self.progressive.match_index + n % len) % len
        };
        if let Some(s) = &self.progressive.session {
            s.action(FileAction::Hit(hits.clone(), self.progressive.match_index));
            self.scroll_to = true;
        }
    }
    pub(super) fn progressive_copy(&mut self, _ctx: &egui::Context, path: bool) {
        let Some(page) = &self.progressive.page else {
            return;
        };
        self.progressive.work_label = "Copy";
        if let Some(s) = &self.progressive.session {
            if path {
                s.submit(FileOperation::Path(page.selected));
                self.progressive.busy = true;
            } else {
                s.submit(FileOperation::Copy {
                    ids: vec![page.selected],
                    array: false,
                });
                self.progressive.busy = true;
            }
        }
    }
    pub(super) fn progressive_export(&mut self, results: bool) {
        if self.progressive.exporting {
            self.notice = "An export is already in progress".into();
            return;
        }
        let ids = if results {
            self.progressive.results.as_ref().map(|r| r.1.clone())
        } else {
            self.progressive.page.as_ref().map(|p| vec![p.selected])
        };
        if let Some(ids) = ids
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .set_file_name("selection.json")
                .save_file()
            && let Some(s) = &self.progressive.session
        {
            s.submit(FileOperation::Export {
                ids,
                array: results,
                path,
            });
            self.progressive.exporting = true;
        }
    }
    pub(super) fn progressive_keyboard(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            self.vim.cancel();
            return;
        }
        let mut actions = Vec::new();
        for (key, action) in [
            (Key::ArrowDown, FileAction::Move(1)),
            (Key::ArrowUp, FileAction::Move(-1)),
            (Key::ArrowLeft, FileAction::Parent),
            (Key::ArrowRight, FileAction::Child),
            (Key::Home, FileAction::Rank(0)),
            (Key::End, FileAction::Rank(u64::MAX)),
            (Key::PageDown, FileAction::Move(self.page_rows as i64)),
            (Key::PageUp, FileAction::Move(-(self.page_rows as i64))),
        ] {
            if ctx.input(|i| i.key_pressed(key)) {
                actions.push(action);
            }
        }
        let events = ctx.input(|i| i.events.clone());
        for e in events {
            if let egui::Event::Text(text) = e {
                for ch in text.chars() {
                    if ch == '.' {
                        ctx.memory_mut(|m| m.request_focus(egui::Id::new("query_field")));
                        continue;
                    }
                    if let Some(cmd) = self.vim.feed(ch) {
                        let count = cmd.count as i64;
                        match cmd.action {
                            Action::Move(n) => actions.push(FileAction::Move(i64::from(n) * count)),
                            Action::Left => actions.push(FileAction::Parent),
                            Action::Right => actions.push(FileAction::Child),
                            Action::First => {
                                actions.push(FileAction::Rank(if cmd.explicit_count {
                                    cmd.count.saturating_sub(1) as u64
                                } else {
                                    0
                                }))
                            }
                            Action::Last => actions.push(FileAction::Rank(if cmd.explicit_count {
                                cmd.count.saturating_sub(1) as u64
                            } else {
                                u64::MAX
                            })),
                            Action::Toggle => {
                                if let Some(p) = &self.progressive.page {
                                    actions.push(FileAction::Toggle(p.selected));
                                }
                            }
                            Action::Open | Action::Close => {
                                if let Some(p) = &self.progressive.page
                                    && p.selection.expanded != matches!(cmd.action, Action::Open)
                                {
                                    actions.push(FileAction::Toggle(p.selected));
                                }
                            }
                            Action::ExpandAll => actions.push(FileAction::All(true)),
                            Action::CollapseAll => actions.push(FileAction::All(false)),
                            Action::History(back) => actions.push(if back {
                                FileAction::Back
                            } else {
                                FileAction::Forward
                            }),
                            Action::Search(reverse) => self.start_search(reverse),
                            Action::Match(back) => self.progressive_match(back, cmd.count),
                            Action::Copy(path) => self.progressive_copy(ctx, path),
                            Action::Screen(c) => {
                                let n = match c {
                                    'H' => self.viewport_first,
                                    'M' => self.viewport_first + self.page_rows / 2,
                                    _ => self.viewport_first + self.page_rows.saturating_sub(1),
                                };
                                actions.push(FileAction::Rank(n as u64));
                            }
                        }
                    }
                }
            }
        }
        if let Some(s) = &self.progressive.session {
            for a in actions {
                s.action(a);
                self.scroll_to = true;
            }
        }
    }
}

pub(super) fn benchmark(
    path: PathBuf,
    indexed: bool,
    search_cases: bool,
) -> Result<serde_json::Value, String> {
    let ctx = egui::Context::default();
    let mut app = Viewer::empty();
    app.colors.indexed_files = indexed;
    let started = Instant::now();
    app.load_file(path, &ctx);
    let until = Instant::now() + Duration::from_secs(600);
    let mut frame_times = std::collections::VecDeque::with_capacity(4096);
    let mut first = None;
    let mut expanded = None;
    let mut expand_request = None;
    let (statistics, budget) = loop {
        let frame_started = Instant::now();
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
        if frame_times.len() == 4096 {
            frame_times.pop_front();
        }
        frame_times.push_back(frame_started.elapsed().as_micros() as u64);
        if let Some(error) = &app.error {
            return Err(error.clone());
        }
        if let Some(page) = &app.progressive.page {
            if first.is_none() && (page.rows.iter().any(|r| r.depth > 0) || page.ready) {
                first = Some(started.elapsed().as_micros() as u64);
                if let Some(row) = page
                    .rows
                    .iter()
                    .find(|r| r.depth == 1 && r.kind.container())
                {
                    let s = app.progressive.session.as_ref().unwrap();
                    s.action(FileAction::Toggle(row.id));
                    expand_request = Some((Instant::now(), row.id));
                }
            }
            if let Some((time, id)) = expand_request
                && page.rows.iter().any(|r| r.depth > 1)
                && page.rows.iter().any(|r| r.id == id && r.expanded)
            {
                expanded = Some(time.elapsed().as_micros() as u64);
                expand_request = None;
            }
            if page.ready && first.is_some() {
                let s = app.progressive.session.as_ref().unwrap();
                break (s.statistics(), s.budget());
            }
        }
        if Instant::now() > until {
            return Err("Progressive benchmark timeout".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let complete_us = started.elapsed().as_micros() as u64;
    let mut searches = Vec::new();
    if search_cases {
        for term in ["1", "0"] {
            app.term = term.into();
            app.scope = SearchScope::Values;
            app.progressive_search();
            let begin = Instant::now();
            while app.progressive.search_busy() {
                app.poll_progressive();
                if let Some(e) = &app.search_error {
                    return Err(e.clone());
                }
                if begin.elapsed() > Duration::from_secs(600) {
                    return Err("Search benchmark timeout".into());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let hits = app
                .progressive
                .matches
                .as_ref()
                .ok_or("Missing search completion")?;
            searches.push(serde_json::json!({"term":term,"count":hits.len(),"complete_us":begin.elapsed().as_micros() as u64}));
            if !hits.is_empty() {
                app.progressive
                    .session
                    .as_ref()
                    .unwrap()
                    .action(FileAction::Hit(hits.clone(), hits.len() - 1));
            }
        }
    }
    let mut navigation = Vec::new();
    for position in 0..3 {
        // Search reveal can expand ancestors. Re-read the settled page after
        // each move; End must resolve against the worker's current row count.
        let count = app.progressive.page.as_ref().unwrap().count;
        let rank = match position {
            0 => 0,
            1 => count / 2,
            _ => u64::MAX,
        };
        app.progressive
            .session
            .as_ref()
            .unwrap()
            .action(FileAction::Rank(rank));
        let begin = Instant::now();
        let previous = app.progressive.page.as_ref().unwrap().clone();
        loop {
            app.poll_progressive();
            if let Some(e) = &app.error {
                return Err(e.clone());
            }
            if let Some(p) = &app.progressive.page
                && !Arc::ptr_eq(p, &previous)
                && p.selection.index
                    == if position == 2 {
                        p.count.saturating_sub(1)
                    } else {
                        rank
                    }
            {
                navigation.push(serde_json::json!({"rank":p.selection.index,"visible_count":p.count,"path":p.selection.path,"latency_us":begin.elapsed().as_micros()as u64}));
                break;
            }
            if begin.elapsed() > Duration::from_secs(30) {
                return Err("Navigation timeout".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let mut frames = frame_times.into_iter().collect::<Vec<_>>();
    frames.sort_unstable();
    let p95 = frames
        .get(frames.len().saturating_sub(1) * 95 / 100)
        .copied()
        .unwrap_or(0);
    let maximum = frames.last().copied().unwrap_or(0);
    let no_legacy = app.doc.is_none() && app.rows.is_empty() && app.expanded.is_empty();
    let cancel = Instant::now();
    app.clear_progressive();
    drop(app);
    crate::shutdown::finish();
    let usage = budget.usage();
    if usage.memory != 0 || usage.disk != 0 || !no_legacy {
        return Err(format!("Retained resources or legacy document: {usage:?}"));
    }
    Ok(
        serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"scope":"B default original UI headless egui submission; full recursive validation; no native presentation","indexed_setting":indexed,"searches":searches,"navigation":navigation,"headless_frame_p95_us":p95,"headless_frame_max_us":maximum,"first_frame_us":first,"first_expand_us":expanded,"complete_us":complete_us,"statistics":statistics,"no_legacy_document":no_legacy,"resources_after_retire":usage,"cleanup_us":cancel.elapsed().as_micros()as u64}),
    )
}

pub(super) fn benchmark_stream_search(
    path: PathBuf,
    indexed: bool,
    term: String,
) -> Result<serde_json::Value, String> {
    let ctx = egui::Context::default();
    let mut app = Viewer::empty();
    app.colors.indexed_files = indexed;
    let opened = Instant::now();
    app.load_file(path, &ctx);
    let mut submitted: Option<Instant> = None;
    let mut first_result = None;
    let mut first_reveal = None;
    let mut first_ready = None;
    let mut search_complete = None;
    let mut index_complete = None;
    let mut ready_at_submit = false;
    let mut frames = std::collections::VecDeque::with_capacity(4096);
    let budget = loop {
        let frame = Instant::now();
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
        if frames.len() == 4096 {
            frames.pop_front();
        }
        frames.push_back(frame.elapsed().as_micros() as u64);
        if let Some(error) = app.error.as_ref().or(app.search_error.as_ref()) {
            return Err(error.clone());
        }
        if let Some(page) = &app.progressive.page {
            if let Some(submit_time) = submitted {
                let elapsed = submit_time.elapsed().as_micros() as u64;
                if app
                    .progressive
                    .matches
                    .as_ref()
                    .is_some_and(|h| !h.is_empty())
                {
                    if first_result.is_none() {
                        first_result = Some(elapsed);
                        first_ready =
                            Some(app.progressive.session.as_ref().unwrap().statistics().ready);
                    }
                    if (page.selection.path != "$" || !page.selection.kind.container())
                        && first_reveal.is_none()
                    {
                        first_reveal = Some(elapsed);
                    }
                }
                if !app.progressive.search_busy() && search_complete.is_none() {
                    search_complete = Some(elapsed);
                }
                if page.ready && index_complete.is_none() {
                    index_complete = Some(opened.elapsed().as_micros() as u64);
                }
                if search_complete.is_some()
                    && index_complete.is_some()
                    && (app
                        .progressive
                        .matches
                        .as_ref()
                        .is_none_or(|h| h.is_empty())
                        || first_reveal.is_some())
                {
                    break app.progressive.session.as_ref().unwrap().budget();
                }
            } else {
                ready_at_submit = app.progressive.session.as_ref().unwrap().statistics().ready;
                app.term = term.clone();
                app.scope = SearchScope::Values;
                submitted = Some(Instant::now());
                app.progressive_search();
            }
        }
        if opened.elapsed() > Duration::from_secs(600) {
            return Err("Streaming search benchmark timeout".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let hits = app
        .progressive
        .matches
        .as_ref()
        .ok_or("Missing completed search")?
        .clone();
    let count = hits.len();
    let mut navigation = Vec::new();
    if count > 0 {
        for rank in [0, count / 2, count - 1] {
            app.progressive
                .session
                .as_ref()
                .unwrap()
                .action(FileAction::Hit(hits.clone(), rank));
            let previous = app.progressive.page.as_ref().unwrap().clone();
            let begin = Instant::now();
            loop {
                app.poll_progressive();
                if let Some(error) = &app.error {
                    return Err(error.clone());
                }
                if let Some(page) = &app.progressive.page
                    && !Arc::ptr_eq(page, &previous)
                {
                    navigation.push(serde_json::json!({"match_rank":rank,"path":page.selection.path,"latency_us":begin.elapsed().as_micros() as u64}));
                    break;
                }
                if begin.elapsed() > Duration::from_secs(30) {
                    return Err("Search navigation timeout".into());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    drop(hits);
    let usage_before = budget.usage();
    let mut frames = frames.into_iter().collect::<Vec<_>>();
    frames.sort_unstable();
    let p95 = frames[frames.len().saturating_sub(1) * 95 / 100];
    let maximum = frames.last().copied().unwrap_or(0);
    let no_legacy = app.doc.is_none() && app.rows.is_empty() && app.expanded.is_empty();
    app.clear_progressive();
    drop(app);
    crate::shutdown::finish();
    let usage = budget.usage();
    if usage.memory != 0 || usage.disk != 0 || !no_legacy {
        return Err(format!("Retained resources or legacy document: {usage:?}"));
    }
    Ok(
        serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"scope":"C original UI headless egui submission; search starts at first page; no native presentation",
        "indexed_setting":indexed,"term":term,"count":count,"ready_at_submit":ready_at_submit,"ready_at_first_result":first_ready,
        "first_result_us":first_result,"first_reveal_us":first_reveal,"search_complete_us":search_complete,"index_complete_from_open_us":index_complete,
        "navigation":navigation,"recent_frame_p95_us":p95,"recent_frame_max_us":maximum,"managed_usage_before_retire":usage_before,"resources_after_retire":usage,"no_legacy_document":no_legacy}),
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    fn wait(app: &mut Viewer) {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(e) = &app.error {
                panic!("{e}")
            }
            if app.progressive.page.as_ref().is_some_and(|p| p.ready) {
                return;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn default_file_ui_keeps_one_session_and_never_installs_a_legacy_document() {
        let ctx = egui::Context::default();
        let mut app = Viewer::empty();
        app.load_file(PathBuf::from("examples/sample.json"), &ctx);
        wait(&mut app);
        assert!(app.doc.is_none() && app.expanded.is_empty() && app.rows.is_empty());
        let session = app.progressive.session.as_ref().unwrap() as *const Session;
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
        assert!(!output.shapes.is_empty());
        assert_eq!(
            app.progressive.session.as_ref().unwrap() as *const Session,
            session
        );
        assert!(app.doc.is_none());
        app.query = "$".into();
        app.progressive_query();
        let until = Instant::now() + Duration::from_secs(5);
        while app.progressive.busy {
            app.poll_progressive();
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.progressive.results.is_some());
        app.query.clear();
        app.progressive_clear_query();
        assert!(app.progressive.results.is_none());
        app.term = "id".into();
        app.progressive_search();
        app.term.clear();
        app.schedule_search(&ctx);
        assert!(!app.progressive.busy && app.progressive.matches.is_none());
    }
    #[test]
    fn rapid_file_replacement_and_paste_retire_the_file_session() {
        let ctx = egui::Context::default();
        let mut app = Viewer::empty();
        for _ in 0..8 {
            app.load_file(PathBuf::from("examples/sample.json"), &ctx);
        }
        wait(&mut app);
        let budget = app.progressive.session.as_ref().unwrap().budget();
        app.load_text("{\"fresh\":true}".into(), "Pasted JSON".into(), &ctx);
        let until = Instant::now() + Duration::from_secs(5);
        while app.pending.is_some() {
            app.poll();
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.progressive.session.is_none());
        assert!(app.doc.as_ref().unwrap().value().is_some());
        drop(app);
        crate::shutdown::finish();
        assert_eq!(budget.usage().memory, 0);
        assert_eq!(budget.usage().disk, 0);
    }
    #[test]
    fn streaming_ui_keeps_selection_and_clamps_navigation_until_completion() {
        let path = std::env::temp_dir().join(format!("FerrisJson-c-ui-{}.tmp", std::process::id()));
        std::fs::write(
            &path,
            format!(
                "[\"needle\",\"{}\",\"needle\"]",
                "x".repeat(16 * 1024 * 1024)
            ),
        )
        .unwrap();
        let ctx = egui::Context::default();
        let mut app = Viewer::empty();
        app.load_file(path.clone(), &ctx);
        let until = Instant::now() + Duration::from_secs(20);
        while app.progressive.page.is_none() {
            app.poll_progressive();
            assert!(app.error.is_none());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        app.term = "needle".into();
        app.progressive_search();
        while app
            .progressive
            .matches
            .as_ref()
            .is_none_or(|h| h.is_empty())
        {
            app.poll_progressive();
            assert!(app.search_error.is_none());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.progressive.search_busy());
        assert!(app.progressive.match_label().contains("searching"));
        app.progressive_match(false, 100);
        assert_eq!(app.progressive.match_index, 0);
        // A user navigation after the initial reveal must survive later batches.
        app.progressive
            .session
            .as_ref()
            .unwrap()
            .action(FileAction::Rank(0));
        while app.progressive.search_busy()
            || app
                .progressive
                .page
                .as_ref()
                .is_none_or(|p| p.selection.path != "$")
        {
            app.poll_progressive();
            assert!(app.search_error.is_none());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(app.progressive.matches.as_ref().unwrap().len(), 2);
        assert_eq!(app.progressive.page.as_ref().unwrap().selection.path, "$");
        assert!(!app.progressive.match_label().contains("searching"));
        app.progressive.match_index = 1;
        app.progressive_match(false, 1);
        assert_eq!(app.progressive.match_index, 0);
        app.term.clear();
        app.progressive_search();
        assert!(!app.progressive.search_busy());
        assert!(app.progressive.matches.is_none());
        app.clear_progressive();
        drop(app);
        crate::shutdown::finish();
        let _ = std::fs::remove_file(path);
    }
}
