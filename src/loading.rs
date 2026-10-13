use crate::document::Document;
use std::{sync::Arc, time::Duration};

/// Keep the last reference on this thread, so destruction finishes before loading.
/// Merely observing a Weak with zero strong references would not wait for Drop.
pub fn release_previous(
    old: Option<Arc<Document>>,
    cancelled: impl Fn() -> bool,
    waiting: impl Fn(bool),
) -> bool {
    if let Some(old) = old {
        waiting(true);
        while Arc::strong_count(&old) > 1 {
            if cancelled() {
                waiting(false);
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(old);
    }
    waiting(false);
    !cancelled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    #[test]
    fn waits_for_other_owners_then_releases_the_document() {
        let old = Arc::new(Document::parse("[1,2,3]").unwrap());
        let weak = Arc::downgrade(&old);
        let worker = Arc::clone(&old);
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let ready = release_previous(
                Some(old),
                || false,
                |waiting| {
                    if waiting {
                        started_tx.send(()).unwrap();
                    }
                },
            );
            done_tx.send(ready).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done_rx.try_recv().is_err());
        assert!(weak.upgrade().is_some());
        drop(worker);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        thread.join().unwrap();
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn cancellation_stops_waiting_without_starting_a_new_load() {
        let old = Arc::new(Document::parse("null").unwrap());
        let owner = Arc::clone(&old);
        let waiting = AtomicBool::new(false);
        assert!(!release_previous(
            Some(old),
            || true,
            |v| waiting.store(v, Ordering::Relaxed)
        ));
        assert!(!waiting.load(Ordering::Relaxed));
        assert_eq!(Arc::strong_count(&owner), 1);
    }
}
