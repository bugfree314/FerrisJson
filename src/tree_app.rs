//! Paged tree viewer for Legacy and Sparse file backends, rendered with gpui.
use crate::{
    engine::{
        session::SessionStatus,
        tree::TreeFrame,
        tree_navigation::Navigation,
    },
    palette::{ColorSettings, Palette, color},
    tree_worker::{Backend, Task},
    ui::{self, InputEvent},
    vim::{Action, Vim},
};
use gpui::prelude::*;
use gpui::*;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

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
    jsonpath_help: bool,
    vim: Vim,
    focus_path: bool,
    viewport_rows: u64,
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
    search_highlight: Option<ui::SearchHighlight>,
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
    focus_handle: FocusHandle,
    wake: crate::repaint::Context,
    path_input: Entity<ui::TextInput>,
    row_input: Entity<ui::TextInput>,
    jsonpath_input: Entity<ui::TextInput>,
    search_input: Entity<ui::TextInput>,
    path_sub: Option<Subscription>,
    row_sub: Option<Subscription>,
    jsonpath_sub: Option<Subscription>,
    search_sub: Option<Subscription>,
    search_generation: u64,
    sync_inputs: bool,
    pending_blur: bool,
}

impl Viewer {
    pub fn new(
        _window: &mut Window,
        cx: &mut Context<Self>,
        path: PathBuf,
        backend: Backend,
    ) -> Self {
        let mut viewer = Self::empty(cx, path.clone(), backend);
        viewer.colors = ColorSettings::load().unwrap_or_default();
        viewer.open(path, cx);
        viewer
    }

