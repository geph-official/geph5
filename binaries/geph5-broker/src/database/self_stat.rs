use std::{thread::available_parallelism, time::Duration};

use anyhow::Context;
use geph5_stats::StatEvent;

use super::POSTGRES;
use crate::rpc_impl::STATS_SINK;

pub async fn self_stat_loop() -> anyhow::Result<()> {
    let ip_addr = String::from_utf8_lossy(
        &reqwest::get("http://checkip.amazonaws.com/")
            .await?
            .bytes()
            .await?,
    )
    .trim()
    .to_string();

    loop {
        if let Some(sink) = STATS_SINK.as_ref() {
            let load_avg: f64 = std::fs::read_to_string("/proc/loadavg")?
                .split_ascii_whitespace()
                .next()
                .context("no first")?
                .parse()?;

            let mut events = vec![StatEvent::gauge(
                "broker_sysstat",
                &[("ip_addr", &ip_addr)],
                load_avg / available_parallelism()?.get() as f64,
            )];

            let pool_counts: Vec<(String, i64)> =
                sqlx::query_as("select pool,count(listen) from bridges_new group by pool")
                    .fetch_all(&*POSTGRES)
                    .await?;
            tracing::debug!("pool_counts: {:?}", pool_counts);
            for (pool, count) in pool_counts {
                events.push(StatEvent::gauge(
                    "bridge_pools",
                    &[("pool", &pool)],
                    count as f64,
                ));
            }

            let (daily_logins,): (i64,) = sqlx::query_as(
                "select count(id) from last_login where login_time > NOW() - INTERVAL '24 hours'",
            )
            .fetch_one(&*POSTGRES)
            .await?;

            let (daily_logins_new,): (i64,) = sqlx::query_as(
                "select count(id) from last_login natural join auth_secret where login_time > NOW() - INTERVAL '24 hours'",
            )
            .fetch_one(&*POSTGRES)
            .await?;

            let (weekly_logins,): (i64,) = sqlx::query_as(
                "select count(id) from last_login where login_time > NOW() - INTERVAL '7 days'",
            )
            .fetch_one(&*POSTGRES)
            .await?;

            events.push(StatEvent::gauge(
                "broker_logins",
                &[("kind", "daily")],
                daily_logins as f64,
            ));
            events.push(StatEvent::gauge(
                "broker_logins",
                &[("kind", "daily_new")],
                daily_logins_new as f64,
            ));
            events.push(StatEvent::gauge(
                "broker_logins",
                &[("kind", "weekly")],
                weekly_logins as f64,
            ));

            let (plus_count,): (i64,) = sqlx::query_as("select count(*) from subscriptions")
                .fetch_one(&*POSTGRES)
                .await?;
            events.push(StatEvent::gauge("plus", &[], plus_count as f64));

            sink.send_many(&events);
        }
        async_io::Timer::after(Duration::from_secs(30)).await;
    }
}
