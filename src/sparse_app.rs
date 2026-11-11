//! S4 finite-page browser. No complete rows/expanded arrays or DOM fallback.
//! Rendered with gpui.
use crate::engine::{
    budget::{BudgetManager, Limits, Resource},
    session::{SessionId, SessionStatus},
    sparse::{CopiedValue, Event, Node, OpenOptions, PublishedPage, open_progressive},
};
use crate::palette::{color, Palette};
use crate::ui::{self, InputEvent};
use gpui::prelude::*;
use gpui::*;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::JoinHandle,
    time::Duration,
};

enum Request {
    Page(Node, u64),
    Locate(String),
    Export(Node, PathBuf),
    Copy(Node),
}
enum Message {
    Event(Event),
    Root(u64, Node),
    Page(u64, Node, u64, PublishedPage),
    Error(u64, String),
    Exported(u64, PathBuf),
    Copied(u64, CopiedValue),
}
struct Worker {
    cancelled: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    requests: SyncSender<Request>,
    messages: Receiver<Message>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            crate::shutdown::defer(thread);
        }
    }
}
fn send(
    tx: &SyncSender<Message>,
    mut message: Message,
    cancelled: &AtomicBool,
    wake: &crate::repaint::Context,
) -> Result<(), String> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Sparse viewer cancelled".into());
        }
        match tx.try_send(message) {
            Ok(()) => {
                wake.request_repaint();
                return Ok(());
            }
            Err(mpsc::TrySendError::Full(m)) => {
                message = m;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return Err("Sparse viewer closed".into()),
        }
    }
}
impl Worker {
    fn new(
        path: PathBuf,
        mut options: OpenOptions,
        generation: u64,
        wake: crate::repaint::Context,
        previous: Option<JoinHandle<()>>,
    ) -> Self {
        let progress = Arc::new(AtomicU64::new(0));
        options.progress = Some(progress.clone());
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let (tx, messages) = mpsc::sync_channel(4);
        let (requests, rx) = mpsc::sync_channel(4);
        let thread = std::thread::spawn(move || {
            if let Some(previous) = previous {
                let _ = previous.join();
            }
            let budget = BudgetManager::new(Limits {
                memory: 256 * 1024 * 1024,
                disk: 64usize.saturating_mul(1024 * 1024 * 1024),
            });
            let _queues = match budget.reserve(
                Resource::Memory,
                64 * 1024,
                "sparse GUI request/message metadata",
            ) {
                Ok(lease) => lease,
                Err(_) => return,
            };
            let stopped = || flag.load(Ordering::Relaxed);
            let result = open_progressive(
                &path,
                budget.clone(),
                options,
                generation,
                &stopped,
                &mut |event| send(&tx, Message::Event(event), &flag, &wake),
            );
            let mut index = match result {
                Ok(index) => index,
                Err(error) => {
                    let _ = send(&tx, Message::Error(generation, error), &flag, &wake);
                    return;
                }
            };
            if send(&tx, Message::Root(generation, index.root()), &flag, &wake).is_err() {
                return;
            }
            while !stopped() {
                let request = match rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(request) => request,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                let result = match request {
                    Request::Page(parent, start) => index
                        .published_children(parent, start, &stopped)
                        .map(|page| Message::Page(generation, parent, start, page)),
                    Request::Locate(pointer) => index.pointer(&pointer, &stopped).and_then(|node| {
                        if node.kind().name() == "object" || node.kind().name() == "array" {
                            index.published_children(node, 0, &stopped)
                        } else {
                            index.published_node(node, &stopped)
                        }
                        .map(|page| Message::Page(generation, node, 0, page))
                    }),
                    Request::Export(node, path) => index
                        .export_value(node, &path, &flag)
                        .map(|()| Message::Exported(generation, path)),
                    Request::Copy(node) => index
                        .copy_value(node, &stopped)
                        .map(|value| Message::Copied(generation, value)),
                };
                let message = result.unwrap_or_else(|error| Message::Error(generation, error));
                if send(&tx, message, &flag, &wake).is_err() {
                    break;
                }
            }
        });
        Self {
            cancelled,
            progress,
            requests,
            messages,
            thread: Some(thread),
        }
    }
}

pub struct Viewer {
    worker: Option<Worker>,
    options: OpenOptions,
    generation: u64,
    session: Option<SessionId>,
    status: SessionStatus,
    source: PathBuf,
    kind: String,
    scanned: u64,
    total_bytes: u64,
    page: Option<PublishedPage>,
    parent: Option<Node>,
    start: u64,
    selected: Option<Node>,
    pointer: String,
    notice: String,
    busy: bool,
    clipboard: Option<CopiedValue>,
    colors: crate::palette::ColorSettings,
    wake: crate::repaint::Context,
    pointer_input: Entity<ui::TextInput>,
    pointer_sub: Option<Subscription>,
    pending_blur: bool,
}

