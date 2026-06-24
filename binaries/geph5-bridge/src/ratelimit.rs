use std::{num::NonZeroU32, sync::Arc, time::Duration};

use governor::{DefaultDirectRateLimiter, Quota};
use rand::Rng;

const KIB: usize = 1024;
const TEN_GIB_IN_KIB: u32 = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct BridgeRateLimiter {
    inner: Option<Arc<DefaultDirectRateLimiter>>,
}

impl BridgeRateLimiter {
    pub fn limited(limit_kib_per_second: u32) -> Self {
        let limit = NonZeroU32::new(limit_kib_per_second).expect("rate limit must be non-zero");
        let burst = NonZeroU32::new(TEN_GIB_IN_KIB).unwrap();
        let inner = governor::RateLimiter::direct(Quota::per_second(limit).allow_burst(burst));
        inner
            .check_n(burst)
            .expect("startup burst must fit configured bridge rate limit")
            .expect("fresh bridge rate limiter should accept startup burst drain");
        Self {
            inner: Some(Arc::new(inner)),
        }
    }

    pub fn unlimited() -> Self {
        Self { inner: None }
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
        loop {
            if self
                .inner
                .as_ref()
                .unwrap()
                .check_n(tokens)
                .unwrap()
                .is_ok()
            {
                break;
            }

            tokio::time::sleep(Duration::from_secs_f32(delay)).await;
            delay += rand::random::<f32>() * 0.05;
        }
    }
}

fn stochastic_kib_charge(bytes: usize) -> u32 {
    let sample = rand::thread_rng().gen_range(0..KIB);
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
