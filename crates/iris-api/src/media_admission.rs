//! Cancellation-safe ownership helpers for process-wide media admission.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::OwnedSemaphorePermit;
use tokio::task::{JoinError, JoinHandle};

/// One shared process-wide media-admission slot.
pub(crate) type MediaPermit = Arc<OwnedSemaphorePermit>;

/// Shares a freshly acquired admission slot with work and response owners.
pub(crate) fn shared(permit: OwnedSemaphorePermit) -> MediaPermit {
    Arc::new(permit)
}

/// Runs blocking work while retaining media admission even if its waiter is cancelled.
pub(crate) async fn blocking<F, R>(permit: &MediaPermit, task: F) -> Result<R, JoinError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let permit = Arc::clone(permit);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        task()
    })
    .await
}

/// Detaches asynchronous work that retains admission until its full future ends.
pub(crate) fn spawn<F>(permit: &MediaPermit, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let permit = Arc::clone(permit);
    tokio::spawn(async move {
        let _permit = permit;
        future.await
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::state::{ApiStats, MAX_CONCURRENT_MEDIA_RESPONSES};

    #[tokio::test]
    async fn cancelled_waiters_do_not_release_running_blocking_work() {
        let stats = Arc::new(ApiStats::default());
        let running = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let mut waiters = Vec::new();

        for expected in 1..=MAX_CONCURRENT_MEDIA_RESPONSES {
            let permit = shared(stats.try_media_response().expect("slot within the limit"));
            let running_work = Arc::clone(&running);
            let release_work = Arc::clone(&release);
            waiters.push(tokio::spawn(async move {
                let _ = blocking(&permit, move || {
                    running_work.fetch_add(1, Ordering::SeqCst);
                    while !release_work.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                })
                .await;
            }));
            tokio::time::timeout(Duration::from_secs(2), async {
                while running.load(Ordering::SeqCst) < expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("blocking work started");
        }

        for waiter in waiters {
            waiter.abort();
            let _ = waiter.await;
        }
        assert!(
            stats.try_media_response().is_none(),
            "detached blocking work must retain every admission slot"
        );

        release.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(permit) = stats.try_media_response() {
                    drop(permit);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admission returned after blocking work ended");
    }

    #[tokio::test]
    async fn detached_async_work_retains_admission_until_completion() {
        let stats = Arc::new(ApiStats::default());
        let permit = shared(stats.try_media_response().expect("first slot"));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let worker = spawn(&permit, async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
        });
        started_rx.await.expect("detached work started");
        drop(worker);
        drop(permit);

        let remaining: Vec<_> = (1..MAX_CONCURRENT_MEDIA_RESPONSES)
            .map(|_| stats.try_media_response().expect("remaining slot"))
            .collect();
        assert!(
            stats.try_media_response().is_none(),
            "detached async work must retain its original slot"
        );

        release_tx.send(()).expect("detached work is waiting");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(permit) = stats.try_media_response() {
                    drop(permit);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached work returned its admission slot");
        drop(remaining);
    }
}
