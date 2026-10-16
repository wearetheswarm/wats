//! Supervision of long-lived tasks (security review, item D "no crash").
//!
//! A panic in a tokio task does not bring the runtime down: the release
//! profile unwinds, and tokio catches the panic at the task boundary. But a
//! long-lived loop that panics is gone, and the feature it carried (the
//! gossip receive loop of a network, the MCP socket, the agent runner)
//! silently stops while the node looks alive. [`supervise`] runs such a loop
//! in its own task and restarts it after a panic, with a log line and a
//! short backoff.
//!
//! The crash-report panic hook (ADR 0013) still runs at the panic and keeps
//! its report; the log line written here says the node survived, so a reader
//! of the report's log tail can tell a restarted task from a process crash.

use std::any::Any;
use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::error;

/// First wait before a restart.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// Longest wait before a restart, for a task that keeps panicking. Short:
/// a stopping node may wait for it.
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// A task that ran this long before panicking restarts after
/// [`BACKOFF_MIN`] again.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(300);

/// Aborts the supervised task when the supervisor itself is dropped or
/// aborted, so that stopping the supervisor stops the task.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run `make()` in its own task, and run it again after a panic. Returns
/// the task's output once it ends normally, or `None` when it was
/// cancelled (the runtime is shutting down). Dropping or aborting the
/// returned future aborts the running task.
pub async fn supervise<F, Fut, T>(name: &str, mut make: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = tokio::time::Instant::now();
        let mut task = AbortOnDrop(tokio::spawn(make()));
        let error = match (&mut task.0).await {
            Ok(output) => return Some(output),
            Err(e) if e.is_panic() => e.into_panic(),
            Err(_) => return None,
        };
        if started.elapsed() >= BACKOFF_RESET_AFTER {
            backoff = BACKOFF_MIN;
        }
        error!(
            task = name,
            "task panicked ({}): restarting it in {}s, the node keeps running \
             (crash report: wats report show)",
            panic_message(error.as_ref()),
            backoff.as_secs()
        );
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(BACKOFF_MAX);
    }
}

/// The message of a panic payload.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn restarts_after_a_panic_and_returns_the_output() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        let output = supervise("test", move || {
            let counter = Arc::clone(&counter);
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                    panic!("boom");
                }
                42
            }
        })
        .await;
        assert_eq!(output, Some(42));
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn aborting_the_supervisor_aborts_the_task() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let mut tx = Some(tx);
        let supervisor = tokio::spawn(supervise("test", move || {
            let tx = tx.take();
            async move {
                let _tx = tx;
                std::future::pending::<()>().await;
            }
        }));
        tokio::task::yield_now().await;
        supervisor.abort();
        // The task held the sender: it is dropped once the task is aborted.
        assert!(rx.await.is_err());
    }
}
