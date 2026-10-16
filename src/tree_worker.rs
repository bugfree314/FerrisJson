//! One builder, independent search/query producers, one interactive lane and one output lane.
//! bounded commands, independent output; never a thread per click.
use crate::{
    document::{Document, LoadProgress},
    engine::{
        budget::{BudgetManager, Limits, Reservation, Resource},
        file_source::{Cancellation, Priority, with_io_context},
        session::{SessionId, SessionStatus},
        sparse::{OpenOptions, open_progressive},
        tree::{
            Clipboard, LegacyProvider, Location, OutputSelection, Provider, SparseProvider,
            TreeFrame,
        },
        tree_navigation::{Navigation, Navigator},
    },
};
use eframe::egui;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
};
#[derive(Clone)]
pub enum Backend {
    Sparse(OpenOptions),
    Legacy(bool),
}
#[derive(Clone)]
pub struct Status {
    pub state: SessionStatus,
    pub session: Option<SessionId>,
    pub message: String,
    pub done: u64,
    pub total: u64,
    pub kind: Option<&'static str>,
}
#[derive(Clone)]
enum Request {
    Navigation(Navigation),
    Output(Location, Option<PathBuf>, bool),
    SearchWake,
    SearchPage(u64, u64),
    QueryWake,
    QueryPage(u64, u64),
    QueryReveal(u64, u64),
    QueryOutput(u64, crate::engine::results::ResultStore, Option<u64>, Option<PathBuf>),
    SearchExport(
        PathBuf,
        bool,
        Option<u64>,
        crate::engine::search::ResultStore,
    ),
}
enum Job {
    Copy(OutputSelection),
    Export(OutputSelection, PathBuf),
    Path(String, Reservation),
    Query(crate::engine::query::stream::Input, crate::engine::results::ResultStore, Option<u64>, Option<PathBuf>, u64),
    Search(
        crate::engine::search::Input,
        crate::engine::search::ResultStore,
        PathBuf,
        bool,
        Option<u64>,
    ),
}
struct Inbox {
    provider: Option<Box<dyn Provider>>,
    commands: VecDeque<(u64, Request)>,
    frame: Option<(u64, Result<TreeFrame, String>)>,
    status: Status,
    job: Option<Job>,
    clipboard: Option<Clipboard>,
    path_copy: Option<(String, Reservation)>,
    notice: String,
    output_busy: bool,
    search_page: Option<SearchPage>,
    query_page: Option<ResultPage>,
    query_page_error: Option<(u64, u64, String)>,
    query_clipboard: Option<(u64, Clipboard)>,
    _queues: Reservation,
}
type Shared = Arc<(Mutex<Inbox>, Condvar)>;
pub struct Task {
    pub search: crate::search_worker::SearchHub,
    pub query: crate::paged_query_worker::QueryHub,
    shared: Shared,
    pub epoch: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    threads: Vec<JoinHandle<()>>,
    budget: BudgetManager,
}
pub struct ResultRow {
    pub index: u64,
    pub path: String,
    pub preview: String,
}
pub struct ResultPage {
    pub generation: u64,
    pub start: u64,
    pub count: u64,
    pub rows: Vec<ResultRow>,
    _lease: Reservation,
}
pub type SearchPage = ResultPage;
pub struct Poll {
    pub search_page: Option<SearchPage>,
    pub query_page: Option<ResultPage>,
    pub query_page_error: Option<(u64, u64, String)>,
    pub query_clipboard: Option<(u64, Clipboard)>,
    pub frame: Option<(u64, Result<TreeFrame, String>)>,
    pub status: Status,
    pub clipboard: Option<Clipboard>,
    pub path_copy: Option<(String, Reservation)>,
    pub notice: String,
}
impl Task {
    pub fn new(path: PathBuf, backend: Backend, ctx: egui::Context) -> Result<Self, String> {
        let budget = BudgetManager::new(Limits {
            memory: 256 * 1024 * 1024,
            disk: 64usize.saturating_mul(1024 * 1024 * 1024),
        });
        let queues = budget.reserve(
            Resource::Memory,
            1024 * 1024,
            "paged task inbox and bounded requests",
        )?;
        let shared = Arc::new((
            Mutex::new(Inbox {
                provider: None,
                commands: VecDeque::with_capacity(16),
                frame: None,
                status: Status {
                    state: SessionStatus::Preparing,
                    session: None,
                    message: String::new(),
                    done: 0,
                    total: 0,
                    kind: None,
                },
                job: None,
                clipboard: None,
                path_copy: None,
                notice: String::new(),
                output_busy: false,
                search_page: None,
                query_page: None,
                query_page_error: None,
                query_clipboard: None,
                _queues: queues,
            }),
            Condvar::new(),
        ));
        let epoch = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let wake_shared = shared.clone();
        let (search, search_thread) = crate::search_worker::SearchHub::new(
            &budget,
            stop.clone(),
            ctx.clone(),
            Arc::new(move || wake_shared.1.notify_all()),
        )?;
        let query_shared = shared.clone();
        let (query, query_thread) = crate::paged_query_worker::QueryHub::new(
            budget.clone(), stop.clone(), ctx.clone(), Arc::new(move || query_shared.1.notify_all()),
        );
        let build_shared = shared.clone();
        let build_epoch = epoch.clone();
        let build_stop = stop.clone();
        let build_progress = progress.clone();
        let build_ctx = ctx.clone();
        let build_budget = budget.clone();
        let builder = std::thread::spawn(move || {
            if build_stop.load(Ordering::Relaxed) {
                return;
            }
            let total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            build_shared.0.lock().unwrap().status.total = total;
            build_ctx.request_repaint();
            let cancelled = || build_stop.load(Ordering::Relaxed);
            let result = with_io_context(
                Priority::Background,
                Cancellation::Flag(build_stop.clone()),
                || -> Result<Box<dyn Provider>, String> {
                    match backend {
                        Backend::Sparse(mut options) => {
                            options.live_views = true;
                            options.progress = Some(build_progress);
                            let index = open_progressive(
                                &path,
                                build_budget,
                                options,
                                1,
                                &cancelled,
                                &mut |mut event| {
                                    let view = event
                                        .view
                                        .take()
                                        .map(|index| {
                                            SparseProvider::new(*index)
                                                .map(|p| Box::new(p) as Box<dyn Provider>)
                                        })
                                        .transpose()?;
                                    let mut inbox = build_shared.0.lock().unwrap();
                                    if cancelled() {
                                        return Err("Build cancelled".into());
                                    }
                                    inbox.status.state = event.status;
                                    inbox.status.session = Some(event.session);
                                    inbox.status.done = event.scanned;
                                    inbox.status.total = event.total_bytes.max(total);
                                    if let Some(kind) = event.root_kind {
                                        inbox.status.kind = Some(kind.name());
                                    }
                                    if let Some(message) = event.message {
                                        inbox.status.message = bounded(message);
                                    }
                                    if let Some(view) = view {
                                        inbox.provider = Some(view);
                                        build_epoch.fetch_add(1, Ordering::Relaxed);
                                    }
                                    if matches!(
                                        event.status,
                                        SessionStatus::Failed | SessionStatus::Cancelled
                                    ) {
                                        inbox.provider = None;
                                        inbox.frame = None;
                                        inbox.commands.clear();
                                        build_epoch.fetch_add(1, Ordering::Relaxed);
                                    }
                                    drop(inbox);
                                    build_shared.1.notify_all();
                                    build_ctx.request_repaint();
                                    if cancelled() {
                                        Err("Build cancelled".into())
                                    } else {
                                        Ok(())
                                    }
                                },
                            )?;
                            Ok(Box::new(SparseProvider::new(index)?))
                        }
                        Backend::Legacy(indexed) => {
                            let doc = if indexed {
                                Document::open_indexed_with_budget(
                                    &path,
                                    cancelled,
                                    &LoadProgress::default(),
                                    build_budget,
                                )?
                            } else {
                                use std::io::Read;
                                let mut file =
                                    std::fs::File::open(&path).map_err(|e| e.to_string())?;
                                let len = usize::try_from(
                                    file.metadata().map_err(|e| e.to_string())?.len(),
                                )
                                .map_err(|_| "Input size overflow")?;
                                let input = build_budget.reserve(
                                    Resource::Memory,
                                    len,
                                    "legacy input text",
                                )?;
                                let mut bytes = vec![0; len];
                                for chunk in bytes.chunks_mut(64 * 1024) {
                                    if cancelled() {
                                        return Err("Build cancelled".into());
                                    }
                                    file.read_exact(chunk).map_err(|e| e.to_string())?;
                                }
                                if file.read(&mut [0; 1]).map_err(|e| e.to_string())? != 0 {
                                    return Err("Input grew while reading; reopen the file".into());
                                }
                                let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
                                drop(input); // The document adopts and charges this same allocation.
                                Document::parse_owned_recover_with_budget(text, build_budget)
                                    .map_err(|(e, _)| e)?
                            };
                            Ok(Box::new(LegacyProvider(Arc::new(doc))))
                        }
                    }
                },
            );
            let mut inbox = build_shared.0.lock().unwrap();
            if !cancelled() {
                match result {
                    Ok(mut provider) => match provider.root(&cancelled) {
                        Ok(root) => {
                            inbox.status.session = Some(root.reference.session);
                            inbox.status.kind = Some(root.kind);
                            inbox.status.state = SessionStatus::Ready;
                            inbox.status.done = total;
                            inbox.provider = Some(provider);
                        }
                        Err(e) => {
                            inbox.status.state = SessionStatus::Failed;
                            inbox.status.message = bounded(e);
                        }
                    },
                    Err(e) => {
                        inbox.status.state = SessionStatus::Failed;
                        inbox.status.message = bounded(e);
                        inbox.provider = None;
                        inbox.frame = None;
                    }
                }
                build_epoch.fetch_add(1, Ordering::Relaxed);
            }
            drop(inbox);
            build_shared.1.notify_all();
            build_ctx.request_repaint();
        });
        let worker_shared = shared.clone();
        let worker_epoch = epoch.clone();
        let worker_stop = stop.clone();
        let worker_ctx = ctx.clone();
        let worker_search = search.clone();
        let worker_query = query.clone();
        let interactive = std::thread::spawn(move || {
            let mut navigator: Option<Navigator> = None;
            loop {
                let (provider, command, version, failed) = {
                    let mut inbox = worker_shared.0.lock().unwrap();
                    while inbox.provider.is_none()
                        && (inbox.commands.is_empty() || navigator.is_none())
                        && !worker_search.selection_ready()
                        && !worker_stop.load(Ordering::Relaxed)
                        && !matches!(
                            inbox.status.state,
                            SessionStatus::Failed | SessionStatus::Cancelled
                        )
                    {
                        inbox = worker_shared.1.wait(inbox).unwrap();
                    }
                    if worker_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let failed = matches!(
                        inbox.status.state,
                        SessionStatus::Failed | SessionStatus::Cancelled
                    );
                    if failed {
                        inbox.frame = None;
                        inbox.provider = None;
                        inbox.commands.clear();
                    }
                    (
                        inbox.provider.take(),
                        inbox.commands.front().cloned(),
                        worker_epoch.load(Ordering::Relaxed),
                        failed,
                    )
                };
                if failed {
                    worker_query.cancel(true);
                    return;
                }
                let cancelled = || {
                    worker_stop.load(Ordering::Relaxed)
                        || worker_epoch.load(Ordering::Relaxed) != version
                };
                let result = with_io_context(
                    Priority::Interactive,
                    Cancellation::Any(vec![
                        Cancellation::Flag(worker_stop.clone()),
                        Cancellation::Generation(worker_epoch.clone(), version),
                    ]),
                    || -> Result<Option<TreeFrame>, String> {
                        if let Some(provider) = provider {
                            if let Some(nav) = &mut navigator {
                                nav.replace(provider, &cancelled)?;
                            } else {
                                navigator = Some(with_io_context(
                                    Priority::Interactive,
                                    Cancellation::Flag(worker_stop.clone()),
                                    || {
                                        Navigator::new(provider, &|| {
                                            worker_stop.load(Ordering::Relaxed)
                                        })
                                    },
                                )?);
                            }
                        }
                        let Some(nav) = &mut navigator else {
                            return Ok(None);
                        };
                        if let Some((id, command)) = &command {
                            let reset = matches!(
                                command,
                                Request::Navigation(
                                    Navigation::ExpandAll | Navigation::CollapseAll
                                )
                            );
                            let result = match command {
                                Request::SearchWake => Ok(()),
                                Request::QueryWake => Ok(()),
                                Request::QueryReveal(generation, index) => (|| {
                                    if *generation != worker_query.generation() { return Ok(()); }
                                    let store = worker_query.store().ok_or("No query results")?;
                                    let query_cancel = || cancelled() || *generation != worker_query.generation();
                                    let location = store.location(*index, &query_cancel)?;
                                    nav.apply(Navigation::Reveal(location), &query_cancel)
                                })(),
                                Request::QueryPage(generation, start) => (|| {
                                    if *generation != worker_query.generation() { return Ok(()); }
                                    let store = worker_query.store().ok_or("No query results")?;
                                    let count = store.state().count;
                                    let lease = nav.tree.provider.budget().reserve(Resource::Memory, 64 * 1024, "bounded query presentation page")?;
                                    let mut rows = Vec::with_capacity(16);
                                    let query_cancel = || cancelled() || *generation != worker_query.generation();
                                    for index in *start..start.saturating_add(16).min(count) {
                                        let loc = store.location(index, &query_cancel)?;
                                        let loc = nav.tree.normalize(&loc, &query_cancel)?;
                                        rows.push(ResultRow { index, path: nav.tree.path(&loc, 512, false, &query_cancel)?,
                                            preview: nav.tree.provider.preview(loc.node, 160, &query_cancel)? });
                                    }
                                    if !query_cancel() { worker_shared.0.lock().unwrap().query_page = Some(ResultPage {
                                        generation: *generation, start: *start, count, rows, _lease: lease,
                                    }); }
                                    Ok(())
                                })(),
                                Request::QueryOutput(generation, store, prefix, path) => (|| {
                                    if *generation != worker_query.generation() { return Err("Query output capture cancelled".into()); }
                                    let input = nav.tree.provider.query_input()?;
                                    worker_shared.0.lock().unwrap().job = Some(Job::Query(input, store.clone(), *prefix, path.clone(), *generation));
                                    worker_shared.1.notify_all(); Ok(())
                                })(),
                                Request::SearchPage(generation, start) => (|| {
                                    if *generation != worker_search.generation() {
                                        return Ok(());
                                    }
                                    let store = worker_search.store().ok_or("No search results")?;
                                    let count = store.state().count;
                                    let lease = nav.tree.provider.budget().reserve(
                                        Resource::Memory,
                                        64 * 1024,
                                        "bounded search result presentation page",
                                    )?;
                                    let mut rows = Vec::with_capacity(16);
                                    let page_cancel =
                                        || cancelled() || *generation != worker_search.generation();
                                    for index in *start..start.saturating_add(16).min(count) {
                                        let loc = store.location(index, &page_cancel)?;
                                        let loc = nav.tree.normalize(&loc, &page_cancel)?;
                                        rows.push(ResultRow {
                                            index,
                                            path: nav.tree.path(&loc, 512, false, &page_cancel)?,
                                            preview: nav.tree.provider.preview(
                                                loc.node,
                                                160,
                                                &page_cancel,
                                            )?,
                                        });
                                    }
                                    if !page_cancel() {
                                        worker_shared.0.lock().unwrap().search_page =
                                            Some(SearchPage {
                                                generation: *generation,
                                                start: *start,
                                                count,
                                                rows,
                                                _lease: lease,
                                            });
                                    }
                                    Ok(())
                                })(
                                ),
                                Request::SearchExport(path, paths, prefix, store) => (|| {
                                    let input = nav.tree.provider.search_input()?;
                                    worker_shared.0.lock().unwrap().job = Some(Job::Search(
                                        input,
                                        store.clone(),
                                        path.clone(),
                                        *paths,
                                        *prefix,
                                    ));
                                    worker_shared.1.notify_all();
                                    Ok(())
                                })(
                                ),
                                Request::Navigation(command) => {
                                    if reset {
                                        with_io_context(
                                            Priority::Interactive,
                                            Cancellation::Flag(worker_stop.clone()),
                                            || {
                                                nav.apply(command.clone(), &|| {
                                                    worker_stop.load(Ordering::Relaxed)
                                                })
                                            },
                                        )
                                    } else {
                                        nav.apply(command.clone(), &cancelled)
                                    }
                                }
                                Request::Output(location, path, copy_path) => {
                                    (|| {
                                        let location = nav.tree.normalize(location, &cancelled)?;
                                        if !nav.tree.provider.complete() {
                                            return Err("Pending: complete validation is required for output".into());
                                        }
                                        let job = if *copy_path {
                                            let budget = nav.tree.provider.budget();
                                            let _workspace = budget.reserve(
                                                Resource::Memory,
                                                64 * 1024 * 1024,
                                                "full path escaping workspace",
                                            )?;
                                            let lease = budget.reserve(
                                                Resource::Memory,
                                                8 * 1024 * 1024,
                                                "complete copied path",
                                            )?;
                                            Job::Path(
                                                nav.tree.path(
                                                    &location,
                                                    8 * 1024 * 1024,
                                                    true,
                                                    &cancelled,
                                                )?,
                                                lease,
                                            )
                                        } else {
                                            let selection = nav
                                                .tree
                                                .provider
                                                .output(location.node, &cancelled)?;
                                            if let Some(path) = path {
                                                Job::Export(selection, path.clone())
                                            } else {
                                                Job::Copy(selection)
                                            }
                                        };
                                        if cancelled() {
                                            return Err("Output capture cancelled".into());
                                        }
                                        let mut inbox = worker_shared.0.lock().unwrap();
                                        inbox.job = Some(job);
                                        // Once captured, output belongs to its independent lane.
                                        // A concurrent navigation must not replay this request.
                                        if inbox
                                            .commands
                                            .front()
                                            .is_some_and(|(front, _)| front == id)
                                        {
                                            inbox.commands.pop_front();
                                        }
                                        drop(inbox);
                                        worker_shared.1.notify_all();
                                        Ok(())
                                    })()
                                }
                            };
                            // A successful navigation has already committed its state.
                            // Remove it even if a newer epoch arrived just after commit.
                            if cancelled() && !reset && result.is_err() {
                                return result.map(|_| None);
                            }
                            let mut inbox = worker_shared.0.lock().unwrap();
                            if inbox.commands.front().is_some_and(|(front, _)| front == id) {
                                inbox.commands.pop_front();
                            }
                            if let Err(e) = result {
                                if let Request::QueryPage(generation, start) = command
                                    && *generation == worker_query.generation()
                                { inbox.query_page_error = Some((*generation, *start, bounded(e.clone()))); }
                                inbox.notice = bounded(e);
                                if matches!(
                                    command,
                                    Request::Output(..) | Request::SearchExport(..) | Request::QueryOutput(..)
                                ) {
                                    inbox.output_busy = false;
                                }
                            }
                            drop(inbox);
                        }
                        if worker_search.pending() && nav.tree.provider.complete() {
                            worker_search.dispatch(nav.tree.provider.search_input()?);
                        }
                        if worker_query.pending() && nav.tree.provider.complete() {
                            match nav.tree.provider.query_input() {
                                Ok(input) => worker_query.dispatch(input),
                                Err(error) => worker_query.fail_pending(error),
                            }
                        }
                        if let Some((generation, index, store)) = worker_search.take_selection() {
                            let selection_cancel =
                                || cancelled() || worker_search.generation() != generation;
                            let result = store.location(index, &selection_cancel).and_then(|loc| {
                                nav.apply(Navigation::Reveal(loc), &selection_cancel)
                            });
                            if result.is_ok() {
                                worker_search.accept(generation, index);
                            } else if selection_cancel() {
                                return Ok(None);
                            } else {
                                worker_search.cancel_selection();
                                return result.map(|_| None);
                            }
                        }
                        if cancelled() {
                            return Ok(None);
                        }
                        Ok(Some(nav.frame(&cancelled)?))
                    },
                );
                if !cancelled() {
                    let mut inbox = worker_shared.0.lock().unwrap();
                    if cancelled()
                        || matches!(
                            inbox.status.state,
                            SessionStatus::Failed | SessionStatus::Cancelled
                        )
                    {
                        continue;
                    }
                    match result {
                        Ok(Some(frame)) => inbox.frame = Some((version, Ok(frame))),
                        Ok(None) => {
                            if navigator.is_none() {
                                inbox.commands.clear();
                            }
                        }
                        Err(e) => {
                            if let Some((_, Request::QueryPage(generation, start))) = &command
                                && *generation == worker_query.generation()
                            { inbox.query_page_error = Some((*generation, *start, bounded(e.clone()))); }
                            if command.as_ref().is_some_and(|(_, command)| matches!(command, Request::Output(..) | Request::SearchExport(..) | Request::QueryOutput(..))) {
                                inbox.output_busy = false;
                            }
                            inbox.notice = bounded(e);
                            if let Some((id, _)) = command
                                && inbox
                                    .commands
                                    .front()
                                    .is_some_and(|(front, _)| *front == id)
                            {
                                inbox.commands.pop_front();
                            }
                        }
                    }
                    drop(inbox);
                    worker_ctx.request_repaint();
                }
            }
        });
        let output_shared = shared.clone();
        let output_stop = stop.clone();
        let output_ctx = ctx.clone();
        let output_query = query.clone();
        let output = std::thread::spawn(move || {
            loop {
                let job = {
                    let mut inbox = output_shared.0.lock().unwrap();
                    while inbox.job.is_none() && !output_stop.load(Ordering::Relaxed) {
                        inbox = output_shared.1.wait(inbox).unwrap();
                    }
                    if output_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    inbox.job.take().unwrap()
                };
                let cancelled = || output_stop.load(Ordering::Relaxed);
                let query_generation = match &job { Job::Query(_, _, _, _, generation) => Some(*generation), _ => None };
                let result = with_io_context(
                    Priority::User,
                    Cancellation::Flag(output_stop.clone()),
                    || match job {
                        Job::Query(input, store, prefix, path, generation) => {
                            let provider = input.provider();
                            if let Some(path) = path {
                                provider.and_then(|provider| crate::engine::results::output::export(provider, &store, &path, false, prefix, &output_stop))
                                    .map(|()| (None, None, "Exported query results".into()))
                            } else {
                                provider.and_then(|provider| crate::engine::results::output::copy(provider, &store, prefix,
                                    &|| cancelled() || output_query.generation() != generation))
                                    .map(|clip| (Some(clip), None, "Copied query results".into()))
                            }
                        }
                        Job::Search(input, store, path, paths, prefix) => {
                            crate::engine::search::output::export(
                                input,
                                &store,
                                &path,
                                paths,
                                prefix,
                                &output_stop,
                            )
                            .map(|()| {
                                (
                                    None,
                                    None,
                                    format!(
                                        "Exported {} search results to {}",
                                        if prefix.is_some() {
                                            "partial"
                                        } else {
                                            "complete"
                                        },
                                        path.display()
                                    ),
                                )
                            })
                        }
                        Job::Copy(selection) => selection
                            .copy(&cancelled)
                            .map(|clip| (Some(clip), None, "Copied complete JSON".into())),
                        Job::Export(selection, path) => selection
                            .export(&path, &output_stop)
                            .map(|()| (None, None, format!("Exported {}", path.display()))),
                        Job::Path(text, lease) => {
                            Ok((None, Some((text, lease)), "Copied complete path".into()))
                        }
                    },
                );
                if !cancelled() {
                    let mut inbox = output_shared.0.lock().unwrap();
                    inbox.output_busy = false;
                    match result {
                        Ok((clip, path, notice)) => {
                            if let Some(generation) = query_generation {
                                if generation == output_query.generation() { inbox.query_clipboard = clip.map(|clip| (generation, clip)); }
                            } else { inbox.clipboard = clip; }
                            inbox.path_copy = path;
                            inbox.notice = bounded(notice);
                        }
                        Err(e) => inbox.notice = bounded(e),
                    }
                    drop(inbox);
                    output_ctx.request_repaint();
                }
            }
        });
        Ok(Self {
            search,
            query,
            shared,
            epoch,
            stop,
            progress,
            threads: vec![builder, interactive, output, search_thread, query_thread],
            budget,
        })
    }
    pub fn budget(&self) -> BudgetManager {
        self.budget.clone()
    }
    pub fn query_request(&self, expression: String) -> Result<(), String> {
        let mut b = self.shared.0.lock().unwrap();
        if matches!(b.status.state, SessionStatus::Failed | SessionStatus::Cancelled) { return Err("Session unavailable".into()); }
        if b.commands.len() >= 16 { return Err("Navigation queue full; retry query".into()); }
        self.query.request(crate::engine::query::QueryRequest::new(expression))?;
        b.query_page = None; b.query_page_error = None; b.query_clipboard = None;
        b.commands.retain(|(_, c)| !matches!(c, Request::QueryWake | Request::QueryPage(..) | Request::QueryReveal(..)));
        let epoch = self.epoch.load(Ordering::Relaxed);
        b.commands.push_back((epoch, Request::QueryWake));
        drop(b); self.shared.1.notify_all(); Ok(())
    }
    pub fn query_clear(&self, clear: bool) {
        self.query.cancel(clear);
        let mut b = self.shared.0.lock().unwrap();
        b.query_page = None; b.query_page_error = None; b.query_clipboard = None;
        b.commands.retain(|(_, c)| !matches!(c, Request::QueryWake | Request::QueryPage(..) | Request::QueryReveal(..)));
        drop(b); self.shared.1.notify_all();
    }
    pub fn query_page(&self, start: u64) -> Result<(), String> {
        self.query_command(Request::QueryPage(self.query.generation(), start))
    }
    pub fn query_select(&self, index: u64) -> Result<(), String> {
        self.search.cancel_selection();
        self.query_command(Request::QueryReveal(self.query.generation(), index))
    }
    fn query_command(&self, request: Request) -> Result<(), String> {
        let mut b = self.shared.0.lock().unwrap();
        if matches!(request, Request::QueryPage(..)) { b.commands.retain(|(_, c)| !matches!(c, Request::QueryPage(..))); }
        if matches!(request, Request::QueryReveal(..)) { b.commands.retain(|(_, c)| !matches!(c, Request::QueryReveal(..))); }
        if b.commands.len() >= 16 { return Err("Navigation queue full; retry query action".into()); }
        let epoch = self.epoch.load(Ordering::Relaxed);
        b.commands.push_back((epoch, request)); drop(b); self.shared.1.notify_all(); Ok(())
    }
    pub fn query_output(&self, path: Option<PathBuf>, prefix: Option<u64>) -> Result<(), String> {
        let store = self.query.store().ok_or("No query results")?;
        let state = store.state();
        if prefix.is_none() && state.status != crate::engine::results::Status::Completed { return Err("Complete output requires a completed query".into()); }
        if prefix.is_some_and(|count| count > state.count) { return Err("Requested query prefix is not committed".into()); }
        let mut b = self.shared.0.lock().unwrap();
        if b.status.state != SessionStatus::Ready { return Err("Pending: full validation required".into()); }
        if b.output_busy || b.commands.len() >= 16 { return Err("Output lane is busy".into()); }
        b.output_busy = true;
        let epoch = self.epoch.load(Ordering::Relaxed);
        b.commands.push_back((epoch, Request::QueryOutput(self.query.generation(), store, prefix, path)));
        drop(b); self.shared.1.notify_all(); Ok(())
    }
    pub fn search_query(
        &self,
        query: crate::engine::search::Query,
        reverse: bool,
    ) -> Result<(), String> {
        self.search.request(query, reverse)?;
        let mut b = self.shared.0.lock().unwrap();
        b.commands
            .retain(|(_, c)| matches!(c, Request::Output(..) | Request::SearchExport(..) | Request::QueryOutput(..) | Request::QueryPage(..) | Request::QueryWake));
        let epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        b.commands
            .push_back((epoch, Request::Navigation(Navigation::CancelPending)));
        b.commands.push_back((epoch, Request::SearchWake));
        drop(b);
        self.shared.1.notify_all();
        Ok(())
    }
    pub fn search_match(&self, forward: bool, count: u64) {
        self.search.next(forward, count);
    }
    pub fn search_select(&self, index: u64) {
        self.search.select(index);
    }
    pub fn search_page(&self, start: u64) {
        let mut b = self.shared.0.lock().unwrap();
        b.commands
            .retain(|(_, c)| !matches!(c, Request::SearchPage(..)));
        if b.commands.len() < 16 {
            let epoch = self.epoch.load(Ordering::Relaxed);
            b.commands
                .push_back((epoch, Request::SearchPage(self.search.generation(), start)));
        }
        drop(b);
        self.shared.1.notify_all();
    }
    pub fn search_export(&self, path: PathBuf, paths: bool, partial: bool) -> Result<(), String> {
        let store = self.search.store().ok_or("No search results")?;
        let prefix = partial.then(|| store.state().count);
        let mut b = self.shared.0.lock().unwrap();
        if b.output_busy || b.commands.len() >= 16 {
            return Err("Output lane is busy".into());
        }
        b.output_busy = true;
        let epoch = self.epoch.load(Ordering::Relaxed);
        b.commands
            .push_back((epoch, Request::SearchExport(path, paths, prefix, store)));
        drop(b);
        self.shared.1.notify_all();
        Ok(())
    }
    pub fn search_clear(&self, cancel: bool) {
        if cancel {
            self.search.cancel();
        } else {
            self.search.clear();
        }
        self.epoch.fetch_add(1, Ordering::Relaxed);
        let mut b = self.shared.0.lock().unwrap();
        b.search_page = None;
        b.commands
            .retain(|(_, c)| !matches!(c, Request::SearchPage(..)));
        drop(b);
        self.shared.1.notify_all();
    }
    pub fn command(&self, command: Navigation) -> Result<(), String> {
        if matches!(
            command,
            Navigation::Select(_)
                | Navigation::Move(_)
                | Navigation::First
                | Navigation::Last
                | Navigation::Row(_)
                | Navigation::Left(_)
                | Navigation::Right(_)
                | Navigation::History(_)
                | Navigation::CancelPending
        ) {
            self.search.cancel_selection();
        }

        let mut inbox = self.shared.0.lock().unwrap();
        if matches!(
            inbox.status.state,
            SessionStatus::Failed | SessionStatus::Cancelled
        ) {
            return Err("Session unavailable".into());
        }
        if matches!(
            command,
            Navigation::Select(_)
                | Navigation::Locate(_)
                | Navigation::First
                | Navigation::Last
                | Navigation::Row(_)
                | Navigation::CancelPending
        ) {
            inbox
                .commands
                .retain(|(_, c)| matches!(c, Request::Output(..) | Request::SearchExport(..) | Request::QueryOutput(..) | Request::QueryPage(..) | Request::QueryWake));
        }
        if matches!(command, Navigation::Anchor(_)) {
            inbox
                .commands
                .retain(|(_, c)| !matches!(c, Request::Navigation(Navigation::Anchor(_))));
        }
        if inbox.commands.len() >= 16 {
            return Err("Navigation queue full".into());
        }
        let epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        inbox
            .commands
            .push_back((epoch, Request::Navigation(command)));
        drop(inbox);
        self.shared.1.notify_all();
        Ok(())
    }
    pub fn output(
        &self,
        location: Location,
        path: Option<PathBuf>,
        copy_path: bool,
    ) -> Result<(), String> {
        let mut inbox = self.shared.0.lock().unwrap();
        if inbox.status.state != SessionStatus::Ready {
            return Err("Pending: full validation required".into());
        }
        if inbox.output_busy || inbox.commands.len() >= 16 {
            return Err("Output lane is busy".into());
        }
        inbox.output_busy = true;
        let epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        inbox
            .commands
            .push_back((epoch, Request::Output(location, path, copy_path)));
        drop(inbox);
        self.shared.1.notify_all();
        Ok(())
    }
    pub fn poll(&self) -> Poll {
        let mut inbox = self.shared.0.lock().unwrap();
        let mut status = inbox.status.clone();
        if status.state == SessionStatus::Validating {
            status.done = status.done.max(self.progress.load(Ordering::Relaxed));
        }
        Poll {
            search_page: inbox.search_page.take(),
            query_page: inbox.query_page.take(),
            query_page_error: inbox.query_page_error.take(),
            query_clipboard: inbox.query_clipboard.take(),
            frame: inbox.frame.take(),
            status,
            clipboard: inbox.clipboard.take(),
            path_copy: inbox.path_copy.take(),
            notice: std::mem::take(&mut inbox.notice),
        }
    }
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.search.stop();
        self.query.cancel(true);
        self.query.stop();
        self.epoch.fetch_add(1, Ordering::Relaxed);
        let mut inbox = self.shared.0.lock().unwrap();
        inbox.status.state = SessionStatus::Cancelled;
        inbox.provider = None;
        inbox.commands.clear();
        inbox.frame = None;
        inbox.job = None;
        drop(inbox);
        self.shared.1.notify_all();
    }
    pub fn retire(mut self) -> JoinHandle<()> {
        self.cancel();
        let threads = std::mem::take(&mut self.threads);
        std::thread::spawn(move || {
            for thread in threads {
                let _ = thread.join();
            }
        })
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel();
        let threads = std::mem::take(&mut self.threads);
        if !threads.is_empty() {
            crate::shutdown::defer(std::thread::spawn(move || {
                for thread in threads {
                    let _ = thread.join();
                }
            }));
        }
    }
}
fn bounded(mut text: String) -> String {
    if text.len() > 16 * 1024 {
        let mut n = 16 * 1024;
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        text.truncate(n);
        text.push('…');
    }
    text
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    struct Input(PathBuf);
    impl Input {
        fn new(bytes: impl AsRef<[u8]>) -> Self {
            static ID: AtomicU64 = AtomicU64::new(0);
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/s5-worker-tests");
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!(
                "input-{}-{}.tmp",
                std::process::id(),
                ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&p, bytes).unwrap();
            Self(p)
        }
    }
    impl Drop for Input {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn wait(task: &Task, predicate: impl Fn(&TreeFrame) -> bool) -> TreeFrame {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let p = task.poll();
            if let Some(clip) = p.clipboard {
                task.shared.0.lock().unwrap().clipboard = Some(clip);
            }
            assert!(
                !matches!(
                    p.status.state,
                    SessionStatus::Failed | SessionStatus::Cancelled
                ),
                "{} {}",
                p.status.message,
                p.notice
            );
            if let Some((epoch, Ok(frame))) = p.frame
                && epoch == task.epoch.load(Ordering::Relaxed)
                && predicate(&frame)
            {
                return frame;
            }
            assert!(Instant::now() < deadline, "worker timed out: {}", p.notice);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn search_backpressure_latest_query_navigation_pages_and_retirement() {
        let input = Input::new(format!(
            "[{}]",
            std::iter::repeat_n("0", 10_000)
                .collect::<Vec<_>>()
                .join(",")
        ));
        let task = Task::new(
            input.0.clone(),
            Backend::Sparse(OpenOptions::default()),
            egui::Context::default(),
        )
        .unwrap();
        let frame = wait(&task, |f| f.exact);
        drop(frame);
        task.search_query(
            crate::engine::search::Query {
                term: "0".into(),
                case: true,
                regex: false,
                scope: crate::document::SearchScope::Values,
            },
            false,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while task.search.store().is_none_or(|s| s.state().count < 385) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(25));
        let snap = task.search.poll();
        assert_eq!(snap.queue_peak, 4);
        assert_eq!(
            snap.state.unwrap().status,
            crate::engine::search::Status::Scanning
        );
        for _ in 0..100 {
            task.search_match(true, 1);
        }
        loop {
            task.search.poll();
            if let Some((_, Ok(frame))) = task.poll().frame
                && frame.selection.path == "$[100]"
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        task.search_select(20_000);
        task.command(Navigation::First).unwrap();
        task.search_page(9990);
        loop {
            let snap = task.search.poll();
            if snap
                .state
                .is_some_and(|s| s.status == crate::engine::search::Status::Completed)
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        task.search_page(9990);
        loop {
            if let Some(page) = task.poll().search_page
                && page.count == 10_000
            {
                assert_eq!(page.rows.len(), 10);
                assert_eq!(page.rows[9].path, "$[9999]");
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        task.command(Navigation::First).unwrap();
        drop(wait(&task, |f| f.selection.path == "$"));
        task.search_query(crate::engine::search::Query::literal("absent"), true)
            .unwrap();
        let generation = task.search.generation();
        loop {
            let snap = task.search.poll();
            assert_eq!(snap.generation, generation);
            if let Some(state) = snap.state
                && state.status == crate::engine::search::Status::Completed
            {
                assert_eq!(state.count, 0);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let b = task.budget();
        task.retire().join().unwrap();
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    #[test]
    fn cancel_search_while_event_queue_is_full_preserves_partial_and_joins() {
        let input = Input::new(format!(
            "[{}]",
            std::iter::repeat_n("1", 10_000)
                .collect::<Vec<_>>()
                .join(",")
        ));
        let task = Task::new(
            input.0.clone(),
            Backend::Sparse(OpenOptions::default()),
            egui::Context::default(),
        )
        .unwrap();
        drop(wait(&task, |f| f.exact));
        task.search_query(crate::engine::search::Query::literal("1"), false)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while task.search.store().is_none_or(|s| s.state().count < 385) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        task.search_clear(true);
        let store = task.search.store().unwrap();
        assert_eq!(
            store.state().status,
            crate::engine::search::Status::Cancelled
        );
        assert!(store.location(0, &|| false).is_ok());
        let b = task.budget();
        task.retire().join().unwrap();
        drop(store);
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    #[test]
    fn reverse_matches_wrap_completed_results_and_full_export_runs_in_output_lane() {
        let input = Input::new(r#"["yes",0,"yes",{"a":"yes"}]"#);
        let task = Task::new(
            input.0.clone(),
            Backend::Sparse(OpenOptions::default()),
            egui::Context::default(),
        )
        .unwrap();
        drop(wait(&task, |f| f.exact));
        task.search_query(
            crate::engine::search::Query {
                term: "yes".into(),
                case: true,
                regex: false,
                scope: crate::document::SearchScope::Values,
            },
            true,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            task.search.poll();
            if let Some((_, Ok(frame))) = task.poll().frame
                && frame.selection.path == r#"$[3]["a"]"#
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        task.search_match(true, 1);
        drop(wait(&task, |f| f.selection.path == "$[0]"));
        let destination = Input::new("old");
        task.search_export(destination.0.clone(), false, false)
            .unwrap();
        task.search_query(crate::engine::search::Query::literal("0"), false)
            .unwrap();
        task.command(Navigation::Last).unwrap();
        loop {
            task.search.poll();
            let poll = task.poll();
            if poll.notice.starts_with("Exported complete") {
                break;
            }
            assert!(Instant::now() < deadline, "{}", poll.notice);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            std::fs::read_to_string(&destination.0).unwrap(),
            r#"["yes","yes","yes"]"#
        );
        task.search_clear(false);
        assert!(task.search.store().is_none());
        let b = task.budget();
        task.retire().join().unwrap();
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    #[test]
    fn legacy_input_budget_rejects_large_files_before_allocating_text() {
        let input = Input::new([]);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&input.0)
            .unwrap()
            .set_len(300 * 1024 * 1024)
            .unwrap();
        let task = Task::new(
            input.0.clone(),
            Backend::Legacy(false),
            egui::Context::default(),
        )
        .unwrap();
        let b = task.budget();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let p = task.poll();
            if p.status.state == SessionStatus::Failed {
                assert!(p.status.message.contains("budget exceeded"));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(b.usage().peak_memory < 2 * 1024 * 1024);
        task.retire().join().unwrap();
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    #[test]
    fn rapid_relative_moves_are_committed_once_across_coalesced_scroll_epochs() {
        let text = format!(
            "[{}]",
            (0..1000)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let input = Input::new(text);
        let task = Task::new(
            input.0.clone(),
            Backend::Sparse(OpenOptions::default()),
            egui::Context::default(),
        )
        .unwrap();
        let b = task.budget();
        wait(&task, |f| f.exact);
        let mut accepted = 0;
        for n in 0..200 {
            if task.command(Navigation::Move(1)).is_ok() {
                accepted += 1;
            }
            let _ = task.command(Navigation::Anchor(n));
            if n % 7 == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(accepted > 0);
        let frame = wait(&task, |f| {
            f.selected_rank == accepted && task.shared.0.lock().unwrap().commands.is_empty()
        });
        assert_eq!(frame.selected_rank, accepted);
        drop(frame);
        task.retire().join().unwrap();
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    #[test]
    fn latest_viewport_selection_copy_and_output_lane_are_bounded() {
        let text = format!(
            "[{}]",
            (0..5000)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let input = Input::new(text);
        let task = Task::new(
            input.0.clone(),
            Backend::Sparse(OpenOptions::default()),
            egui::Context::default(),
        )
        .unwrap();
        let first = wait(&task, |f| f.exact);
        assert_eq!(first.total, 5001);
        for anchor in 0..100 {
            task.command(Navigation::Anchor(anchor)).unwrap();
        }
        task.command(Navigation::Locate("/4999".into())).unwrap();
        let last = wait(&task, |f| f.selected_rank == 5000);
        assert_eq!(last.selection.path, "$[4999]");
        task.output(last.selected.clone(), None, false).unwrap();
        task.command(Navigation::First).unwrap();
        let root = wait(&task, |f| f.selected_rank == 0);
        assert_eq!(root.selection.path, "$");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(clip) = task.poll().clipboard {
                assert_eq!(clip.text, "4999");
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(task.shared.0.lock().unwrap().commands.len() <= 16);
        drop(first);
        drop(last);
        drop(root);
        let b = task.budget();
        task.retire().join().unwrap();
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    #[test]
    fn committed_tree_navigation_is_available_before_full_scan_and_cancel_closes() {
        let mut bytes = b"[0,\"".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 20 * 1024 * 1024));
        bytes.extend_from_slice(b"\",2]");
        let input = Input::new(bytes);
        let task = Task::new(
            input.0.clone(),
            Backend::Sparse(OpenOptions::default()),
            egui::Context::default(),
        )
        .unwrap();
        let partial = wait(&task, |f| !f.exact && f.total >= 2);
        task.command(Navigation::Row(1)).unwrap();
        let value = wait(&task, |f| f.selected_rank == 1);
        assert!(!value.exact);
        assert_eq!(value.selection.preview, "0");
        assert!(task.output(value.selected.clone(), None, false).is_err());
        task.command(Navigation::Last).unwrap();
        task.command(Navigation::CancelPending).unwrap();
        drop(partial);
        drop(value);
        let b = task.budget();
        let start = Instant::now();
        task.retire().join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    #[test]
    fn tail_failure_never_returns_a_ready_tree_and_late_results_are_dropped() {
        let input = Input::new(b"[1,2] trailing");
        let task = Task::new(
            input.0.clone(),
            Backend::Sparse(OpenOptions::default()),
            egui::Context::default(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let p = task.poll();
            assert_ne!(p.status.state, SessionStatus::Ready);
            if p.status.state == SessionStatus::Failed {
                assert!(p.frame.is_none());
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(task.command(Navigation::Last).is_err());
        task.retire().join().unwrap();
    }
}
