use std::time::{Duration, Instant};
use std::{collections::VecDeque, sync::RwLock};

use crate::client::CtxField;

pub static TRAFF_COUNT: CtxField<RwLock<TraffCount>> = |_| RwLock::new(TraffCount::new());

/// A structure that maintains a history of traffic volumes.
/// Used to calculate speeds based on traffic over time.
pub struct TraffCount {
    /// Per-second bins for traffic measurements
    bins: VecDeque<f64>,
    /// When the current time window started (for the oldest bin)
    window_start: Instant,
    /// Maximum history length to maintain (in seconds)
    max_history_seconds: usize,
}

impl TraffCount {
    /// Create a new traffic counter with default settings
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            bins: VecDeque::with_capacity(600), // Pre-allocate bins for a minute
            window_start: now,
            max_history_seconds: 600, // Keep up to a minute of history by default
        }
    }

    /// Create a new traffic counter with custom history length
    pub fn with_history(max_seconds: usize) -> Self {
        let now = Instant::now();
        Self {
            bins: VecDeque::with_capacity(max_seconds),
            window_start: now,
            max_history_seconds: max_seconds,
        }
    }

    /// Increment the traffic count with the given number of bytes
    pub fn incr(&mut self, bytes: f64) {
        let now = Instant::now();
        self.ensure_bins_updated(now);

        // Add the new bytes to the most recent bin
        if let Some(last) = self.bins.back_mut() {
            *last += bytes;
        } else if bytes > 0.0 {
            // If there are no bins yet but we have traffic, create the first bin
            self.bins.push_back(bytes);
        }
    }

    /// Ensure the bins array is up-to-date with current time
    fn ensure_bins_updated(&mut self, now: Instant) {
        // Calculate how many seconds have passed since our window started
        let seconds_elapsed = now.duration_since(self.window_start).as_secs() as usize;

        if seconds_elapsed == 0 && !self.bins.is_empty() {
            // Still in the same second, no need to add bins
            return;
        }

        if self.bins.is_empty() {
            // Initialize with a single bin if empty
            self.bins.push_back(0.0);
            return;
        }

        if seconds_elapsed >= self.max_history_seconds {
            // Too much time has passed, reset everything
            self.bins.clear();
            self.window_start = now;
            self.bins.push_back(0.0);
            return;
        }

        // Add new bins for elapsed seconds
        for _ in 0..seconds_elapsed {
            // If we exceed our max history, remove the oldest bin
            if self.bins.len() >= self.max_history_seconds {
                self.bins.pop_front();
            }
            // Add a new empty bin
            self.bins.push_back(0.0);
        }

        // Update the window start time to account for the bins we've added
        self.window_start += Duration::from_secs(seconds_elapsed as u64);
    }

    /// Gets a vector of speeds binned per second, in bytes per second
    pub fn speed_history(&self) -> Vec<f64> {
        // Return bins directly - they're already per-second
        self.bins.iter().cloned().collect()
    }

    /// Remove measurements that are too old
    fn cleanup(&mut self) {
        let now = Instant::now();
        self.ensure_bins_updated(now);
    }
}

impl Default for TraffCount {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_traffic_count_basic() {
        let mut counter = TraffCount::new();

        // Add some traffic
        counter.incr(100.0);
        counter.incr(200.0);

        // We should have some history
        let history = counter.speed_history();
        assert!(!history.is_empty());

        // The most recent entry should contain our traffic
        assert!(history.last().unwrap() > &0.0);
    }

    #[test]
    fn test_traffic_cleanup() {
        let mut counter = TraffCount::with_history(2); // Only keep 2 seconds of history

        // Add initial traffic
        counter.incr(100.0);

        // Sleep to make the first measurement old
        sleep(Duration::from_secs(3));

        // Add new traffic
        counter.incr(200.0);

        // After sleeping for 3 seconds and adding more traffic,
        // the history should be refreshed and the old bin removed
        assert_eq!(counter.bins.len(), 1);
    }
}
