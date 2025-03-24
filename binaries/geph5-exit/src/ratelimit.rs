use std::{
    num::NonZeroU32,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use async_io_bufpool::pooled_read;
use atomic_float::AtomicF32;
use futures_util::{AsyncRead, AsyncWrite, AsyncWriteExt};
use geph5_broker_protocol::AccountLevel;
use governor::{DefaultDirectRateLimiter, Quota};
use mizaru2::ClientToken;
use moka::future::Cache;
use once_cell::sync::Lazy;
use smol_timeout2::TimeoutExt;
use stdcode::StdcodeSerializeExt;
use sysinfo::System;

use crate::CONFIG_FILE;

static FREE_RL_CACHE: Lazy<Cache<blake3::Hash, RateLimiter>> = Lazy::new(|| {
    Cache::builder()
        .time_to_idle(Duration::from_secs(86400))
        .build()
});

static PLUS_RL_CACHE: Lazy<Cache<blake3::Hash, RateLimiter>> = Lazy::new(|| {
    Cache::builder()
        .time_to_idle(Duration::from_secs(86400))
        .build()
});

static CPU_USAGE: Lazy<AtomicF32> = Lazy::new(|| AtomicF32::new(0.0));
static CURRENT_SPEED: Lazy<AtomicF32> = Lazy::new(|| AtomicF32::new(0.0));

pub fn get_load() -> f32 {
    // we weigh CPU usage lower until it's really close to massively overloading
    let cpu = CPU_USAGE.load(Ordering::Relaxed).powi(2);
    let speed = CURRENT_SPEED.load(Ordering::Relaxed)
        / (CONFIG_FILE.wait().total_ratelimit as f32 * 1000.0);
    cpu.max(speed)
}

pub fn get_kbps() -> f32 {
    CURRENT_SPEED.load(Ordering::Relaxed) / 1000.0
}

pub static TOTAL_BYTE_COUNT: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(0));

pub fn update_load_loop() {
    let mut sys = System::new_all();
    let mut cpu_accum = 0.0;
    let mut last_count_time = Instant::now();
    let mut last_byte_count = 0;
    let mut last_speed = 0.0;
    loop {
        for _ in 0..100 {
            sys.refresh_all();
            let cpu_usage: f32 = sys.cpus().iter().map(|cpu| cpu.cpu_usage()).sum::<f32>()
                / sys.cpus().len() as f32
                / 100.0;
            cpu_accum = cpu_accum * 0.99 + cpu_usage * 0.01;

            CPU_USAGE.store(cpu_accum, Ordering::Relaxed);
            let new_byte_count = TOTAL_BYTE_COUNT.load(Ordering::Relaxed);
            let byte_diff = new_byte_count - last_byte_count;
            let byte_rate = byte_diff as f32 / last_count_time.elapsed().as_secs_f32();
            last_speed = last_speed * 0.99 + byte_rate * 0.01; // Exponential decay for current speed
            CURRENT_SPEED.store(last_speed, Ordering::Relaxed);

            last_count_time = Instant::now();
            last_byte_count = new_byte_count;

            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::info!(load = get_load(), "updated load");
    }
}

pub async fn get_ratelimiter(level: AccountLevel, token: ClientToken) -> RateLimiter {
    match level {
        AccountLevel::Free => {
            FREE_RL_CACHE
                .get_with(blake3::hash(&(level, token).stdcode()), async {
                    RateLimiter::new(CONFIG_FILE.wait().free_ratelimit, 100)
                })
                .await
        }
        AccountLevel::Plus => {
            PLUS_RL_CACHE
                .get_with(blake3::hash(&(level, token).stdcode()), async {
                    RateLimiter::new(CONFIG_FILE.wait().plus_ratelimit, 100)
                })
                .await
        }
    }
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
        TOTAL_BYTE_COUNT.fetch_add(bytes as _, Ordering::Relaxed);
        if bytes == 0 {
            return;
        }
        let multiplier = (1.0 / (1.0 - get_load().min(0.999)) - 1.0) / 2.0;

        let bytes = (bytes as f32 * (multiplier.max(1.0))).min(100000.0);
        if let Some(inner) = &self.inner {
            let mut delay: f32 = 0.005;
            while inner
                .check_n((bytes as u32).try_into().unwrap())
                .unwrap()
                .is_err()
            {
                smol::Timer::after(Duration::from_secs_f32(delay)).await;
                delay += rand::random::<f32>() * 0.05;
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

        loop {
            let bts = pooled_read(&mut read_stream)
                .timeout(Duration::from_secs(1800))
                .await
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout in TCP read")
                })??;
            if bts.is_empty() {
                break;
            }

            self.wait(bts.len()).await;

            write_stream.write_all(&bts).await?;
            total_bytes += bts.len() as u64;
        }

        Ok(total_bytes)
    }
}
