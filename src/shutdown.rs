//! Finish temporary-output cleanup after the native window has closed.
use std::sync::Mutex;
use std::thread::JoinHandle;
static CLEANUP: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());
pub fn defer(thread: JoinHandle<()>) {
    CLEANUP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(thread);
}
pub fn finish() {
    let threads = std::mem::take(&mut *CLEANUP.lock().unwrap_or_else(|e| e.into_inner()));
    for thread in threads {
        let _ = thread.join();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_can_be_deferred_until_after_the_event_loop() {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        defer(std::thread::spawn(move || {
            rx.recv().unwrap();
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }));
        assert!(!done.load(std::sync::atomic::Ordering::Relaxed));
        tx.send(()).unwrap();
        finish();
        assert!(done.load(std::sync::atomic::Ordering::Relaxed));
    }
}
