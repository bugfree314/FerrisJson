//! Framework-neutral repaint notification shared by the desktop GUI and the
//! terminal UI. Worker threads only need to record that new output exists; the
//! front end decides when to poll it.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub struct Context {
    flag: Arc<AtomicBool>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Context {
    pub fn with_notify(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { flag: Arc::default(), notify: Some(notify) }
    }

    /// Mark the view as stale and, for the GUI, wake its event loop.
    pub fn request_repaint(&self) {
        self.flag.store(true, Ordering::Release);
        if let Some(notify) = &self.notify {
            notify();
        }
    }

    /// Consume a pending notification. The terminal front end polls this flag.
    #[cfg_attr(
        not(any(target_os = "linux", all(target_os = "macos", feature = "tui"))),
        allow(dead_code)
    )]
    pub fn take_repaint(&self) -> bool {
        self.flag.swap(false, Ordering::AcqRel)
    }
}