impl Viewer {
    pub fn new(
        _window: &mut Window,
        cx: &mut Context<Self>,
        path: PathBuf,
        options: OpenOptions,
    ) -> Self {
        let (wake_tx, wake_rx) = async_channel::unbounded::<()>();
        let wake = crate::repaint::Context::with_notify(Arc::new(move || {
            let _ = wake_tx.try_send(());
        }));
        let pointer_input = cx.new(|cx| ui::TextInput::new("JSON Pointer, e.g. /items/0", 4096, cx));
        let mut viewer = Self {
            worker: None,
            options,
            generation: 0,
            session: None,
            status: SessionStatus::Preparing,
            source: path.clone(),
            kind: String::new(),
            scanned: 0,
            total_bytes: 0,
            page: None,
            parent: None,
            start: 0,
            selected: None,
            pointer: String::new(),
            notice: String::new(),
            busy: false,
            clipboard: None,
            colors: crate::palette::ColorSettings::default(),
            wake,
            pointer_input: pointer_input.clone(),
            pointer_sub: None,
            pending_blur: false,
        };
        viewer.pointer_sub = Some(cx.subscribe(&pointer_input, |this: &mut Self, _input, event: &InputEvent, cx| {
            match event {
                InputEvent::Changed(text) => this.pointer = text.to_string(),
                InputEvent::Submitted => {
                    this.request(Request::Locate(this.pointer.clone()));
                    this.pending_blur = true;
                }
                InputEvent::Escape => this.pending_blur = true,
            }
            cx.notify();
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
        viewer.colors = crate::palette::ColorSettings::load().unwrap_or_default();
        viewer.open(path, cx);
        viewer
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let messages: Vec<_> = self
            .worker
            .as_ref()
            .map(|worker| worker.messages.try_iter().take(4).collect())
            .unwrap_or_default();
        for message in messages {
            self.accept(message, cx);
        }
        if self.status == SessionStatus::Validating
            && let Some(worker) = &self.worker
        {
            self.scanned = self.scanned.max(worker.progress.load(Ordering::Relaxed));
        }
        if let Some(mut value) = self.clipboard.take() {
            cx.write_to_clipboard(ClipboardItem::new_string(std::mem::take(&mut value.text)));
            self.notice = "Copied complete source JSON value".into();
        }
    }

    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() {
            self.open(path, cx);
        }
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let previous = self.worker.take().and_then(|mut worker| {
            worker.cancelled.store(true, Ordering::Relaxed);
            worker.thread.take()
        });
        self.generation += 1;
        self.session = None;
        self.status = SessionStatus::Preparing;
        self.page = None;
        self.parent = None;
        self.selected = None;
        self.start = 0;
        self.scanned = 0;
        self.total_bytes = 0;
        self.kind.clear();
        self.notice.clear();
        self.busy = false;
        self.clipboard = None;
        self.source = path.clone();
        self.worker = Some(Worker::new(
            path,
            self.options.clone(),
            self.generation,
            self.wake.clone(),
            previous,
        ));
        cx.notify();
    }

    fn request(&mut self, request: Request) {
        if self.status != SessionStatus::Ready || self.busy {
            return;
        }
        if let Some(worker) = &self.worker {
            match worker.requests.try_send(request) {
                Ok(()) => self.busy = true,
                Err(_) => self.notice = "Request queue busy; try again".into(),
            }
        }
    }

    fn accept(&mut self, message: Message, cx: &mut Context<Self>) {
        match message {
            Message::Event(event) if event.generation == self.generation => {
                if self.session.is_some_and(|session| session != event.session)
                    || matches!(
                        self.status,
                        SessionStatus::Cancelled | SessionStatus::Failed
                    )
                {
                    return;
                }
                self.session = Some(event.session);
                self.status = event.status;
                self.scanned = event.scanned;
                self.total_bytes = event.total_bytes;
                if let Some(kind) = event.root_kind {
                    self.kind = kind.name().into();
                }
                if let Some(page) = event.page {
                    self.page = Some(page);
                    self.start = 0;
                }
                if let Some(message) = event.message {
                    self.notice = message;
                }
                if event.reused {
                    self.notice = "Strict cache reuse: complete source equality checked".into();
                } else if event.resumed {
                    self.notice = "Resumed from committed parser checkpoint".into();
                }
            }
            Message::Page(generation, parent, start, page) if generation == self.generation => {
                self.parent = Some(parent);
                self.selected = None;
                self.start = start;
                self.page = Some(page);
                self.busy = false;
            }
            Message::Root(generation, node) if generation == self.generation => {
                self.parent = Some(node);
            }
            Message::Error(generation, error) if generation == self.generation => {
                self.notice = error;
                self.busy = false;
                if !matches!(self.status, SessionStatus::Ready | SessionStatus::Cancelled) {
                    self.status = SessionStatus::Failed;
                }
            }
            Message::Copied(generation, value)
                if generation == self.generation && self.status == SessionStatus::Ready =>
            {
                self.clipboard = Some(value);
                self.busy = false;
            }
            Message::Exported(generation, path) if generation == self.generation => {
                self.notice = format!("Exported {}", path.display());
                self.busy = false;
            }
            _ => (),
        }
        cx.notify();
    }

    fn palette(&self) -> Palette {
        self.colors.active()
    }

    fn toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .pt_2()
            .child(div().text_lg().font_weight(FontWeight::BOLD).text_color(rgb(0x60b4fa)).child("FerrisJson · Progressive"))
            .child(ui::button("sparse-open", "Open file", palette).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| this.open_dialog(cx),
            )))
            .child(ui::button("sparse-reload", "Reload / Resume", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| this.open(this.source.clone(), cx)),
            ))
            .child(ui::button("sparse-cancel", "Cancel", palette).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    if let Some(worker) = &this.worker {
                        worker.cancelled.store(true, Ordering::Relaxed);
                    }
                    this.status = SessionStatus::Cancelled;
                    this.busy = false;
                    cx.notify();
                }),
            ))
            .child(div().flex_1())
            .child(div().text_xs().whitespace_nowrap().overflow_hidden().child(self.source.display().to_string()))
            .into_any_element()
    }

    fn status_row(&mut self) -> AnyElement {
        let palette = self.palette();
        let ratio = if self.total_bytes == 0 {
            0.0
        } else {
            (self.scanned as f32 / self.total_bytes as f32).clamp(0.0, 1.0)
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .pb_1()
            .child(div().text_xs().child(format!(
                "{:?} · {} · scanned {} / {} bytes",
                self.status, self.kind, self.scanned, self.total_bytes
            )))
            .child(
                div()
                    .h(px(6.0))
                    .w_full()
                    .bg(color(palette.stripe))
                    .child(
                        div()
                            .h_full()
                            .w(relative(ratio))
                            .bg(color(palette.selection)),
                    ),
            )
            .into_any_element()
    }

    fn pointer_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let ready = self.status == SessionStatus::Ready && !self.busy;
        div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .py_1()
            .child(div().text_xs().child("JSON Pointer"))
            .child(ui::field(&self.pointer_input, 320.0, palette))
            .child(
                ui::button("sparse-go", "Go", palette)
                    .opacity(if ready { 1.0 } else { 0.4 })
                    .when(ready, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.request(Request::Locate(this.pointer.clone()));
                            cx.notify();
                        }))
                    }),
            )
            .child(
                ui::button("sparse-root", "Root", palette)
                    .opacity(if ready { 1.0 } else { 0.4 })
                    .when(ready, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.pointer.clear();
                            this.pointer_input.update(cx, |input, cx| input.set_text("", cx));
                            this.request(Request::Locate(String::new()));
                            cx.notify();
                        }))
                    }),
            )
            .into_any_element()
    }

    fn page_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let ready = self.status == SessionStatus::Ready && !self.busy;
        let page = self.page.as_ref().map(|page| {
            (
                page.total,
                page.next,
                page.nodes.clone(),
                page.labels.clone(),
                page.previews.clone(),
            )
        });
        let mut column = div()
            .id("sparse-page")
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap_1()
            .overflow_y_scroll()
            .px_2();
        let Some((total, next, nodes, labels, previews)) = page else {
            return column
                .child(div().text_xs().child("Preparing stable snapshot / waiting for a complete safe page…"))
                .into_any_element();
        };
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_xs().child(total.map_or_else(
                    || format!("At least {} completed children", nodes.len()),
                    |total| format!("{total} children"),
                )))
                .child(
                    ui::button("sparse-prev", "Previous page", palette)
                        .opacity(if ready && self.parent.is_some() && self.start > 0 { 1.0 } else { 0.4 })
                        .when(ready && self.parent.is_some() && self.start > 0, |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Some(node) = this.parent {
                                    this.request(Request::Page(node, this.start.saturating_sub(64)));
                                }
                                cx.notify();
                            }))
                        }),
                )
                .child(
                    ui::button("sparse-next", "Next page", palette)
                        .opacity(if ready && next.is_some() && self.parent.is_some() { 1.0 } else { 0.4 })
                        .when(ready && next.is_some() && self.parent.is_some(), |button| {
                            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if let Some(node) = this.parent {
                                    this.request(Request::Page(node, next.unwrap_or(0)));
                                }
                                cx.notify();
                            }))
                        }),
                ),
        );
        for (index, node) in nodes.iter().enumerate() {
            let selected = self.selected.is_some_and(|selected| selected.start() == node.start());
            let label = labels.get(index).cloned().unwrap_or_default();
            let preview = previews.get(index).cloned().unwrap_or_default();
            let container = matches!(node.kind().name(), "array" | "object");
            let node = *node;
            column = column.child(
                div()
                    .id(("sparse-node", index))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1()
                    .rounded(px(4.0))
                    .when(selected, |row| row.bg(color(palette.selection)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id(("sparse-row", index))
                                    .flex_1()
                                    .font_family(ui::mono_family())
                                    .text_xs()
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.selected = Some(node);
                                        cx.notify();
                                    }))
                                    .child(format!(
                                        "{} · {} · {}..{}",
                                        label,
                                        node.kind().name(),
                                        node.start(),
                                        node.end()
                                    )),
                            )
                            .child(
                                ui::button(("sparse-browse", index), "Browse", palette)
                                    .opacity(if ready && container { 1.0 } else { 0.4 })
                                    .when(ready && container, |button| {
                                        button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            this.request(Request::Page(node, 0));
                                            cx.notify();
                                        }))
                                    }),
                            ),
                    )
                    .child(div().text_xs().whitespace_nowrap().overflow_hidden().child(ui::json_text(&preview, palette, None))),
            );
        }
        column = column.child(
            div()
                .flex()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(
                    ui::button("sparse-copy", "Copy selected complete JSON (up to 8 MiB)", palette)
                        .opacity(if ready && self.selected.is_some() { 1.0 } else { 0.4 })
                        .when(ready && self.selected.is_some(), |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Some(node) = this.selected {
                                    this.request(Request::Copy(node));
                                }
                                cx.notify();
                            }))
                        }),
                )
                .child(
                    ui::button("sparse-export-container", "Export current container / value", palette)
                        .opacity(if ready && self.parent.is_some() { 1.0 } else { 0.4 })
                        .when(ready && self.parent.is_some(), |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Some(node) = this.parent
                                    && let Some(path) = rfd::FileDialog::new()
                                        .add_filter("JSON", &["json"])
                                        .save_file()
                                {
                                    this.request(Request::Export(node, path));
                                }
                                cx.notify();
                            }))
                        }),
                )
                .child(
                    ui::button("sparse-export-value", "Export selected complete value", palette)
                        .opacity(if ready && self.selected.is_some() { 1.0 } else { 0.4 })
                        .when(ready && self.selected.is_some(), |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Some(node) = this.selected
                                    && let Some(path) = rfd::FileDialog::new()
                                        .add_filter("JSON", &["json"])
                                        .save_file()
                                {
                                    this.request(Request::Export(node, path));
                                }
                                cx.notify();
                            }))
                        }),
                ),
        );
        column.into_any_element()
    }

    fn footer(&mut self) -> AnyElement {
        let palette = self.palette();
        let mut footer = div().flex().flex_col().gap_1().px_2().py_1().bg(color(palette.stripe));
        if self.status == SessionStatus::Validating {
            footer = footer.child(div().text_xs().child(
                "Pending full validation. Completed array values are provisional document content; totals and complete export wait for Ready.",
            ));
        }
        if matches!(self.status, SessionStatus::Failed | SessionStatus::Cancelled) {
            footer = footer.child(div().text_xs().text_color(rgb(0xff8080)).child(
                "This document has not completed validation. Navigation and export are disabled.",
            ));
        }
        footer
            .child(div().text_xs().child(self.notice.clone()))
            .child(div().text_xs().child(
                "Legacy finite-page browser. Use --sparse for the paged tree with independent search and JSONPath queries.",
            ))
            .into_any_element()
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.pending_blur) {
            window.blur();
        }
        self.tick(cx);
        let palette = self.palette();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(color(palette.background))
            .text_color(color(palette.punctuation))
            .text_size(px(13.0))
            .child(self.toolbar(cx))
            .child(self.status_row())
            .child(self.pointer_row(cx))
            .child(self.page_view(cx))
            .child(self.footer())
    }
}
