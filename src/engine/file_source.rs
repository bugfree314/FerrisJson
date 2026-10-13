//! Positioned snapshot reads with bounded concurrency and cancellable admission.
use std::{
    cell::RefCell,
    fs::File,
    io,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Clone)]
pub enum Cancellation {
    Any(Vec<Cancellation>),
    Flag(Arc<AtomicBool>),
    Generation(Arc<AtomicU64>, u64),
}
impl Cancellation {
    fn cancelled(&self) -> bool {
        match self {
            Self::Any(tokens) => tokens.iter().any(Self::cancelled),
            Self::Flag(flag) => flag.load(Ordering::Relaxed),
            Self::Generation(epoch, value) => epoch.load(Ordering::Relaxed) != *value,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Interactive,
    User,
    Background,
}
thread_local! {
    static CONTEXT: RefCell<Option<(Priority, Cancellation)>> = const { RefCell::new(None) };
}
pub fn with_io_context<T>(
    priority: Priority,
    cancellation: Cancellation,
    work: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<(Priority, Cancellation)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONTEXT.with(|context| *context.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CONTEXT.with(|context| context.replace(Some((priority, cancellation)))));
    work()
}
pub fn check_cancelled() -> io::Result<()> {
    if CONTEXT.with(|context| {
        context
            .borrow()
            .as_ref()
            .is_some_and(|(_, token)| token.cancelled())
    }) {
        Err(io::Error::other("File operation cancelled"))
    } else {
        Ok(())
    }
}
pub(crate) fn lock_cancellable<T>(mutex: &Mutex<T>) -> io::Result<std::sync::MutexGuard<'_, T>> {
    loop {
        check_cancelled()?;
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(io::Error::other("Cache lock poisoned"));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(5))
            }
        }
    }
}

#[derive(Default)]
struct State {
    active: usize,
    interactive_waiters: usize,
    user_waiters: usize,
}
struct Gate {
    state: Mutex<State>,
    signal: Condvar,
    limit: usize,
}
impl Gate {
    fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            signal: Condvar::new(),
            limit,
        }
    }
    fn acquire(&self, priority: Priority, cancelled: &dyn Fn() -> bool) -> io::Result<Permit<'_>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let interactive = priority == Priority::Interactive;
        let user = priority == Priority::User;
        if interactive {
            state.interactive_waiters += 1;
        }
        if user {
            state.user_waiters += 1;
        }
        loop {
            if cancelled() {
                if interactive {
                    state.interactive_waiters -= 1;
                }
                if user {
                    state.user_waiters -= 1;
                }
                self.signal.notify_all();
                return Err(io::Error::other("File admission cancelled"));
            }
            let preferred = interactive
                || (state.interactive_waiters == 0 && (user || state.user_waiters == 0));
            if state.active < self.limit && preferred {
                if interactive {
                    state.interactive_waiters -= 1;
                }
                if user {
                    state.user_waiters -= 1;
                }
                state.active += 1;
                return Ok(Permit(self));
            }
            state = self
                .signal
                .wait_timeout(state, Duration::from_millis(5))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
struct Permit<'a>(&'a Gate);
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active -= 1;
        self.0.signal.notify_all();
    }
}
pub(crate) fn with_io_slot<T>(
    cancelled: &dyn Fn() -> bool,
    work: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    static GATE: OnceLock<Gate> = OnceLock::new();
    let priority = CONTEXT.with(|context| {
        context
            .borrow()
            .as_ref()
            .map_or(Priority::Background, |(priority, _)| *priority)
    });
    let cancelled = || cancelled() || check_cancelled().is_err();
    let _permit = GATE
        .get_or_init(|| Gate::new(4))
        .acquire(priority, &cancelled)?;
    work()
}

