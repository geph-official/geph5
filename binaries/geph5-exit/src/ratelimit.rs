use std::{num::NonZeroU32, sync::Arc, time::Duration};

use futures_util::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use geph5_broker_protocol::AccountLevel;
use governor::{DefaultDirectRateLimiter, Quota};
use mizaru2::ClientToken;
use moka::future::Cache;
use once_cell::sync::Lazy;
use stdcode::StdcodeSerializeExt;

static RL_CACHE: Lazy<Cache<blake3::Hash, RateLimiter>> = Lazy::new(|| {
    Cache::builder()
        .time_to_idle(Duration::from_secs(86400))
        .build()
});

pub async fn get_ratelimiter(level: AccountLevel, token: ClientToken) -> RateLimiter {
    RL_CACHE
        .get_with(blake3::hash(&(level, token).stdcode()), async {
            RateLimiter::new(1000, 10_000_000)
        })
        .await
}

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

    /// Copy one stream to another, rate-limited by this rate limit.
    pub async fn io_copy(
        &self,
        mut read_stream: impl AsyncRead + Unpin,
        mut write_stream: impl AsyncWrite + Unpin,
    ) -> std::io::Result<u64> {
        let mut total_bytes = 0;
        let mut buf = [0u8; 8192];

        loop {
            let bytes_read = read_stream.read(&mut buf).await?;
            if bytes_read == 0 {
                break;
            }

            self.wait(bytes_read).await;

            write_stream.write_all(&buf[..bytes_read]).await?;
            total_bytes += bytes_read as u64;
        }

        Ok(total_bytes)
    }
}