    fn empty(cx: &mut Context<Self>, path: PathBuf, backend: Backend) -> Self {
        let (wake_tx, wake_rx) = async_channel::unbounded::<()>();
        let wake = crate::repaint::Context::with_notify(Arc::new(move || {
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
        let mut viewer = Self {
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
            jsonpath_help: false,
            vim: Vim::default(),
            focus_path: false,
            viewport_rows: 24,
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
            focus_handle: cx.focus_handle(),
            wake,
            path_input: cx.new(|cx| ui::TextInput::new("/items/0", 4096, cx)),
            row_input: cx.new(|cx| ui::TextInput::new("1", 20, cx)),
            jsonpath_input: cx.new(|cx| ui::TextInput::new("$.items[?@.active]", 4096, cx)),
            search_input: cx.new(|cx| ui::TextInput::new("/ Search forward...", 16 * 1024, cx)),
            path_sub: None,
            row_sub: None,
            jsonpath_sub: None,
            search_sub: None,
            search_generation: 0,
            sync_inputs: true,
            pending_blur: false,
        };
        let path_input = viewer.path_input.clone();
        viewer.path_sub = Some(cx.subscribe(&path_input, |this: &mut Self, _input, event: &InputEvent, cx| {
            match event {
                InputEvent::Changed(text) => this.query = text.to_string(),
                InputEvent::Submitted => {
                    this.command(Navigation::Locate(this.query.clone()));
                    this.pending_blur = true;
                }
                InputEvent::Escape => {
                    this.vim.cancel();
                    this.pending_blur = true;
                }
            }
            cx.notify();
        }));
        let row_input = viewer.row_input.clone();
        viewer.row_sub = Some(cx.subscribe(&row_input, |this: &mut Self, _input, event: &InputEvent, cx| {
            match event {
                InputEvent::Changed(text) => this.row = text.to_string(),
                InputEvent::Submitted => {
                    match this.row.parse::<u64>() {
                        Ok(n) if n > 0 => this.command(Navigation::Row(n - 1)),
                        _ => this.notice = "Enter a positive integer row number".into(),
                    }
                    this.pending_blur = true;
                }
                InputEvent::Escape => {
                    this.vim.cancel();
                    this.pending_blur = true;
                }
            }
            cx.notify();
        }));
        let jsonpath_input = viewer.jsonpath_input.clone();
        viewer.jsonpath_sub = Some(cx.subscribe(&jsonpath_input, |this: &mut Self, _input, event: &InputEvent, cx| {
            match event {
                InputEvent::Changed(text) => this.jsonpath = text.to_string(),
                InputEvent::Submitted => {
                    this.start_query();
                    this.pending_blur = true;
                }
                InputEvent::Escape => {
                    this.vim.cancel();
                    this.pending_blur = true;
                }
            }
            cx.notify();
        }));
        let search_input = viewer.search_input.clone();
        viewer.search_sub = Some(cx.subscribe(&search_input, |this: &mut Self, _input, event: &InputEvent, cx| {
            match event {
                InputEvent::Changed(text) => {
                    this.search_term = text.to_string();
                    this.search_changed(cx);
                }
                InputEvent::Submitted => {
                    this.start_search();
                    this.pending_blur = true;
                }
                InputEvent::Escape => {
                    this.vim.cancel();
                    this.pending_blur = true;
                }
            }
            cx.notify();
        }));
        viewer
    }

    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() {
            self.open(path, cx);
        }
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
        self.vim.cancel();
        self.pending_open = Some(path);
        if self.retiring.is_none() {
            self.start_pending(cx);
        }
        cx.notify();
    }

    fn start_pending(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.pending_open.take() else {
            return;
        };
        self.task = match Task::new(path, self.backend.clone(), self.wake.clone()) {
            Ok(task) => Some(task),
            Err(error) => {
                self.status = SessionStatus::Failed;
                self.notice = error;
                None
            }
        };
        cx.notify();
    }

    fn command(&mut self, command: Navigation) {
        self.page_request = None;
        self.query_page_request = None;
        if let Some(task) = &self.task
            && let Err(error) = task.command(command)
        {
            self.notice = error;
        }
    }

    fn output(&mut self, path: Option<PathBuf>, copy_path: bool) {
        if let (Some(task), Some(frame)) = (&self.task, &self.frame) {
            if !frame.exact || self.status != SessionStatus::Ready {
                self.notice = "Pending: complete validation required".into();
                return;
            }
            if let Err(error) = task.output(frame.selected.clone(), path, copy_path) {
                self.notice = error;
            }
        }
    }

    fn start_search(&mut self) {
        self.search_highlight = ui::SearchHighlight::new(
            &self.search_term,
            self.search_case,
            self.search_regex,
            self.search_scope,
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
            let query = crate::engine::search::Query {
                term: self.search_term.clone(),
                case: self.search_case,
                regex: self.search_regex,
                scope: self.search_scope,
            };
            if let Err(error) = task.search_query(query, self.search_reverse) {
                self.search_highlight = None;
                self.notice = error;
            }
        }
    }

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        self.search_highlight = None;
        if let Some(task) = &self.task {
            task.search_clear(false);
        }
        self.search_page = None;
        self.page_request = None;
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        if self.colors.live_search && !self.search_term.is_empty() {
            self.search_due = Some(Instant::now() + Duration::from_millis(200));
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.search_generation == generation
                        && this.search_due.is_some_and(|due| Instant::now() >= due)
                    {
                        this.start_search();
                        cx.notify();
                    }
                });
            })
            .detach();
        } else {
            self.search_due = None;
        }
    }

    fn start_query(&mut self) {
        self.query_page = None;
        self.query_page_error = None;
        self.query_page_request = None;
        self.query_start = 0;
        self.query_selected = None;
        if let Some(task) = &self.task {
            if self.jsonpath.trim().is_empty() {
                task.query_clear(true);
                self.query_snapshot = None;
                return;
            }
            if let Err(error) = task.query_request(self.jsonpath.clone()) {
                self.notice = error;
            }
            self.query_snapshot = Some(task.query.poll());
        }
    }

    fn next_query_match(&mut self, backwards: bool) {
        let count = self
            .query_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.state.as_ref())
            .map_or(0, |state| state.count);
        if count == 0 {
            return;
        }
        let index = match self.query_selected {
            None => {
                if backwards {
                    count - 1
                } else {
                    0
                }
            }
            Some(0) if backwards => count - 1,
            Some(current) if backwards => current - 1,
            Some(current) => {
                if current + 1 >= count {
                    0
                } else {
                    current + 1
                }
            }
        };
        if let Some(task) = &self.task {
            match task.query_select(index) {
                Ok(()) => {
                    self.query_selected = Some(index);
                    let start = index / 16 * 16;
                    if self.query_start != start {
                        self.query_start = start;
                        self.query_page = None;
                        self.query_page_error = None;
                        self.query_page_request = None;
                    }
                }
                Err(error) => self.notice = error,
            }
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if self
            .retiring
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            let _ = self.retiring.take().unwrap().join();
            self.start_pending(cx);
        }
        if let Some(task) = &self.task {
            let poll = task.poll();
            self.query_snapshot = Some(task.query.poll());
            if let Some(page) = poll.query_page
                && page.generation == task.query.generation()
                && page.start == self.query_start
                && self.query_page_request.is_none_or(|(_, _, count)| page.count >= count)
            {
                self.query_page = Some(page);
                self.query_page_error = None;
            }
            if let Some((generation, start, error)) = poll.query_page_error
                && generation == task.query.generation()
                && start == self.query_start
            {
                self.query_page_error = Some(error);
            }
            if let Some(snapshot) = &self.query_snapshot
                && let Some(state) = &snapshot.state
            {
                if state.count > 0 && self.query_selected.is_none() {
                    match task.query_select(0) {
                        Ok(()) => self.query_selected = Some(0),
                        Err(error) => self.notice = error,
                    }
                }
                let key = (
                    snapshot.generation,
                    self.query_start,
                    state.count.min(self.query_start.saturating_add(16)),
                );
                if state.count > self.query_start && self.query_page_request != Some(key) {
                    match task.query_page(self.query_start) {
                        Ok(()) => self.query_page_request = Some(key),
                        Err(error) => self.notice = error,
                    }
                }
            }
            if let Some((generation, clipboard)) = poll.query_clipboard
                && generation == task.query.generation()
            {
                cx.write_to_clipboard(ClipboardItem::new_string(clipboard.text));
            }
            let previous_cursor = self.search.as_ref().and_then(|snapshot| snapshot.cursor);
            self.search = Some(task.search.poll());
            if let Some(cursor) = self.search.as_ref().and_then(|snapshot| snapshot.cursor)
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
                && self.page_request.is_none_or(|(_, _, count)| page.count >= count)
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
                            self.frame_epoch = epoch;
                            self.frame = Some(frame);
                        }
                    }
                    Err(error) => self.notice = error,
                }
            }
            if !poll.status.message.is_empty() {
                self.notice = poll.status.message;
            }
            if !poll.notice.is_empty() {
                self.notice = poll.notice;
            }
            if let Some(clipboard) = poll.clipboard {
                cx.write_to_clipboard(ClipboardItem::new_string(clipboard.text));
            }
            if let Some((text, _lease)) = poll.path_copy {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            if matches!(
                self.status,
                SessionStatus::Failed | SessionStatus::Cancelled
            ) {
                self.frame = None;
            }
        }
    }

    fn palette(&self) -> Palette {
        self.colors.active()
    }

    fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.path_input.read(cx).focus_handle(cx).is_focused(window)
            || self.row_input.read(cx).focus_handle(cx).is_focused(window)
            || self.jsonpath_input.read(cx).focus_handle(cx).is_focused(window)
            || self.search_input.read(cx).focus_handle(cx).is_focused(window)
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
        if key == "f1" {
            self.help = !self.help;
            cx.notify();
        }
        if key == "escape" {
            self.vim.cancel();
            self.command(Navigation::CancelPending);
            self.help = false;
            window.blur();
            cx.notify();
            return;
        }
        if self.help {
            return;
        }
        if secondary && key == "o" {
            self.vim.cancel();
            self.open_dialog(cx);
            return;
        }
        if key == "f4" {
            self.next_query_match(m.shift);
            cx.notify();
            return;
        }
        if key == "f5" {
            self.open(self.path.clone(), cx);
            return;
        }
        if self.input_focused(window, cx) {
            return;
        }
        if !m.control && !m.alt && !m.platform
            && let Some(ch) = keystroke.key_char.as_deref().and_then(|s| s.chars().next())
            && ch != ' '
        {
            if ch == '.' {
                self.focus_path = true;
                cx.notify();
                return;
            }
            if let Some(command) = self.vim.feed(ch) {
                let count = command.count as u64;
                let navigation = match command.action {
                    Action::Move(delta) => Some(Navigation::Move(delta as i64 * count as i64)),
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
                        None
                    }
                    Action::Match(reverse) => {
                        if let Some(task) = &self.task {
                            task.search_match(!(reverse ^ self.search_reverse), count);
                        }
                        None
                    }
                };
                if let Some(navigation) = navigation {
                    self.command(navigation);
                }
                cx.notify();
                return;
            }
        }
        for (name, command) in [
            ("down", Navigation::Move(1)),
            ("up", Navigation::Move(-1)),
            ("left", Navigation::Left(1)),
            ("right", Navigation::Right(1)),
            ("pagedown", Navigation::Move(self.viewport_rows as i64)),
            ("pageup", Navigation::Move(-(self.viewport_rows as i64))),
            ("space", Navigation::Toggle),
        ] {
            if key == name {
                self.command(command);
                cx.notify();
            }
        }
        if m.control {
            let half = (self.viewport_rows / 2).max(1) as i64;
            match key {
                "d" => self.command(Navigation::Move(half)),
                "u" => self.command(Navigation::Move(-half)),
                "e" => self.command(Navigation::Move(self.viewport_rows as i64)),
                "b" => self.command(Navigation::Move(-(self.viewport_rows as i64))),
                "c" => self.output(None, false),
                "p" => self.output(None, true),
                "f" => self.focus_search = true,
                _ => (),
            }
        }
    }

    fn apply_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.sync_inputs) {
            let path = self.query.clone();
            let row = self.row.clone();
            let jsonpath = self.jsonpath.clone();
            let term = self.search_term.clone();
            self.path_input.update(cx, |input, cx| input.set_text(path, cx));
            self.row_input.update(cx, |input, cx| input.set_text(row, cx));
            self.jsonpath_input.update(cx, |input, cx| input.set_text(jsonpath, cx));
            self.search_input.update(cx, |input, cx| input.set_text(term, cx));
        }
        if std::mem::take(&mut self.focus_path) {
            let handle = self.path_input.read(cx).focus_handle(cx);
            window.focus(&handle);
        }
        if std::mem::take(&mut self.focus_search) {
            let handle = self.search_input.read(cx).focus_handle(cx);
            window.focus(&handle);
        }
        if std::mem::take(&mut self.pending_blur) {
            window.blur();
        }
    }

    fn update_viewport(&mut self, window: &Window, cx: &mut Context<Self>) {
        let rows = ((f32::from(window.viewport_size().height) - 250.0) / 24.0)
            .floor()
            .clamp(1.0, 64.0) as u64;
        if rows != self.viewport_rows {
            self.viewport_rows = rows;
            self.command(Navigation::ViewportRows(rows));
            cx.notify();
        }
    }

    fn toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let preset = self.colors.preset.next();
        div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .pt_2()
            .child(div().text_lg().font_weight(FontWeight::BOLD).text_color(rgb(0x60b4fa)).child("FerrisJson · Tree"))
            .child(ui::button("paged-open", "Open file", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| this.open_dialog(cx),
            )))
            .child(ui::button("paged-reload", "Reload / Resume", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| this.open(this.path.clone(), cx),
            )))
            .child(ui::button("paged-cancel", "Cancel", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    if let Some(task) = &this.task {
                        task.cancel();
                    }
                    this.status = SessionStatus::Cancelled;
                    this.frame = None;
                    cx.notify();
                },
            )))
            .child(ui::toggle("paged-dark", "Dark", self.colors.dark, palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.colors.dark = !this.colors.dark;
                    cx.notify();
                }),
            ))
            .child(ui::button("paged-preset", self.colors.preset.name(), palette).on_click(
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.colors.preset = preset;
                    cx.notify();
                }),
            ))
            .child(ui::button("paged-help", "Help", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.help = !this.help;
                    cx.notify();
                },
            )))
            .child(div().flex_1())
            .child(div().text_xs().whitespace_nowrap().overflow_hidden().child(self.path.display().to_string()))
            .into_any_element()
    }

    fn path_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mut row = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .py_1()
            .child(div().text_xs().child("Path"))
            .child(ui::field(&self.path_input, 260.0, palette))
            .child(ui::button("paged-go", "Go", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.command(Navigation::Locate(this.query.clone()));
                    cx.notify();
                },
            )))
            .child(div().text_xs().child("Row"))
            .child(ui::field(&self.row_input, 80.0, palette))
            .child(ui::button("paged-jump", "Jump", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    match this.row.parse::<u64>() {
                        Ok(n) if n > 0 => this.command(Navigation::Row(n - 1)),
                        _ => this.notice = "Enter a positive integer row number".into(),
                    }
                    cx.notify();
                },
            )));
        if self.status != SessionStatus::Ready {
            row = row.child(div().text_xs().text_color(rgb(0x64b4eb)).child(format!(
                "{:?} · {}: {} / {} bytes",
                self.status,
                self.kind.unwrap_or("root pending"),
                self.done,
                self.total_bytes
            )));
        }
        row.into_any_element()
    }

    fn jsonpath_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let count = self
            .query_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.state.as_ref())
            .map_or(0, |state| state.count);
        div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .pb_1()
            .child(div().text_xs().child("JSONPath"))
            .child(ui::button("paged-jsonpath-help", "Cheatsheet", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.vim.cancel();
                    this.jsonpath_help = true;
                    cx.notify();
                }),
            ))
            .child(ui::field(&self.jsonpath_input, 320.0, palette))
            .child(ui::button("paged-run-query", "Run query", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.start_query();
                    cx.notify();
                }),
            ))
            .child(ui::button("paged-clear-query", "Clear query", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.jsonpath.clear();
                    this.jsonpath_input.update(cx, |input, cx| input.set_text("", cx));
                    this.start_query();
                    cx.notify();
                }),
            ))
            .child(div().text_xs().child(format!(
                "{}/{count}",
                self.query_selected.map_or(0, |index| index + 1)
            )))
            .child(ui::button("paged-query-prev", "<", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.next_query_match(true);
                    cx.notify();
                },
            )))
            .child(ui::button("paged-query-next", ">", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.next_query_match(false);
                    cx.notify();
                },
            )))
            .into_any_element()
    }

    fn search_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .pb_1()
            .child(div().text_xs().child(if self.search_reverse { "Search ?" } else { "Search /" }))
            .child(ui::field(&self.search_input, 220.0, palette))
            .child(ui::toggle("paged-search-case", "Case", self.search_case, palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.search_case = !this.search_case;
                    this.search_changed(cx);
                    cx.notify();
                }),
            ))
            .child(ui::toggle("paged-search-regex", "Regex", self.search_regex, palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.search_regex = !this.search_regex;
                    this.search_changed(cx);
                    cx.notify();
                }),
            ))
            .child(ui::button("paged-search-scope", format!("{:?}", self.search_scope), palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.search_scope = match this.search_scope {
                        crate::document::SearchScope::All => crate::document::SearchScope::Keys,
                        crate::document::SearchScope::Keys => crate::document::SearchScope::Values,
                        crate::document::SearchScope::Values => crate::document::SearchScope::All,
                    };
                    this.search_changed(cx);
                    cx.notify();
                }),
            ))
            .child(ui::toggle("paged-live", "Live", self.colors.live_search, palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.colors.live_search = !this.colors.live_search;
                    this.search_changed(cx);
                    cx.notify();
                }),
            ))
            .child(ui::button("paged-search", "Search", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.start_search();
                    cx.notify();
                },
            )))
            .child(ui::button("paged-search-clear", "Clear", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.search_term.clear();
                    this.search_input.update(cx, |input, cx| input.set_text("", cx));
                    this.start_search();
                    cx.notify();
                },
            )))
            .child(ui::button("paged-search-stop", "Stop search", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    if let Some(task) = &this.task {
                        task.search_clear(true);
                    }
                    this.page_request = None;
                    cx.notify();
                }),
            ))
            .into_any_element()
    }

    fn actions_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .pb_1()
            .child(ui::button("paged-expand", "Expand all · zR", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.command(Navigation::ExpandAll);
                    cx.notify();
                }),
            ))
            .child(ui::button("paged-collapse", "Collapse all · zM", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.command(Navigation::CollapseAll);
                    cx.notify();
                }),
            ))
            .child(ui::button("paged-first", "First · gg", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.command(Navigation::First);
                    cx.notify();
                },
            )))
            .child(ui::button("paged-last", "Last · G", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.command(Navigation::Last);
                    cx.notify();
                },
            )))
            .into_any_element()
    }

    fn main_area(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut area = div().flex().flex_1().min_h(px(0.));
        if self
            .search
            .as_ref()
            .is_some_and(|snapshot| snapshot.state.is_some() || snapshot.waiting)
        {
            area = area.child(self.search_panel(cx));
        }
        area = area
            .child(self.tree_list(cx))
            .child(self.inspector(cx));
        area.into_any_element()
    }

    fn search_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mut panel = div()
            .w(px(260.0))
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .border_r_1()
            .border_color(color(palette.selection))
            .child(div().font_weight(FontWeight::BOLD).child("Search results"));
        if let Some(snapshot) = &self.search
            && let Some(state) = &snapshot.state
        {
            let status = state.status;
            let count = state.count;
            let scanned = state.scanned;
            let error = state.error.clone();
            let queue_peak = snapshot.queue_peak;
            let completed = status == crate::engine::search::Status::Completed;
            panel = panel
                .child(div().text_xs().child(format!(
                    "{:?} · {}{} matches · {} scanned · queue {}/4",
                    status,
                    if completed { "" } else { "≥" },
                    count,
                    scanned,
                    queue_peak
                )))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(ui::button("search-page-prev", "Prev", palette).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.search_start = this.search_start.saturating_sub(16);
                                this.page_request = None;
                                this.search_page = None;
                                cx.notify();
                            },
                        )))
                        .child(
                            ui::button("search-page-next", "Next", palette)
                                .opacity(if self.search_start.saturating_add(16) < count { 1.0 } else { 0.4 })
                                .when(self.search_start.saturating_add(16) < count, |button| {
                                    button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.search_start += 16;
                                        this.page_request = None;
                                        this.search_page = None;
                                        cx.notify();
                                    }))
                                }),
                        ),
                );
            if !error.is_empty() {
                panel = panel.child(div().text_xs().text_color(rgb(0xff8080)).child(error));
            }
        }
        let rows = self
            .search_page
            .as_ref()
            .map(|page| {
                page.rows
                    .iter()
                    .map(|row| (row.index, row.path.clone(), row.preview.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(page) = &self.search_page {
            panel = panel.child(div().text_xs().child(format!(
                "From {} · {} committed",
                page.start + 1,
                page.count
            )));
        }
        let cursor = self.search.as_ref().and_then(|snapshot| snapshot.cursor);
        let highlight = self.search_highlight.clone();
        let mut list = div().id("paged-search-results").flex().flex_col().flex_1().min_h(px(0.0)).overflow_y_scroll();
        for (index, path, preview) in rows {
            let selected = cursor == Some(index);
            let text = match &highlight {
                Some(highlight) => StyledText::new(format!("{} {path}", index + 1))
                    .with_runs(highlight.runs(&format!("{} {path}", index + 1), color(palette.punctuation), palette, true)),
                None => StyledText::new(format!("{} {path}", index + 1)),
            };
            list = list.child(
                div()
                    .id(("search-result", index as usize))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .p_1()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(color(palette.selection)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(task) = &this.task {
                            task.search_select(index);
                        }
                        cx.notify();
                    }))
                    .child(text)
                    .child(
                        div()
                            .text_xs()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(ui::json_text(&preview, palette, highlight.as_ref())),
                    ),
            );
        }
        panel = panel.child(list);
        let mut exports = div().flex().items_center().flex_wrap().gap_1();
        for (id, label, paths, partial) in [
            ("export-values", "All values", false, false),
            ("export-paths", "All paths", true, false),
            ("export-partial-values", "Partial values", false, true),
            ("export-partial-paths", "Partial paths", true, true),
        ] {
            exports = exports.child(ui::button(id, label, palette).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("JSON", &["json"])
                        .save_file()
                        && let Some(task) = &this.task
                        && let Err(error) = task.search_export(path, paths, partial)
                    {
                        this.notice = error;
                    }
                    cx.notify();
                },
            )));
        }
        panel.child(exports).into_any_element()
    }

    fn tree_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mut list = div()
            .id("tree-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(240.0))
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px_1();
        let Some(frame) = &self.frame else {
            return list
                .child(div().p_2().text_xs().child("Preparing snapshot / waiting for a complete safe page…"))
                .into_any_element();
        };
        let digits = frame.total.max(1).to_string().len();
        let selected_rank = frame.selected_rank;
        let total = frame.total;
        let exact = frame.exact;
        let visible = self.viewport_rows.max(1) as usize;
        let start = frame.start;
        let count = frame.rows.len();
        let rows: Vec<_> = frame
            .rows
            .iter()
            .take(visible)
            .map(|row| {
                (
                    row.rank,
                    row.expanded,
                    row.label.clone(),
                    row.preview.clone(),
                    row.location.clone(),
                )
            })
            .collect();
        let highlight = self.search_highlight.clone();
        for (rank, expanded, label, preview, location) in rows {
            let number = if rank == selected_rank {
                rank + 1
            } else {
                rank.abs_diff(selected_rank)
            };
            let container = location.node.container();
            let kind = location.node.kind;
            let depth = location.ancestors.len().min(32);
            let is_selected = location.node.reference == frame.selected.node.reference;
            let id = location.clone();
            let toggle_id = location.clone();
            let label_text = if let Some(highlight) = &highlight {
                StyledText::new(label.clone())
                    .with_runs(highlight.runs(&label, palette.kind(kind), palette, true))
            } else {
                ui::plain_text(&label, palette.kind(kind))
            };
            list = list.child(
                div()
                    .id(("paged-row", rank as usize))
                    .flex()
                    .items_center()
                    .h(px(24.0))
                    .w_full()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .when(is_selected, |row| row.bg(color(palette.selection)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.command(Navigation::Select(id.clone()));
                        window.blur();
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(digits as f32 * 8.0 + 12.0))
                            .text_xs()
                            .child(format!("{number:>width$}", width = digits)),
                    )
                    .child(div().w(px(depth as f32 * 12.0)).flex_none())
                    .child(if container {
                        ui::button(("paged-expander", rank as usize), if expanded { "▾" } else { "▸" }, palette)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.command(Navigation::Select(toggle_id.clone()));
                                this.command(Navigation::Toggle);
                                window.blur();
                                cx.notify();
                            }))
                            .into_any_element()
                    } else {
                        div().w(px(18.0)).into_any_element()
                    })
                    .child(label_text)
                    .child(div().text_color(color(palette.punctuation)).child("  "))
                    .child(div().flex_1().min_w(px(0.0)).overflow_hidden().child(ui::json_text(&preview, palette, highlight.as_ref()))),
            );
        }
        list = list.child(div().p_1().text_xs().child(format!(
            "Window {}..{}; {}{} visible rows",
            start + 1,
            start + count as u64,
            if exact { "" } else { "at least " },
            total
        )));
        list.into_any_element()
    }

    fn inspector(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        div()
            .w(px(420.0))
            .min_w(px(280.0))
            .flex()
            .flex_col()
            .h_full()
            .border_l_1()
            .border_color(color(palette.selection))
            .child(self.query_panel(cx))
            .child(div().h(px(1.0)).bg(color(palette.selection)))
            .child(self.preview_panel(cx))
            .into_any_element()
    }

    fn query_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .min_h(px(0.0))
            .child(div().font_weight(FontWeight::BOLD).child("JSONPath results"));
        let Some(snapshot) = &self.query_snapshot else {
            return panel
                .child(div().text_xs().child("Waiting for a committed page…"))
                .into_any_element();
        };
        if snapshot.expression.is_empty() {
            return panel
                .child(div().text_xs().child("Run a JSONPath query to inspect matching values."))
                .into_any_element();
        }
        let expression = snapshot.expression.clone();
        let waiting = snapshot.waiting;
        let elapsed = snapshot.elapsed;
        let error = snapshot.error.clone();
        let state = snapshot.state.clone();
        panel = panel.child(div().text_xs().font_family(ui::mono_family()).whitespace_nowrap().overflow_hidden().child(expression));
        if let Some(state) = &state {
            let completed = state.status == crate::engine::results::Status::Completed;
            let count = state.count;
            panel = panel.child(div().text_xs().child(format!(
                "{:?} · {}{} matches",
                state.status,
                if completed { "" } else { "≥" },
                count
            )));
            if !state.error.is_empty() {
                panel = panel.child(div().text_xs().text_color(rgb(0xff8080)).child(state.error.clone()));
            }
        } else if waiting {
            panel = panel.child(div().text_xs().child("Waiting for complete source validation / preparing query…"));
        }
        if !error.is_empty() {
            panel = panel.child(div().text_xs().text_color(rgb(0xff8080)).child(error));
        }
        if let Some(elapsed) = elapsed {
            panel = panel.child(div().text_xs().child(format!("{:.3} s", elapsed.as_secs_f64())));
        }
        let count = state.as_ref().map_or(0, |state| state.count);
        let complete = state
            .as_ref()
            .is_some_and(|state| state.status == crate::engine::results::Status::Completed);
        let scanning = waiting
            || state
                .as_ref()
                .is_some_and(|state| state.status == crate::engine::results::Status::Scanning);
        panel = panel.child(
            div()
                .flex()
                .items_center()
                .flex_wrap()
                .gap_1()
                .child(
                    ui::button("paged-query-cancel", "Cancel query", palette)
                        .opacity(if scanning { 1.0 } else { 0.4 })
                        .when(scanning, |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Some(task) = &this.task {
                                    task.query_clear(false);
                                }
                                this.query_page_request = None;
                                cx.notify();
                            }))
                        }),
                )
                .child(ui::button("paged-query-clear", "Clear query", palette).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.query_selected = None;
                        if let Some(task) = &this.task {
                            task.query_clear(true);
                        }
                        this.jsonpath.clear();
                        this.jsonpath_input.update(cx, |input, cx| input.set_text("", cx));
                        this.query_snapshot = None;
                        this.query_page = None;
                        this.query_page_error = None;
                        this.query_page_request = None;
                        cx.notify();
                    }),
                ))
                .child(
                    ui::button("paged-query-prev-page", "Previous page", palette)
                        .opacity(if self.query_start > 0 { 1.0 } else { 0.4 })
                        .when(self.query_start > 0, |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.query_start = this.query_start.saturating_sub(16);
                                this.query_page = None;
                                this.query_page_error = None;
                                this.query_page_request = None;
                                cx.notify();
                            }))
                        }),
                )
                .child(
                    ui::button("paged-query-next-page", "Next page", palette)
                        .opacity(if self.query_start.saturating_add(16) < count { 1.0 } else { 0.4 })
                        .when(self.query_start.saturating_add(16) < count, |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.query_start += 16;
                                this.query_page = None;
                                this.query_page_error = None;
                                this.query_page_request = None;
                                cx.notify();
                            }))
                        }),
                ),
        );
        if let Some(page) = &self.query_page {
            panel = panel.child(div().text_xs().child(format!(
                "{}–{} / {}{}",
                if page.rows.is_empty() { 0 } else { page.start + 1 },
                page.start + page.rows.len() as u64,
                if complete { "" } else { "≥" },
                count
            )));
            let rows: Vec<_> = page
                .rows
                .iter()
                .map(|row| (row.index, row.path.clone(), row.preview.clone()))
                .collect();
            let selected = self.query_selected;
            let mut values = div().id("paged-query-values").flex().flex_col().max_h(px(200.0)).overflow_y_scroll();
            for (index, path, preview) in rows {
                values = values.child(
                    div()
                        .id(("paged-query-row", index as usize))
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .p_1()
                        .cursor_pointer()
                        .when(selected == Some(index), |row| row.bg(color(palette.selection)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if let Some(task) = &this.task {
                                match task.query_select(index) {
                                    Ok(()) => this.query_selected = Some(index),
                                    Err(error) => this.notice = error,
                                }
                            }
                            cx.notify();
                        }))
                        .child(div().text_xs().font_family(ui::mono_family()).child(format!("{} {path}", index + 1)))
                        .child(div().text_xs().whitespace_nowrap().overflow_hidden().child(ui::json_text(&preview, palette, None))),
                );
            }
            panel = panel.child(values);
        } else if count > 0 && self.query_page_error.is_none() {
            panel = panel.child(div().text_xs().child("Loading result page…"));
        }
        if let Some(error) = &self.query_page_error {
            panel = panel.child(div().text_xs().text_color(rgb(0xff8080)).child(error.clone()));
        }
        if self.query_page_error.is_some() {
            panel = panel.child(ui::button("paged-query-retry", "Retry page", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.query_page_error = None;
                    this.query_page_request = None;
                    cx.notify();
                }),
            ));
        }
        panel
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        ui::button("paged-copy-all", "Copy all", palette)
                            .opacity(if complete { 1.0 } else { 0.4 })
                            .when(complete, |button| {
                                button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    if let Some(task) = &this.task
                                        && let Err(error) = task.query_output(None, None)
                                    {
                                        this.notice = error;
                                    }
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(
                        ui::button("paged-export-all", "Export all", palette)
                            .opacity(if complete { 1.0 } else { 0.4 })
                            .when(complete, |button| {
                                button.on_click(cx.listener(
                                    move |this, _: &ClickEvent, _, cx| {
                                        if let Some(path) = rfd::FileDialog::new()
                                            .add_filter("JSON", &["json"])
                                            .set_file_name("query-results.json")
                                            .save_file()
                                            && let Some(task) = &this.task
                                            && let Err(error) = task.query_output(Some(path), None)
                                        {
                                            this.notice = error;
                                        }
                                        cx.notify();
                                    },
                                ))
                            }),
                    )
                    .child(
                        ui::button("paged-copy-partial", "Copy partial", palette)
                            .opacity(if count > 0 && !complete { 1.0 } else { 0.4 })
                            .when(count > 0 && !complete, |button| {
                                button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    if let Some(task) = &this.task
                                        && let Err(error) = task.query_output(None, Some(count))
                                    {
                                        this.notice = error;
                                    }
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(
                        ui::button("paged-export-partial", "Export partial", palette)
                            .opacity(if count > 0 && !complete { 1.0 } else { 0.4 })
                            .when(count > 0 && !complete, |button| {
                                button.on_click(cx.listener(
                                    move |this, _: &ClickEvent, _, cx| {
                                        if let Some(path) = rfd::FileDialog::new()
                                            .add_filter("JSON", &["json"])
                                            .set_file_name("query-partial.json")
                                            .save_file()
                                            && let Some(task) = &this.task
                                            && let Err(error) = task.query_output(Some(path), Some(count as u64))
                                        {
                                            this.notice = error;
                                        }
                                        cx.notify();
                                    },
                                ))
                            }),
                    ),
            )
            .child(div().text_xs().child("Copy is limited to 8 MiB; use Export for larger results."))
            .into_any_element()
    }

    fn preview_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let wrap = self.colors.wrap_values;
        let header = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .child(div().font_weight(FontWeight::BOLD).child("Value preview"))
            .child(ui::toggle("paged-wrap-values", "Wrap", wrap, palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.colors.wrap_values = !this.colors.wrap_values;
                    cx.notify();
                }),
            ));
        let Some(frame) = &self.frame else {
            return div()
                .flex()
                .flex_col()
                .gap_1()
                .p_1()
                .child(header)
                .child(div().text_xs().child("Waiting for a committed page…"))
                .into_any_element();
        };
        let path = frame.selection.path.clone();
        let ready = self.status == SessionStatus::Ready && frame.exact;
        let mut preview = frame.selection.preview.clone();
        let mut truncated = false;
        if preview.len() > 16 * 1024 {
            let mut end = 16 * 1024;
            while !preview.is_char_boundary(end) {
                end -= 1;
            }
            preview.truncate(end);
            truncated = true;
        }
        let highlight = self.search_highlight.clone();
        let buttons = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .child(
                ui::button("paged-copy-json", "Copy JSON", palette)
                    .opacity(if ready { 1.0 } else { 0.4 })
                    .when(ready, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.output(None, false);
                            cx.notify();
                        }))
                    }),
            )
            .child(
                ui::button("paged-copy-path", "Copy path", palette)
                    .opacity(if ready { 1.0 } else { 0.4 })
                    .when(ready, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.output(None, true);
                            cx.notify();
                        }))
                    }),
            )
            .child(
                ui::button("paged-export", "Export", palette)
                    .opacity(if ready { 1.0 } else { 0.4 })
                    .when(ready, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("JSON", &["json"])
                                .save_file()
                            {
                                this.output(Some(path), false);
                            }
                            cx.notify();
                        }))
                    }),
            );
        let mut area = div()
            .id("paged-preview")
            .flex_1()
            .min_h(px(0.0))
            .p_1()
            .text_size(px(13.0));
        area = if wrap {
            area.overflow_y_scroll()
        } else {
            area.overflow_scroll().whitespace_nowrap()
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_1()
            .p_1()
            .child(header)
            .child(div().text_xs().whitespace_nowrap().overflow_hidden().child(path))
            .child(buttons)
            .child(area.child(ui::json_text(&preview, palette, highlight.as_ref())))
            .when(truncated, |column| {
                column.child(div().text_xs().child(
                    "Display limited to 16 KiB; complete copy and export use the full value.",
                ))
            })
            .into_any_element()
    }

    fn status_bar(&mut self, window: &Window, cx: &App) -> AnyElement {
        let palette = self.palette();
        let mut left = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .text_xs()
            .child(div().font_family(ui::mono_family()).child(format!("{:?}", self.status)));
        if let Some(frame) = &self.frame {
            left = left.child(div().font_family(ui::mono_family()).child(format!(
                "Ln {}/{}{} · {}",
                frame.selected_rank + 1,
                if frame.exact { "" } else { "≥" },
                frame.total,
                self.vim.display()
            )));
        }
        left = left
            .child(div().font_family(ui::mono_family()).text_color(if self.input_focused(window, cx) { rgb(0x64b4eb) } else { color(palette.punctuation) }).child(self.input_focused(window, cx).then_some("INPUT").unwrap_or("NORMAL")))
            .child(div().whitespace_nowrap().overflow_hidden().child(self.notice.clone()));
        if self.status == SessionStatus::Validating {
            left = left.child(div().text_xs().child(
                "Browsing committed content; final counts and complete output are pending validation.",
            ));
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
            .into_any_element()
    }

    fn overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut overlays = Vec::new();
        if self.help {
            let palette = self.palette();
            let mut body = div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xl().font_weight(FontWeight::BOLD).child("Paged tree shortcuts"));
            for line in [
                "hjkl / arrows: move, parent, expand or child; digits repeat",
                "gg / G: first / last; number + gg or G: exact integer row",
                "H M L: viewport top / middle / bottom",
                "PageUp / PageDown: page; Ctrl+U / D: half page; Ctrl+B / E: page",
                "za / zo / zc: toggle / open / close; zR / zM: expand / collapse all",
                "[ / ]: history; yy / Ctrl+C: copy JSON; yp / Ctrl+P: copy path",
                ".: focus Path (JSON Pointer, e.g. /items/0)",
                "⌘O (macOS) / Ctrl+O (Windows): open file; F5: reload; F1: help; Esc: cancel pending jump and key sequence",
                "Search: / ? or Ctrl+F; n / N with counts. JSONPath: Run query / Enter; F4 / Shift+F4: next / previous result (wraps).",
                "Line numbers: current visible row is absolute; other rows show distance from the cursor. Results have independent paging, cancel and output.",
                "During validation, only committed content is browsable; complete output waits for Ready.",
            ] {
                body = body.child(div().text_xs().child(line));
            }
            body = body.child(ui::button("paged-close-help", "Close", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.help = false;
                    cx.notify();
                }),
            ));
            overlays.push(ui::modal("paged-help-modal", palette, 640.0, body).into_any_element());
        }
        if self.jsonpath_help {
            let palette = self.palette();
            let mut body = div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xl().font_weight(FontWeight::BOLD).child("JSONPath cheatsheet"));
            let rows = [
                ("$", "根节点：选择整份 JSON。"),
                ("$.users", "选择根对象的 users 字段。"),
                ("$.users[*].name", "选取每个用户的 name 字段。"),
                ("$..email", "递归查找任意深度的 email 字段。"),
                ("$.users[?(@.active == true)]", "布尔值为真；true、false、null 不加引号。"),
                ("$.users[0:10]", "前 10 项；切片格式为 [起点:终点:步长]，不包含终点。"),
                ("/users/0/name", "JSON Pointer 也可用于 Path。"),
            ];
            for (index, (expression, explanation)) in rows.into_iter().enumerate() {
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .p_1()
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(color(palette.selection))
                        .child(div().flex_1().text_xs().font_family(ui::mono_family()).child(expression))
                        .child(ui::button(("paged-copy-path", index), "Copy", palette).on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(expression.to_string()));
                                this.notice = "Copied".into();
                                cx.notify();
                            }),
                        )),
                );
                body = body.child(div().text_xs().child(explanation));
            }
            body = body.child(ui::button("paged-close-jsonpath", "Close", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.jsonpath_help = false;
                    cx.notify();
                }),
            ));
            overlays.push(ui::modal("paged-jsonpath-modal", palette, 620.0, body).into_any_element());
        }
        overlays
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_focus(window, cx);
        self.tick(cx);
        self.apply_focus(window, cx);
        self.update_viewport(window, cx);
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
                    this.open(path.clone(), cx);
                }
            }))
            .child(self.toolbar(cx))
            .child(self.path_row(cx))
            .child(self.jsonpath_row(cx))
            .child(self.search_row(cx))
            .child(self.actions_row(cx))
            .child(self.main_area(cx))
            .child(self.status_bar(window, cx))
            .children(self.overlays(cx))
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        if let Some(retiring) = self.retiring.take() {
            crate::shutdown::defer(retiring);
        }
    }
}