pub(crate) struct FileSource {
    file: Option<File>,
}
impl FileSource {
    pub(crate) fn new(file: File) -> Self {
        Self { file: Some(file) }
    }
    pub(crate) fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("open snapshot")
    }
    pub(crate) fn close(&mut self) {
        self.file.take();
    }
    pub(crate) fn read_exact_at(&self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
        self.read_exact_at_cancellable(offset, bytes, &|| false)
    }
    pub(crate) fn read_exact_at_cancellable(
        &self,
        mut offset: u64,
        mut bytes: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::other("Snapshot closed"))?;
        while !bytes.is_empty() {
            check_cancelled()?;
            // Admission is released between blocks; an interactive read can pass
            // a long scan/export. Reads do not depend on a shared cursor.
            let length = bytes.len().min(64 * 1024);
            let result = with_io_slot(cancelled, || {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    file.read_at(&mut bytes[..length], offset)
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileExt;
                    file.seek_read(&mut bytes[..length], offset)
                }
                #[cfg(not(any(unix, windows)))]
                {
                    let _ = (file, length);
                    Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Positioned reads unavailable",
                    ))
                }
            });
            let count = match result {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                other => other?,
            };
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Snapshot truncated",
                ));
            }
            offset = offset
                .checked_add(count as u64)
                .ok_or_else(|| io::Error::other("Read offset overflow"))?;
            bytes = &mut bytes[count..];
        }
        if cancelled() {
            return Err(io::Error::other("File operation cancelled"));
        }
        check_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn waiting_admission_is_cancellable_and_returns_its_slot() {
        let gate = Arc::new(Gate::new(1));
        let permit = gate.acquire(Priority::Background, &|| false).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let other = Arc::clone(&gate);
        let flag = Arc::clone(&cancelled);
        let thread = std::thread::spawn(move || {
            other
                .acquire(Priority::Interactive, &|| flag.load(Ordering::Relaxed))
                .is_err()
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while gate.state.lock().unwrap().interactive_waiters == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        cancelled.store(true, Ordering::Relaxed);
        assert!(thread.join().unwrap());
        drop(permit);
        assert!(gate.acquire(Priority::Background, &|| false).is_ok());
    }
    #[test]
    fn io_context_is_restored_after_scope_exit() {
        let flag = Arc::new(AtomicBool::new(true));
        with_io_context(Priority::Interactive, Cancellation::Flag(flag), || {
            assert!(check_cancelled().is_err())
        });
        assert!(check_cancelled().is_ok());
    }
    #[test]
    fn cancellation_interrupts_waiting_for_a_cache_lock() {
        let mutex = Arc::new(Mutex::new(()));
        let held = mutex.lock().unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let pending = Arc::clone(&flag);
        let other = Arc::clone(&mutex);
        let thread = std::thread::spawn(move || {
            with_io_context(Priority::Interactive, Cancellation::Flag(pending), || {
                lock_cancellable(&other).is_err()
            })
        });
        flag.store(true, Ordering::Relaxed);
        assert!(thread.join().unwrap());
        drop(held);
        assert!(lock_cancellable(&mutex).is_ok());
    }
    #[test]
    fn positioned_reads_do_not_mix_offsets_and_handle_eof_and_cancel() {
        use std::io::Write;
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let input = Cleanup(std::env::temp_dir().join(format!(
            "FerrisJson-source-{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        )));
        let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        let mut output = File::create(&input.0).unwrap();
        output.write_all(&data).unwrap();
        drop(output);
        let source = Arc::new(FileSource::new(File::open(&input.0).unwrap()));
        let threads: Vec<_> = (0..8)
            .map(|lane| {
                let source = Arc::clone(&source);
                std::thread::spawn(move || {
                    for index in 0..20 {
                        let offset = lane * 10_000 + index * 37;
                        let mut buffer = vec![0; 70_000];
                        source.read_exact_at(offset as u64, &mut buffer).unwrap();
                        assert!(
                            buffer
                                .iter()
                                .enumerate()
                                .all(|(i, &byte)| byte == ((offset + i) % 251) as u8)
                        );
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            source
                .read_exact_at(199_999, &mut [0; 2])
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(
            source
                .read_exact_at_cancellable(0, &mut [0; 2], &|| true)
                .is_err()
        );
        drop(source);
    }
    #[test]
    fn admission_respects_interactive_user_and_background_priorities() {
        let gate = Arc::new(Gate::new(1));
        let held = gate.acquire(Priority::Background, &|| false).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let background = Arc::clone(&gate);
        let bg = tx.clone();
        let first = std::thread::spawn(move || {
            let _permit = background.acquire(Priority::Background, &|| false).unwrap();
            bg.send(0).unwrap();
        });
        let user = Arc::clone(&gate);
        let user_output = tx.clone();
        let middle = std::thread::spawn(move || {
            let _permit = user.acquire(Priority::User, &|| false).unwrap();
            user_output.send(1).unwrap();
        });
        let interactive = Arc::clone(&gate);
        let second = std::thread::spawn(move || {
            let _permit = interactive
                .acquire(Priority::Interactive, &|| false)
                .unwrap();
            tx.send(2).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = gate.state.lock().unwrap();
            if state.interactive_waiters > 0 && state.user_waiters > 0 {
                break;
            }
            drop(state);
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(held);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 0);
        first.join().unwrap();
        middle.join().unwrap();
        second.join().unwrap();
    }
}
