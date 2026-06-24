//! Long-lived, auto-restarting tasks. A faithful reimplementation of
//! `smolscale::immortal` on top of the global tokio runtime.

use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

use rand::Rng;

use crate::Task;

/// An `Immortal` represents a task that never stops on its own — it is only
/// cancelled when explicitly cancelled or when this handle is dropped (it holds
/// a cancel-on-drop [`Task`]).
pub struct Immortal(Task<Infallible>);

impl Immortal {
    /// Directly spawns an immortal future (one whose output is `Infallible`,
    /// i.e. that never returns).
    pub fn spawn<F: Future<Output = Infallible> + Send + 'static>(f: F) -> Self {
        Self(crate::spawn(f))
    }

    /// Spawns an immortal that runs a piece of code repeatedly, restarting it
    /// whenever it returns, according to `strategy`.
    pub fn respawn<T, F>(
        strategy: RespawnStrategy,
        mut inner: impl FnMut() -> F + Send + 'static,
    ) -> Self
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let task = crate::spawn(async move {
            loop {
                inner().await;
                match strategy {
                    RespawnStrategy::Immediate => tokio::task::yield_now().await,
                    RespawnStrategy::FixedDelay(delay) => tokio::time::sleep(delay).await,
                    RespawnStrategy::JitterDelay(low, high) => {
                        let low = low.min(high);
                        let millis = rand::thread_rng()
                            .gen_range((low.as_millis() as u64)..=(high.as_millis() as u64));
                        tokio::time::sleep(Duration::from_millis(millis)).await;
                    }
                }
            }
        });
        Self(task)
    }

    /// Takes ownership of the immortal and cancels it, waiting until it has
    /// fully stopped. Dropping the handle is usually simpler.
    pub async fn cancel(self) {
        self.0.cancel().await;
    }
}

/// How an [`Immortal::respawn`] loop waits between restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RespawnStrategy {
    /// Restart immediately (yielding once to the scheduler).
    Immediate,
    /// Wait a fixed delay before restarting.
    FixedDelay(Duration),
    /// Wait a random delay in `[low, high]` before restarting.
    JitterDelay(Duration, Duration),
}
