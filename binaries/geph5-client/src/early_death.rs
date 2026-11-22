use std::{collections::HashMap, net::SocketAddr, time::Duration};

use parking_lot::Mutex;
use rand::Rng;

use crate::client::CtxField;

pub(crate) static EARLY_DEATHS: CtxField<EarlyDeathTracker> = |_| EarlyDeathTracker::default();

#[derive(Default)]
pub(crate) struct EarlyDeathTracker {
    counts: Mutex<HashMap<SocketAddr, u8>>,
}

impl EarlyDeathTracker {
    pub(crate) fn record(&self, addr: SocketAddr, lifetime: Duration) {
        if lifetime >= Duration::from_secs(5 * 60) {
            return;
        }
        let mut counts = self.counts.lock();
        let count = counts.entry(addr).or_default();
        *count = count.saturating_add(1);
        tracing::debug!(%addr, ?lifetime, count = *count, "recorded early connection death");
    }

    pub(crate) fn delay(&self, addr: SocketAddr) -> Option<Duration> {
        let count = self.counts.lock().get(&addr).copied().unwrap_or_default();
        (count > 2).then(|| Duration::from_millis(rand::rng().random_range(1000..=2000)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_third_short_lived_connection_penalizes_its_endpoint() {
        let tracker = EarlyDeathTracker::default();
        let addr = SocketAddr::from(([127, 0, 0, 1], 443));
        tracker.record(addr, Duration::from_secs(300));
        for _ in 0..2 {
            tracker.record(addr, Duration::from_millis(299_999));
            assert!(tracker.delay(addr).is_none());
        }
        tracker.record(addr, Duration::ZERO);
        assert!((1000..=2000).contains(&tracker.delay(addr).unwrap().as_millis()));
        assert!(tracker.delay(SocketAddr::new(addr.ip(), 444)).is_none());
    }
}