/// Headless driver used by the benchmark examples. It keeps the same paged
/// task, navigation and polling code as the viewer but never creates a window,
/// so the measurements cover engine and paging CPU only.
struct Harness {
    task: Task,
    frame: Option<TreeFrame>,
    status: SessionStatus,
    epoch: u64,
    notice: String,
    search: Option<crate::search_worker::Snapshot>,
}

impl Harness {
    fn new(path: PathBuf) -> Result<Self, String> {
        Ok(Self {
            task: Task::new(
                path,
                Backend::Sparse(crate::engine::sparse::OpenOptions::default()),
                crate::repaint::Context::default(),
            )?,
            frame: None,
            status: SessionStatus::Preparing,
            epoch: 0,
            notice: String::new(),
            search: None,
        })
    }

    fn poll(&mut self) {
        let poll = self.task.poll();
        self.search = Some(self.task.search.poll());
        self.status = poll.status.state;
        if !poll.status.message.is_empty() {
            self.notice = poll.status.message;
        }
        if let Some((epoch, result)) = poll.frame
            && epoch == self.task.epoch.load(std::sync::atomic::Ordering::Relaxed)
        {
            match result {
                Ok(frame) => {
                    self.epoch = epoch;
                    self.frame = Some(frame);
                }
                Err(error) => self.notice = error,
            }
        }
        if matches!(
            self.status,
            SessionStatus::Failed | SessionStatus::Cancelled
        ) {
            self.frame = None;
        }
    }

