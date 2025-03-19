use std::{thread::available_parallelism, time::Duration};

use anyhow::Context;
use influxdb_line_protocol::LineProtocolBuilder;

use crate::{database::POSTGRES, CONFIG_FILE};

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
        if let Some(endpoint) = &CONFIG_FILE.wait().influxdb {
            let load_avg: f64 = std::fs::read_to_string("/proc/loadavg")?
                .split_ascii_whitespace()
                .next()
                .context("no first")?
                .parse()?;

            // Send system stats to InfluxDB
            endpoint
                .send_line(
                    LineProtocolBuilder::new()
                        .measurement("broker_sysstat")
                        .tag("ip_addr", &ip_addr)
                        .field(
                            "nmlz_load_factor",
                            load_avg / available_parallelism()?.get() as f64,
                        )
                        .close_line()
                        .build(),
                )
                .await?;

            // Get bridge pool counts
            let pool_counts: Vec<(String, i64)> =
                sqlx::query_as("select pool,count(listen) from bridges_new group by pool")
                    .fetch_all(&*POSTGRES)
                    .await?;
            tracing::debug!("pool_counts: {:?}", pool_counts);

            // Send bridge pool counts to InfluxDB
            for (pool, count) in pool_counts {
                endpoint
                    .send_line(
                        LineProtocolBuilder::new()
                            .measurement("bridge_pools")
                            .tag("pool", &pool)
                            .field("count", count as f64)
                            .close_line()
                            .build(),
                    )
                    .await?;
            }

            // Get login statistics
            let (daily_logins,): (i64,) = sqlx::query_as(
                "select count(id) from last_login where login_time > NOW() - INTERVAL '24 hours'",
            )
            .fetch_one(&*POSTGRES)
            .await?;

            let (weekly_logins,): (i64,) = sqlx::query_as(
                "select count(id) from last_login where login_time > NOW() - INTERVAL '7 days'",
            )
            .fetch_one(&*POSTGRES)
            .await?;

            // Send daily logins to InfluxDB
            endpoint
                .send_line(
                    LineProtocolBuilder::new()
                        .measurement("broker_logins")
                        .field("daily_logins", daily_logins as f64)
                        .field("weekly_logins", weekly_logins as f64)
                        .close_line()
                        .build(),
                )
                .await?;
        }
        async_io::Timer::after(Duration::from_secs(5)).await;
    }
}
