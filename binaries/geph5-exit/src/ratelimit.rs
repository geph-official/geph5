use std::{num::NonZeroU32, sync::Arc, time::Duration};

use governor::{DefaultDirectRateLimiter, Quota};

/// A generic rate limiter.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Option<Arc<DefaultDirectRateLimiter>>,
}

impl RateLimiter {
    /// Creates a new rate limiter with the given speed limit, in B/s
    pub fn new(limit_kb: u32, burst_kb: u32) -> Self {
        let limit = NonZeroU32::new((limit_kb + 1) * 1024).unwrap();
        let burst_size = NonZeroU32::new(burst_kb * 1024).unwrap();
        let inner = governor::RateLimiter::direct(Quota::per_second(limit).allow_burst(burst_size));
        Self {
            inner: Some(Arc::new(inner)),
        }
    }

    /// Creates a new unlimited ratelimit.
    pub fn unlimited() -> Self {
        Self { inner: None }
    }

    /// Waits until the given number of bytes can be let through.
    pub async fn wait(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        if let Some(inner) = &self.inner {
            while inner
                .check_n((bytes as u32).try_into().unwrap())
                .unwrap()
                .is_err()
            {
                smol::Timer::after(Duration::from_secs_f32(rand::random::<f32>() * 0.05)).await;
            }
        }
    }
}
