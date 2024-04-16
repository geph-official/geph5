use std::time::{Duration, Instant};

use poll_promise::Promise;

pub struct RefreshCell<T: Send + Sync + 'static> {
    last_updated: Instant,
    stable: Option<T>,
    next: Option<Promise<T>>,
}

impl<T: Send + Sync + 'static> RefreshCell<T> {
    /// Creates a new RefreshCell.
    pub fn new() -> Self {
        Self {
            last_updated: Instant::now(),
            stable: None,
            next: None,
        }
    }

    /// Gets a value from the RefreshCell. If there's no value, starts refreshing it, but returns the stale value in the meantime.
    pub fn get_or_refresh(
        &mut self,
        timeout: Duration,
        refresh: impl FnOnce() -> T + Send + 'static,
    ) -> Option<&T> {
        let must_refresh = self.next.is_none() || self.last_updated.elapsed() > timeout;
        if must_refresh {
            if let Some(Ok(res)) = self.next.take().map(|taken| taken.try_take()) {
                self.stable = Some(res);
            }
            self.next = Some(Promise::spawn_thread("refresh_cell", refresh));
            self.last_updated = Instant::now();
        }

        if self.last_updated.elapsed() > timeout * 2 {
            return None;
        }

        self.next.as_ref().unwrap().ready().or(self.stable.as_ref())
    }
}
