use std::{thread::available_parallelism, time::Duration};

use anyhow::Context;
use cadence::Gauged;

use crate::{database::POSTGRES, rpc_impl::STATSD_CLIENT};

pub async fn self_stat_loop() -> anyhow::Result<()> {
    let ip_addr = String::from_utf8_lossy(
        &reqwest::get("http://checkip.amazonaws.com/")
            .await?
            .bytes()
            .await?,
    )
    .trim()
    .to_string()
    .replace(".", "-");
    loop {
        if let Some(client) = STATSD_CLIENT.as_ref() {
            let load_avg: f64 = std::fs::read_to_string("/proc/loadavg")?
                .split_ascii_whitespace()
                .next()
                .context("no first")?
                .parse()?;
            client.gauge(
                &format!("broker.{ip_addr}.nmlz_load_factor"),
                load_avg / available_parallelism().unwrap().get() as f64,
            )?;

            let pool_counts: Vec<(String, i64)> =
                sqlx::query_as("select pool,count(listen) from bridges_new group by pool")
                    .fetch_all(&*POSTGRES)
                    .await?;
            tracing::debug!("pool_counts: {:?}", pool_counts);
            for (pool, count) in pool_counts {
                client.gauge(&format!("broker.bridge_pool_count.{pool}"), count as f64)?;
            }

            let (daily_logins,): (i64,) = sqlx::query_as(
                "select count(id) from last_login where login_time > NOW() - INTERVAL '24 hours'",
            )
            .fetch_one(&*POSTGRES)
            .await?;
            client.gauge("broker.daily_logins", daily_logins as f64)?;
            let (weekly_logins,): (i64,) = sqlx::query_as(
                "select count(id) from last_login where login_time > NOW() - INTERVAL '7 days'",
            )
            .fetch_one(&*POSTGRES)
            .await?;
            client.gauge("broker.weekly_logins", weekly_logins as f64)?;
        }
        async_io::Timer::after(Duration::from_secs(5)).await;
    }
}
