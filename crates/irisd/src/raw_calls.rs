//! Raw filesystem calls a maintenance owner runs on counted threads.
//!
//! An in-kernel `statvfs`, unlink, or directory sync cannot be cancelled, so a
//! maintenance job that times out leaves its thread running. [`RawCalls`]
//! counts those threads until they finish. The owner (`MaintenanceGuard` in
//! `run.rs`) starts no further job while the count is nonzero, which keeps at
//! most one such call per owner and never races a late removal.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// Raw filesystem calls a maintenance owner started on standard-library
/// threads and that have not returned. Clones share one count.
#[derive(Clone, Debug, Default)]
pub struct RawCalls(Arc<AtomicUsize>);

impl RawCalls {
    /// Calls whose threads have not finished.
    pub fn in_flight(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    /// Runs `operation` on a named standard-library thread, counted until the
    /// thread finishes. A thread that cannot be started is not counted.
    ///
    /// # Errors
    ///
    /// Returns the operating system's error when the thread cannot be
    /// created.
    pub fn spawn<T, F>(&self, name: &str, operation: F) -> io::Result<thread::JoinHandle<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let guard = RawCallGuard::enter(&self.0);
        thread::Builder::new().name(name.to_owned()).spawn(move || {
            let _guard = guard;
            operation()
        })
    }
}

/// Holds one count of [`RawCalls`] until the raw call's thread finishes or
/// fails to start.
struct RawCallGuard(Arc<AtomicUsize>);

impl RawCallGuard {
    fn enter(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(count))
    }
}

impl Drop for RawCallGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_is_counted_until_its_thread_finishes() {
        let raw = RawCalls::default();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = raw
            .spawn("irisd-test-raw", move || {
                let _ = release_rx.recv();
            })
            .unwrap();
        assert_eq!(raw.in_flight(), 1);
        assert_eq!(raw.clone().in_flight(), 1, "clones share the count");
        release_tx.send(()).unwrap();
        thread.join().unwrap();
        assert_eq!(raw.in_flight(), 0);
    }
}
