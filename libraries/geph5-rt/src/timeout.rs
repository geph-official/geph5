//! A `.timeout()` future combinator backed by `tokio::time`, preserving the
//! `smol-timeout2` contract (returns `None` on timeout) so existing call sites
//! need only swap the import.

use std::future::Future;
use std::time::Duration;

/// Extension trait adding a `.timeout(duration)` combinator to any future.
///
/// Returns `Some(output)` if the future completes in time, or `None` if it
/// elapses — matching `smol_timeout2::TimeoutExt`.
pub trait TimeoutExt: Future + Sized {
    fn timeout(self, duration: Duration) -> impl Future<Output = Option<Self::Output>> {
        async move { tokio::time::timeout(duration, self).await.ok() }
    }
}

impl<F: Future> TimeoutExt for F {}
