//! tokio-based runtime helpers for geph5.
//!
//! This crate replaces the smol/smolscale runtime surface (spawning, blocking,
//! timeouts, the immortal/respawn pattern, and the task reaper) while preserving
//! the property the codebase relies on for structured concurrency: smol's
//! `Task<T>` **cancels the task when the handle is dropped**. tokio's
//! `JoinHandle` detaches on drop instead, so [`Task`] reintroduces drop-cancel.

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::task::{Context, Poll, ready};

use tokio::runtime::{Builder, Runtime};
use tokio::task::JoinHandle;

#[cfg(unix)]
pub mod asyncfd;
mod bufpool;
pub mod immortal;
pub mod reaper;
mod timeout;

pub use bufpool::{pooled_read, pooled_read_callback};
pub use immortal::{Immortal, RespawnStrategy};
pub use reaper::TaskReaper;
pub use timeout::TimeoutExt;

// Centralized time helpers, so callers don't each reach into `tokio::time`.
pub use tokio::time::{sleep, sleep_until};

/// The global multi-threaded tokio runtime that drives all geph5 async work.
///
/// Lazily initialized on first use, mirroring smolscale's global executor so
/// that [`spawn`] and [`block_on`] work from any thread — including foreign FFI
/// threads that are not themselves runtime workers.
static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("could not build the global tokio runtime")
});

/// Returns a cloneable handle to the global runtime.
pub fn handle() -> tokio::runtime::Handle {
    RUNTIME.handle().clone()
}

/// Spawns a future onto the global runtime, returning a cancel-on-drop handle.
pub fn spawn<F>(future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Task(Some(RUNTIME.spawn(future)))
}

/// Runs a blocking closure on the global runtime's blocking pool.
pub fn spawn_blocking<F, R>(f: F) -> Task<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    Task(Some(RUNTIME.spawn_blocking(f)))
}

/// Blocks the current thread on a future, driving it on the global runtime.
///
/// Must **not** be called from within a runtime worker thread (tokio panics on
/// nested block-on). Use it only at process entry points, FFI boundaries, and
/// tests.
pub fn block_on<F: Future>(future: F) -> F::Output {
    RUNTIME.block_on(future)
}

/// A cancel-on-drop task handle, mirroring `smol::Task`.
///
/// * Dropping the handle aborts the underlying task (structured concurrency).
/// * `.await`ing it yields the task's output `T` directly, propagating a panic
///   if the task panicked.
/// * [`Task::detach`] lets it run to completion in the background instead.
#[must_use = "dropping a Task cancels the task; use .detach() to let it run in the background"]
pub struct Task<T>(Option<JoinHandle<T>>);

impl<T> Task<T> {
    /// Detaches the task, letting it run to completion in the background
    /// (equivalent to `smol::Task::detach`).
    pub fn detach(mut self) {
        self.0.take();
    }

    /// Aborts the task and waits until it has fully stopped.
    ///
    /// Prefer simply dropping the handle unless you must wait for the task to
    /// finish stopping before continuing.
    pub async fn cancel(mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let handle = self
            .0
            .as_mut()
            .expect("Task polled after being detached or cancelled");
        match ready!(Pin::new(handle).poll(cx)) {
            Ok(value) => Poll::Ready(value),
            Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
            // The task was aborted while still being awaited. This cannot happen
            // in the normal drop-cancel flow (nobody awaits an aborted handle);
            // it only arises from explicit cancel paths, where staying pending
            // is the least-surprising behavior.
            Err(_) => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn drop_cancels_the_task() {
        block_on(async {
            let flag = Arc::new(AtomicBool::new(false));
            let f2 = flag.clone();
            let task = spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                f2.store(true, Ordering::SeqCst);
            });
            drop(task);
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                !flag.load(Ordering::SeqCst),
                "a dropped Task must be cancelled"
            );
        });
    }

    #[test]
    fn detach_runs_to_completion() {
        block_on(async {
            let flag = Arc::new(AtomicBool::new(false));
            let f2 = flag.clone();
            spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                f2.store(true, Ordering::SeqCst);
            })
            .detach();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                flag.load(Ordering::SeqCst),
                "a detached Task must run to completion"
            );
        });
    }

    #[test]
    fn await_yields_output() {
        block_on(async {
            let task = spawn(async { 42u32 });
            assert_eq!(task.await, 42);
        });
    }

    #[test]
    fn timeout_returns_none_on_elapse() {
        block_on(async {
            let r = tokio::time::sleep(Duration::from_secs(10))
                .timeout(Duration::from_millis(50))
                .await;
            assert!(r.is_none());
        });
    }

    #[test]
    fn reaper_cancels_on_drop() {
        block_on(async {
            let flag = Arc::new(AtomicBool::new(false));
            let f2 = flag.clone();
            let reaper = TaskReaper::new();
            reaper.attach(spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                f2.store(true, Ordering::SeqCst);
            }));
            // give the reaper a moment to receive the task, then drop it
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(reaper);
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                !flag.load(Ordering::SeqCst),
                "dropping the reaper must cancel attached tasks"
            );
        });
    }
}
