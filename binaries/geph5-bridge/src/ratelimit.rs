use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use governor::{DefaultDirectRateLimiter, Quota, middleware::StateInformationMiddleware};
use rand::Rng;

const KIB: usize = 1024;
const TEN_GIB_IN_KIB: u32 = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct BridgeRateLimiter {
    inner: Option<Arc<DefaultDirectRateLimiter<StateInformationMiddleware>>>,
    pressure: Arc<Mutex<Pressure>>,
    limit: u32,
}

impl BridgeRateLimiter {
    pub fn limited(limit_kib_per_second: u32) -> Self {
        let limit = NonZeroU32::new(limit_kib_per_second).expect("rate limit must be non-zero");
        let burst = NonZeroU32::new(TEN_GIB_IN_KIB).unwrap();
        let inner = governor::RateLimiter::direct(Quota::per_second(limit).allow_burst(burst))
            .with_middleware::<StateInformationMiddleware>();
        inner
            .check_n(burst)
            .expect("startup burst must fit configured bridge rate limit")
            .expect("fresh bridge rate limiter should accept startup burst drain");
        Self {
            inner: Some(Arc::new(inner)),
            pressure: Arc::new(Mutex::new(Pressure::new(Instant::now()))),
            limit: limit_kib_per_second,
        }
    }

    pub fn unlimited() -> Self {
        Self {
            inner: None,
            pressure: Arc::new(Mutex::new(Pressure::new(Instant::now()))),
            limit: 0,
        }
    }

    pub fn should_advertise(&self) -> bool {
        if self.inner.is_none() {
            return true;
        }
        self.pressure
            .lock()
            .unwrap()
            .advertise(Instant::now(), self.limit)
    }

    pub async fn wait(&self, bytes: usize) {
        if self.inner.is_none() {
            return;
        }

        let mut tokens = stochastic_kib_charge(bytes);
        while tokens > 0 {
            let chunk = tokens.min(TEN_GIB_IN_KIB);
            self.wait_tokens(chunk).await;
            tokens -= chunk;
        }
    }

    async fn wait_tokens(&self, tokens: u32) {
        let tokens = NonZeroU32::new(tokens).unwrap();
        let mut delay = 0.005;
        let mut waiting = None;
        loop {
            {
                // Serialize decisions and snapshots so concurrent transfers cannot
                // overwrite newer credit observations with older ones.
                let mut pressure = self.pressure.lock().unwrap();
                let now = Instant::now();
                pressure.advance(now);
                if let Ok(snapshot) = self.inner.as_ref().unwrap().check_n(tokens).unwrap() {
                    pressure.credit = snapshot.remaining_burst_capacity();
                    pressure.credit_at = now;
                    break;
                }
                if waiting.is_none() {
                    pressure.waiters += 1;
                    waiting = Some(Waiter(self.pressure.clone()));
                }
            }

            tokio::time::sleep(Duration::from_secs_f32(delay)).await;
            delay += rand::random::<f32>() * 0.05;
        }
    }
}

const WINDOW: Duration = Duration::from_secs(30);

struct Pressure {
    started: Instant,
    updated: Instant,
    // Fixed-size buckets bound memory even with many short waits.
    buckets: [Duration; 31],
    second: u64,
    waiters: usize,
    quiet_since: Instant,
    suspended: bool,
    credit: u32,
    credit_at: Instant,
}

impl Pressure {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            updated: now,
            buckets: [Duration::ZERO; 31],
            second: 0,
            waiters: 0,
            quiet_since: now,
            suspended: false,
            credit: 0,
            credit_at: now,
        }
    }

    fn advance(&mut self, now: Instant) {
        // Discard history older than the window without iterating over long idle gaps.
        if now.duration_since(self.updated) > Duration::from_secs(31) {
            self.updated = now - Duration::from_secs(31);
            self.second = self.updated.duration_since(self.started).as_secs();
            self.buckets.fill(Duration::ZERO);
        }
        while self.updated < now {
            let second = self.updated.duration_since(self.started).as_secs();
            if second != self.second {
                self.second = second;
                self.buckets[second as usize % 31] = Duration::ZERO;
            }
            let end = now.min(self.started + Duration::from_secs(second + 1));
            if self.waiters > 0 {
                self.buckets[second as usize % 31] += end - self.updated;
            }
            self.updated = end;
        }
    }

    fn advertise(&mut self, now: Instant, limit: u32) -> bool {
        self.advance(now);
        // Use the last 30 complete seconds; detection lags by at most one second.
        let second = now.duration_since(self.started).as_secs();
        let blocked: Duration = (second.saturating_sub(30)..second)
            .map(|s| self.buckets[s as usize % 31])
            .sum();
        let fraction = blocked.as_secs_f64() / 30.0;
        let before = self.suspended;
        if !self.suspended && now - self.started >= WINDOW && fraction >= 0.9 {
            self.suspended = true;
        } else if self.suspended && self.waiters == 0 && now - self.quiet_since >= WINDOW {
            let credit = (self.credit as f64 + (now - self.credit_at).as_secs_f64() * limit as f64)
                .min(TEN_GIB_IN_KIB as f64);
            // Very large configured rates can make ten seconds exceed bucket capacity.
            let reserve = (u64::from(limit) * 10).min(u64::from(TEN_GIB_IN_KIB));
            if credit >= reserve as f64 {
                self.suspended = false;
            }
        }
        if before != self.suspended {
            tracing::info!(
                suspended = self.suspended,
                blocked_fraction = fraction,
                "bridge advertising state changed"
            );
        }
        !self.suspended
    }
}

