use std::time::Duration;

use anyhow::Context;
use cadence::Gauged;

use crate::rpc_impl::STATSD_CLIENT;

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
            client.gauge(&format!("broker.{ip_addr}.nmlz_load_factor"), load_avg)?;
        }
        async_io::Timer::after(Duration::from_secs(1)).await;
    }
}