    fn command(&mut self, command: Navigation) {
        if let Err(error) = self.task.command(command) {
            self.notice = error;
        }
    }

    fn wait(&mut self, predicate: &dyn Fn(&TreeFrame) -> bool, limit: Duration) -> Result<f64, String> {
        let start = Instant::now();
        loop {
            self.poll();
            let epoch = self.task.epoch.load(std::sync::atomic::Ordering::Relaxed);
            if self.epoch == epoch && self.frame.as_ref().is_some_and(predicate) {
                return Ok(start.elapsed().as_secs_f64() * 1000.0);
            }
            if start.elapsed() > limit {
                return Err(format!("Navigation deadline exceeded: {}", self.notice));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn close(mut self) -> Result<(), String> {
        let thread = self.task.retire();
        self.frame = None;
        thread.join().map_err(|_| "Tree task join failed".to_string())
    }
}

/// Development harness: measures engine, paging and polling CPU. It does not
/// open a window, so native rendering and GPU frame time are out of scope.
#[allow(
    dead_code,
    reason = "Called by the tree_build benchmark examples which reuse this renderer"
)]
pub fn benchmark_search(path: PathBuf, expected: u64) -> Result<serde_json::Value, String> {
    let mut harness = Harness::new(path.clone())?;
    let budget = harness.task.budget();
    let ready_ms = harness.wait(
        &|frame| frame.exact && frame.total > 1,
        Duration::from_secs(600),
    )?;
    let mut reports = Vec::new();
    for (term, matches) in [("NEEDLE", 3), ("x", expected)] {
        let query = crate::engine::search::Query {
            term: term.into(),
            case: true,
            regex: false,
            scope: crate::document::SearchScope::Values,
        };
        harness
            .task
            .search_query(query, false)
            .map_err(|error| error.to_string())?;
        let start = Instant::now();
        let mut first = None;
        let mut samples = Vec::new();
        loop {
            let tick = Instant::now();
            harness.poll();
            if samples.len() < 8192 {
                samples.push(tick.elapsed().as_secs_f64() * 1000.0);
            }
            if let Some(snapshot) = &harness.search
                && let Some(state) = &snapshot.state
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
                    let store = harness
                        .task
                        .search
                        .store()
                        .ok_or("Missing completed result store")?;
                    for hit in [0, matches / 2, matches - 1] {
                        let location = store.location(hit, &|| false)?;
                        let expected_ordinal = if term == "NEEDLE" {
                            [0, expected / 2, expected - 1][hit as usize]
                        } else {
                            hit
                        };
                        if location
                            .ancestors
                            .first()
                            .is_none_or(|edge| edge.ordinal != expected_ordinal)
                        {
                            return Err("First/middle/last result location mismatch".into());
                        }
                    }
                    samples.sort_by(f64::total_cmp);
                    let p95 = samples[(samples.len() - 1) * 95 / 100];
                    reports.push(serde_json::json!({
                        "term": term,
                        "matches": state.count,
                        "scanned": state.scanned,
                        "first_observed_ms": first,
                        "complete_observed_ms": start.elapsed().as_secs_f64() * 1000.0,
                        "poll_p95_ms": p95,
                        "poll_max_ms": samples.last(),
                        "poll_samples": samples.len(),
                        "queue_peak": snapshot.queue_peak,
                        "budget": budget.usage(),
                    }));
                    break;
                }
            }
            if start.elapsed() > Duration::from_secs(600) {
                return Err("Search benchmark timeout".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    harness.close()?;
    let after = budget.usage();
    if after.disk != 0 {
        return Err(format!("Search files leaked: {after:?}"));
    }
    Ok(serde_json::json!({
        "file": path.display().to_string(),
        "bytes": std::fs::metadata(&path).map_err(|error| error.to_string())?.len(),
        "ready_ms": ready_ms,
        "searches": reports,
        "after_retirement": after,
        "scope": "headless engine/paging/polling CPU; gpui rendering and native FPS not measured",
    }))
}

/// Development harness: navigation, expansion and polling CPU without a window.
#[allow(dead_code, reason = "Standalone CPU benchmark entry")]
pub fn benchmark_live(
    path: PathBuf,
    expected: u64,
    nested: bool,
) -> Result<serde_json::Value, String> {
    let mut harness = Harness::new(path)?;
    let budget = harness.task.budget();
    let started = Instant::now();
    let mut validating = Vec::new();
    let mut ready_samples = Vec::new();
    let mut first_page = None;
    loop {
        let frame_started = Instant::now();
        harness.poll();
        let elapsed = frame_started.elapsed().as_secs_f64() * 1000.0;
        if harness.status == SessionStatus::Failed {
            return Err(harness.notice.clone());
        }
        if harness.status == SessionStatus::Validating && validating.len() < 8192 {
            validating.push(elapsed);
        }
        if first_page.is_none()
            && harness.frame.as_ref().is_some_and(|frame| frame.rows.len() > 1)
        {
            first_page = Some(started.elapsed().as_secs_f64() * 1000.0);
        }
        if harness.status == SessionStatus::Ready
            && harness.frame.as_ref().is_some_and(|frame| frame.exact)
        {
            break;
        }
        if started.elapsed() > Duration::from_secs(1800) {
            return Err("Tree build deadline exceeded".into());
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    let ready_ms = started.elapsed().as_secs_f64() * 1000.0;
    if harness.frame.as_ref().unwrap().total != expected + 1 {
        return Err("Unexpected initial row count".into());
    }
    let mut probes = Vec::new();
    for ordinal in [0, expected / 2, expected - 1] {
        let pointer = if nested {
            format!("/{ordinal}/id")
        } else {
            format!("/{ordinal}")
        };
        let wanted = if nested {
            format!("$[{ordinal}][\"id\"]")
        } else {
            format!("$[{ordinal}]")
        };
        harness.command(Navigation::Locate(pointer));
        let elapsed = harness.wait(
            &|frame| frame.selection.path == wanted,
            Duration::from_secs(60),
        )?;
        let text = &harness.frame.as_ref().unwrap().selection.preview;
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
                    .all(|byte| *byte == b'x')
            {
                return Err("Scalar full-value probe mismatch".into());
            }
        }
        probes.push(serde_json::json!({"ordinal": ordinal, "locate_ms": elapsed}));
    }
    harness.command(Navigation::ExpandAll);
    let full_rows = expected * (if nested { 8 } else { 1 }) + 1;
    let expand_ms = harness.wait(
        &|frame| frame.total == full_rows,
        Duration::from_secs(60),
    )?;
    harness.command(Navigation::Last);
    let end_ms = harness.wait(
        &|frame| frame.selected_rank == full_rows - 1,
        Duration::from_secs(60),
    )?;
    let last_path = &harness.frame.as_ref().unwrap().selection.path;
    let wanted = if nested {
        format!("$[{}][\"payload\"]", expected - 1)
    } else {
        format!("$[{}]", expected - 1)
    };
    if last_path != &wanted {
        return Err("Full expansion end path mismatch".into());
    }
    harness.command(Navigation::CollapseAll);
    let collapse_ms = harness.wait(&|frame| frame.total == 1, Duration::from_secs(60))?;
    harness.command(Navigation::ExpandAll);
    harness.wait(&|frame| frame.total == full_rows, Duration::from_secs(60))?;
    let mut interactions = Vec::with_capacity(120);
    for operation in 0..120 {
        let start = Instant::now();
        let command = match operation % 12 {
            0 => Navigation::CollapseAll,
            1 => Navigation::ExpandAll,
            2 => Navigation::Last,
            3 => Navigation::First,
            _ => Navigation::Anchor(
                ((operation as u64 * 7919) % full_rows.max(1)).min(full_rows - 1),
            ),
        };
        harness.command(command);
        harness.wait(&|_| true, Duration::from_secs(60))?;
        interactions.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    for frame_number in 0..600 {
        if frame_number % 20 == 0 {
            let anchor = ((frame_number as u64 / 20) * (full_rows / 30)).min(full_rows - 1);
            harness.command(Navigation::Anchor(anchor));
        }
        let before = Instant::now();
        harness.poll();
        ready_samples.push(before.elapsed().as_secs_f64() * 1000.0);
        if harness.frame.as_ref().is_some_and(|frame| frame.rows.len() > 64) {
            return Err("UI window exceeded 64 rows".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    fn stats(mut values: Vec<f64>) -> serde_json::Value {
        values.sort_by(f64::total_cmp);
        if values.is_empty() {
            return serde_json::json!({"samples": 0});
        }
        serde_json::json!({
            "samples": values.len(),
            "p50_ms": values[values.len() / 2],
            "p95_ms": values[(values.len() - 1) * 95 / 100],
            "max_ms": values.last().unwrap(),
        })
    }
    let result = serde_json::json!({
        "package_version": env!("CARGO_PKG_VERSION"),
        "first_page_ms": first_page,
        "ready_ms": ready_ms,
        "full_rows": full_rows,
        "expand_ms": expand_ms,
        "collapse_ms": collapse_ms,
        "last_ms": end_ms,
        "probes": probes,
        "validating_poll_samples": stats(validating),
        "ready_poll_samples": stats(ready_samples),
        "interaction_latency": stats(interactions),
        "managed": budget.usage(),
        "ui_window_max": 64,
        "scope": "headless engine/paging CPU, with actual background building; gpui rendering and native FPS unverified",
    });
    harness.close()?;
    crate::shutdown::finish();
    if budget.usage().memory != 0 || budget.usage().disk != 0 {
        return Err("Tree task retained budget after close".into());
    }
    Ok(result)
}
