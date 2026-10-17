//! Terminal counterpart of the desktop repaint notification. The existing worker
//! queues and cancellation protocol stay shared; no windowing crate is needed.
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

#[derive(Clone, Default)]
pub struct Context(Arc<AtomicBool>);

impl Context {
    pub fn request_repaint(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn take_repaint(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}