// Dropping a cancelled transfer must clear its contribution to pressure.
struct Waiter(Arc<Mutex<Pressure>>);
impl Drop for Waiter {
    fn drop(&mut self) {
        let mut pressure = self.0.lock().unwrap();
        let now = Instant::now();
        pressure.advance(now);
        pressure.waiters -= 1;
        if pressure.waiters == 0 {
            pressure.quiet_since = now;
        }
    }
}

fn stochastic_kib_charge(bytes: usize) -> u32 {
    let sample = rand::rng().random_range(0..KIB);
    stochastic_kib_charge_with_sample(bytes, sample)
}

pub(crate) fn stochastic_kib_charge_with_sample(bytes: usize, sample: usize) -> u32 {
    debug_assert!(sample < KIB);

    let whole = bytes / KIB;
    let remainder = bytes % KIB;
    let extra = usize::from(sample < remainder);
    whole.saturating_add(extra).min(u32::MAX as usize) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sustained_pressure_and_recovery() {
        let start = Instant::now();
        let mut p = Pressure::new(start);
        p.waiters = 2; // Overlapping waits contribute only wall-clock time.
        assert!(p.advertise(start + Duration::from_secs(29), 100));
        assert!(!p.advertise(start + Duration::from_secs(30), 100));
        p.waiters = 0;
        p.quiet_since = start + Duration::from_secs(30);
        p.credit_at = start + Duration::from_secs(59);
        assert!(!p.advertise(start + Duration::from_secs(60), 100));
        assert!(p.advertise(start + Duration::from_secs(69), 100));
    }

    #[test]
    fn short_pressure_and_long_idle_keep_advertising() {
        let start = Instant::now();
        let mut p = Pressure::new(start);
        p.waiters = 1;
        p.advance(start + Duration::from_secs(26));
        p.waiters = 0;
        assert!(p.advertise(start + Duration::from_secs(30), 100));
        assert!(p.advertise(start + Duration::from_secs(300), 100));
        assert_eq!(p.buckets.iter().sum::<Duration>(), Duration::ZERO);
    }

    #[test]
    fn long_sustained_wait_is_detected_after_idle_gap() {
        let start = Instant::now();
        let mut p = Pressure::new(start);
        p.waiters = 1;
        assert!(!p.advertise(start + Duration::from_secs(300), 100));
    }

    #[test]
    fn dropping_waiter_clears_pressure() {
        let limiter = BridgeRateLimiter::limited(100);
        limiter.pressure.lock().unwrap().waiters = 1;
        let waiter = Waiter(limiter.pressure.clone());
        drop(waiter);
        assert_eq!(limiter.pressure.lock().unwrap().waiters, 0);
        assert!(BridgeRateLimiter::unlimited().should_advertise());
    }

    #[test]
    fn stochastic_kib_charge_handles_empty_and_exact_kib() {
        assert_eq!(stochastic_kib_charge_with_sample(0, 0), 0);
        assert_eq!(stochastic_kib_charge_with_sample(1024, 0), 1);
        assert_eq!(stochastic_kib_charge_with_sample(1024, 1023), 1);
        assert_eq!(stochastic_kib_charge_with_sample(2048, 512), 2);
    }

    #[test]
    fn stochastic_kib_charge_uses_remainder_as_probability() {
        assert_eq!(stochastic_kib_charge_with_sample(1, 0), 1);
        assert_eq!(stochastic_kib_charge_with_sample(1, 1), 0);
        assert_eq!(stochastic_kib_charge_with_sample(1536, 511), 2);
        assert_eq!(stochastic_kib_charge_with_sample(1536, 512), 1);
        assert_eq!(stochastic_kib_charge_with_sample(1536, 1023), 1);
    }

    #[test]
    fn limited_rate_limiter_starts_empty() {
        let limiter = BridgeRateLimiter::limited(1);
        let token = NonZeroU32::new(1).unwrap();
        let decision = limiter.inner.as_ref().unwrap().check_n(token).unwrap();
        assert!(decision.is_err());
    }
}
