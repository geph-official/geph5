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
        let load_avg: f64 = std::fs::read_to_string("/proc/loadavg")?
            .split_ascii_whitespace()
            .next()
            .context("no first")?
            .parse()?;
        if let Some(client) = STATSD_CLIENT.as_ref() {
            client.gauge(
                &format!("broker.{ip_addr}.nmlz_load_factor"),
                load_avg / available_parallelism().unwrap().get() as f64,
            )?;
        }
        let pool_counts: Vec<(String, i32)> =
            sqlx::query_as("select pool,count(listen) from bridges_new group by pool")
                .fetch_all(&*POSTGRES)
                .await?;
        tracing::debug!("pool_counts: {:?}", pool_counts);
        for (pool, count) in pool_counts {
            if let Some(client) = STATSD_CLIENT.as_ref() {
                client.gauge(&format!("broker.bridge_pool_count.{pool}"), count as f64)?;
            }
        }
        async_io::Timer::after(Duration::from_secs(5)).await;
    }
}
