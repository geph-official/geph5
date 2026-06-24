//! A task "reaper" that cancels everything it owns when dropped, yet does not
//! leak handles to tasks that have already finished. A faithful reimplementation
//! of `smolscale::reaper` on top of [`crate::Task`].
//!
//! Note: a bare `tokio::task::JoinSet` is *not* a drop-in substitute — it needs
//! `&mut self` to spawn (this `attach` takes `&self`) and never reaps finished
//! tasks without active `join_next` calls.

use async_channel::{Receiver, Sender};
use futures_concurrency::future::Race;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;

use crate::Task;

/// Owns a set of detached tasks: finished tasks are dropped as they complete,
/// and all still-running tasks are cancelled when the reaper is dropped.
pub struct TaskReaper<T> {
    send_task: Sender<Task<T>>,
    _reaper: Task<()>,
}

impl<T: Send + 'static> Default for TaskReaper<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static> TaskReaper<T> {
    /// Creates a new reaper with its background driver task.
    pub fn new() -> Self {
        let (send_task, recv_task) = async_channel::unbounded();
        let _reaper = crate::spawn(reaper_loop(recv_task));
        Self { send_task, _reaper }
    }

    /// Attaches a task to this reaper, transferring ownership of its handle.
    pub fn attach(&self, task: Task<T>) {
        let _ = self.send_task.try_send(task);
    }
}

async fn reaper_loop<T: Send + 'static>(recv_task: Receiver<Task<T>>) {
    let mut inner: FuturesUnordered<Task<T>> = FuturesUnordered::new();
    loop {
        // Race receiving a new task against draining finished tasks. The drain
        // arm never resolves (it parks on `pending` after polling the set); its
        // only purpose is to drive attached tasks so completed ones are removed.
        let next = (async { recv_task.recv().await }, async {
            inner.next().await;
            std::future::pending::<Result<Task<T>, async_channel::RecvError>>().await
        })
            .race()
            .await;
        match next {
            Ok(task) => inner.push(task),
            // Channel closed: the reaper was dropped. Returning drops `inner`,
            // which aborts all still-running attached tasks.
            Err(_) => return,
        }
    }
}
