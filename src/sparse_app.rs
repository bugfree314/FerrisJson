//! S4 finite-page browser. No complete rows/expanded arrays or DOM fallback.
use crate::engine::{
    budget::{BudgetManager, Limits, Resource},
    session::{SessionId, SessionStatus},
    sparse::{CopiedValue, Event, Node, OpenOptions, PublishedPage, open_progressive},
};
use eframe::egui;
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
        if let Some(t) = self.thread.take() {
            crate::shutdown::defer(t);
        }
    }
}
fn send(
    tx: &SyncSender<Message>,
    mut message: Message,
    cancelled: &AtomicBool,
    ctx: &egui::Context,
) -> Result<(), String> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Sparse viewer cancelled".into());
        }
        match tx.try_send(message) {
            Ok(()) => {
                ctx.request_repaint();
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
        ctx: egui::Context,
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
                Ok(r) => r,
                Err(_) => return,
            };
            let stopped = || flag.load(Ordering::Relaxed);
            let result = open_progressive(
                &path,
                budget.clone(),
                options,
                generation,
                &stopped,
                &mut |e| send(&tx, Message::Event(e), &flag, &ctx),
            );
            let mut index = match result {
                Ok(i) => i,
                Err(e) => {
                    let _ = send(&tx, Message::Error(generation, e), &flag, &ctx);
                    return;
                }
            };
            if send(&tx, Message::Root(generation, index.root()), &flag, &ctx).is_err() {
                return;
            }
            while !stopped() {
                let request = match rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(r) => r,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                let result = match request {
                    Request::Page(parent, start) => index
                        .published_children(parent, start, &stopped)
                        .map(|p| Message::Page(generation, parent, start, p)),
                    Request::Locate(pointer) => {
                        index.pointer(&pointer, &stopped).and_then(|node| {
                            if node.kind().name() == "object" || node.kind().name() == "array" {
                                index.published_children(node, 0, &stopped)
                            } else {
                                index.published_node(node, &stopped)
                            }
                            .map(|page| Message::Page(generation, node, 0, page))
                        })
                    }
                    Request::Export(node, path) => index
                        .export_value(node, &path, &flag)
                        .map(|()| Message::Exported(generation, path)),
                    Request::Copy(node) => index
                        .copy_value(node, &stopped)
                        .map(|v| Message::Copied(generation, v)),
                };
                let message = result.unwrap_or_else(|e| Message::Error(generation, e));
                if send(&tx, message, &flag, &ctx).is_err() {
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
}
impl Viewer {
    pub fn new(cc: &eframe::CreationContext<'_>, path: PathBuf, options: OpenOptions) -> Self {
        cc.egui_ctx.set_fonts(crate::fonts::definitions());
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
        };
        viewer.open(path, &cc.egui_ctx);
        viewer
    }
    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        let previous = self.worker.take().and_then(|mut w| {
            w.cancelled.store(true, Ordering::Relaxed);
            w.thread.take()
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
            ctx.clone(),
            previous,
        ));
    }
    fn request(&mut self, request: Request) {
        if self.status != SessionStatus::Ready || self.busy {
            return;
        }
        if let Some(w) = &self.worker {
            match w.requests.try_send(request) {
                Ok(()) => self.busy = true,
                Err(_) => self.notice = "Request queue busy; try again".into(),
            }
        }
    }
    fn accept(&mut self, message: Message) {
        match message {
            Message::Event(e) if e.generation == self.generation => {
                if self.session.is_some_and(|s| s != e.session)
                    || matches!(
                        self.status,
                        SessionStatus::Cancelled | SessionStatus::Failed
                    )
                {
                    return;
                }
                self.session = Some(e.session);
                self.status = e.status;
                self.scanned = e.scanned;
                self.total_bytes = e.total_bytes;
                if let Some(k) = e.root_kind {
                    self.kind = k.name().into();
                }
                if let Some(p) = e.page {
                    self.page = Some(p);
                    self.start = 0;
                }
                if let Some(m) = e.message {
                    self.notice = m;
                }
                if e.reused {
                    self.notice = "Strict cache reuse: complete source equality checked".into();
                } else if e.resumed {
                    self.notice = "Resumed from committed parser checkpoint".into();
                }
            }
            Message::Page(g, parent, start, page) if g == self.generation => {
                self.parent = Some(parent);
                self.selected = None;
                self.start = start;
                self.page = Some(page);
                self.busy = false;
            }
            Message::Root(g, node) if g == self.generation => {
                self.parent = Some(node);
            }
            Message::Error(g, e) if g == self.generation => {
                self.notice = e;
                self.busy = false;
                if !matches!(self.status, SessionStatus::Ready | SessionStatus::Cancelled) {
                    self.status = SessionStatus::Failed;
                }
            }
            Message::Copied(g, value)
                if g == self.generation && self.status == SessionStatus::Ready =>
            {
                self.clipboard = Some(value);
                self.busy = false;
            }
            Message::Exported(g, p) if g == self.generation => {
                self.notice = format!("Exported {}", p.display());
                self.busy = false;
            }
            _ => (),
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
        let messages: Vec<_> = self
            .worker
            .as_ref()
            .map(|w| w.messages.try_iter().take(4).collect())
            .unwrap_or_default();
        for m in messages {
            self.accept(m);
        }
        if self.status == SessionStatus::Validating
            && let Some(w) = &self.worker
        {
            self.scanned = self.scanned.max(w.progress.load(Ordering::Relaxed));
        }
        if let Some(mut value) = self.clipboard.take() {
            ctx.copy_text(std::mem::take(&mut value.text));
            self.notice = "Copied complete source JSON value".into();
        }
        ctx.request_repaint_after(Duration::from_millis(25));
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()))
        {
            self.open(path, ctx);
        }
        egui::TopBottomPanel::top("sparse_toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("FerrisJson · Progressive");
                if ui.button("Open file").clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter("JSON", &["json"])
                        .pick_file()
                {
                    self.open(p, ctx);
                }
                if ui.button("Reload / Resume").clicked() {
                    self.open(self.source.clone(), ctx);
                }
                if ui.button("Cancel").clicked() {
                    if let Some(w) = &self.worker {
                        w.cancelled.store(true, Ordering::Relaxed);
                    }
                    self.status = SessionStatus::Cancelled;
                    self.busy = false;
                }
            });
            ui.add(egui::Label::new(self.source.display().to_string()).truncate());
            ui.label(format!(
                "{:?} · {} · scanned {} / {} bytes",
                self.status, self.kind, self.scanned, self.total_bytes
            ));
            if self.total_bytes != 0 {
                ui.add(egui::ProgressBar::new(
                    self.scanned as f32 / self.total_bytes as f32,
                ));
            }
            ui.horizontal(|ui| {
                ui.label("JSON Pointer");
                ui.add(egui::TextEdit::singleline(&mut self.pointer).char_limit(4096));
                if ui
                    .add_enabled(
                        self.status == SessionStatus::Ready && !self.busy,
                        egui::Button::new("Go"),
                    )
                    .clicked()
                {
                    self.request(Request::Locate(self.pointer.clone()));
                }
                if ui
                    .add_enabled(
                        self.status == SessionStatus::Ready && !self.busy,
                        egui::Button::new("Root"),
                    )
                    .clicked()
                {
                    self.pointer.clear();
                    self.request(Request::Locate(String::new()));
                }
            });
        });
        egui::TopBottomPanel::bottom("sparse_status").show(ctx,|ui|{
            if self.status==SessionStatus::Validating{ui.label("Pending full validation. Completed array values are provisional document content; totals and complete export wait for Ready.");}
            if matches!(self.status,SessionStatus::Failed|SessionStatus::Cancelled){ui.colored_label(egui::Color32::LIGHT_RED,"This document has not completed validation. Navigation and export are disabled.");}
            ui.label(&self.notice);
            ui.label("Legacy finite-page browser. Use --sparse for the paged tree with independent search and JSONPath queries.");
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut enter = None;
            let mut previous = false;
            let mut next = None;
            if let Some(page) = &self.page {
                ui.horizontal(|ui| {
                    ui.label(page.total.map_or_else(
                        || format!("At least {} completed children", page.nodes.len()),
                        |n| format!("{n} children"),
                    ));
                    previous = ui
                        .add_enabled(
                            self.status == SessionStatus::Ready
                                && self.parent.is_some()
                                && self.start > 0
                                && !self.busy,
                            egui::Button::new("Previous page"),
                        )
                        .clicked();
                    if ui
                        .add_enabled(
                            self.status == SessionStatus::Ready
                                && !self.busy
                                && page.next.is_some()
                                && self.parent.is_some(),
                            egui::Button::new("Next page"),
                        )
                        .clicked()
                    {
                        next = page.next;
                    }
                });
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, node) in page.nodes.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(
                                    self.selected.is_some_and(|n| n.start() == node.start()),
                                    format!(
                                        "{} · {} · {}..{}",
                                        page.labels[i],
                                        node.kind().name(),
                                        node.start(),
                                        node.end()
                                    ),
                                )
                                .clicked()
                            {
                                self.selected = Some(*node);
                            }
                            if ui
                                .add_enabled(
                                    self.status == SessionStatus::Ready
                                        && !self.busy
                                        && matches!(node.kind().name(), "array" | "object"),
                                    egui::Button::new("Browse"),
                                )
                                .clicked()
                            {
                                enter = Some(*node);
                            }
                        });
                        ui.monospace(&page.previews[i]);
                        ui.separator();
                    }
                });
            } else {
                ui.label("Preparing stable snapshot / waiting for a complete safe page…");
            }
            if let Some(n) = enter {
                self.request(Request::Page(n, 0));
            }
            if previous && let Some(n) = self.parent {
                self.request(Request::Page(n, self.start.saturating_sub(64)));
            }
            if let Some(start) = next {
                if let Some(n) = self.parent {
                    self.request(Request::Page(n, start));
                } else {
                    self.request(Request::Locate(String::new()));
                }
            }
            if ui
                .add_enabled(
                    self.status == SessionStatus::Ready && self.selected.is_some() && !self.busy,
                    egui::Button::new("Copy selected complete JSON (up to 8 MiB)"),
                )
                .clicked()
            {
                self.request(Request::Copy(self.selected.unwrap()));
            }
            if ui
                .add_enabled(
                    self.status == SessionStatus::Ready && self.parent.is_some() && !self.busy,
                    egui::Button::new("Export current container / value"),
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("JSON", &["json"])
                    .save_file()
            {
                self.request(Request::Export(self.parent.unwrap(), path));
            }
            if ui
                .add_enabled(
                    self.status == SessionStatus::Ready && self.selected.is_some() && !self.busy,
                    egui::Button::new("Export selected complete value"),
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("JSON", &["json"])
                    .save_file()
            {
                self.request(Request::Export(self.selected.unwrap(), path));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn viewer() -> Viewer {
        Viewer {
            worker: None,
            options: OpenOptions::default(),
            generation: 4,
            session: None,
            status: SessionStatus::Preparing,
            source: PathBuf::from("test"),
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
        }
    }
    fn event(session: SessionId, generation: u64, status: SessionStatus) -> Message {
        Message::Event(Event {
            session,
            generation,
            commit_sequence: 0,
            committed_frontier: 0,
            status,
            scanned: 0,
            total_bytes: 100,
            root_kind: None,
            page: None,
            reused: false,
            resumed: false,
            message: None,
            view: None,
        })
    }
    #[test]
    fn stale_sessions_generations_and_late_ready_after_cancel_are_rejected() {
        let a = crate::document::Document::parse("[]").unwrap().session_id();
        let b = crate::document::Document::parse("[]").unwrap().session_id();
        let mut v = viewer();
        v.accept(event(a, 3, SessionStatus::Ready));
        assert_eq!(v.status, SessionStatus::Preparing);
        v.accept(event(a, 4, SessionStatus::Validating));
        assert_eq!(v.status, SessionStatus::Validating);
        v.accept(event(b, 4, SessionStatus::Ready));
        assert_eq!(v.status, SessionStatus::Validating);
        v.status = SessionStatus::Cancelled;
        v.accept(event(a, 4, SessionStatus::Ready));
        v.accept(Message::Error(4, "cancelled".into()));
        assert_eq!(v.status, SessionStatus::Cancelled);
    }
    #[test]
    fn failed_view_draws_and_disables_requests_without_restoring_ready() {
        let id = crate::document::Document::parse("[]").unwrap().session_id();
        let mut v = viewer();
        v.accept(event(id, 4, SessionStatus::Failed));
        v.accept(event(id, 4, SessionStatus::Ready));
        v.request(Request::Locate("/0".into()));
        assert!(!v.busy);
        assert_eq!(v.status, SessionStatus::Failed);
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| v.draw(ctx));
        assert!(!out.shapes.is_empty());
    }
    #[test]
    fn full_message_queue_wait_is_cancellable() {
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.send(Message::Error(1, "first".into())).unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let worker_flag = flag.clone();
        let ctx = egui::Context::default();
        let (started, waiting) = mpsc::channel();
        let t = std::thread::spawn(move || {
            started.send(()).unwrap();
            send(&tx, Message::Error(1, "second".into()), &worker_flag, &ctx)
        });
        waiting.recv().unwrap();
        flag.store(true, Ordering::Relaxed);
        assert!(t.join().unwrap().is_err());
    }
    #[test]
    fn ready_frame_renders_a_bounded_page_for_ten_thousand_children() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/s4-ui-tests");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("frame-{}.tmp", std::process::id()));
        let text = format!(
            "[{}]",
            (0..10000)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        std::fs::write(&path, text).unwrap();
        let mut viewer = viewer();
        let budget = BudgetManager::new(Limits {
            memory: 1024 * 1024,
            disk: 32 * 1024 * 1024,
        });
        let index = open_progressive(
            &path,
            budget.clone(),
            OpenOptions::default(),
            4,
            &|| false,
            &mut |e| {
                viewer.accept(Message::Event(e));
                Ok(())
            },
        )
        .unwrap();
        viewer.accept(Message::Root(4, index.root()));
        assert_eq!(viewer.status, SessionStatus::Ready);
        let page = viewer.page.as_ref().unwrap();
        assert_eq!(page.nodes.len(), 64);
        assert_eq!(page.total, Some(10000));
        assert_eq!(page.next, Some(64));
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| viewer.draw(ctx));
        assert!(!out.shapes.is_empty());
        drop(index);
        drop(viewer);
        assert_eq!(budget.usage().memory, 0);
        assert_eq!(budget.usage().disk, 0);
        std::fs::remove_file(path).unwrap();
    }
}
