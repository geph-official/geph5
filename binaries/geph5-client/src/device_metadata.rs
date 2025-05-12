use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use anyctx::AnyCtx;
use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use smol::lock::Semaphore;

use crate::client::Config;
use crate::database;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceMetadata {
    pub version: String,
    pub ip_addr: String,
}

/// Get device metadata information including package version and IP address.
/// The IP address is fetched from checkip.amazonaws.com and cached for 24 hours.
pub async fn get_device_metadata(ctx: &AnyCtx<Config>) -> anyhow::Result<DeviceMetadata> {
    if ctx.init().vpn {
        anyhow::bail!("cannot get device metadata if VPN is on")
    }

    // Get the version from Cargo package
    let version = env!("CARGO_PKG_VERSION").to_string();

    // Get the IP address
    let ip_addr = get_ip_address(ctx).await?;

    Ok(DeviceMetadata { version, ip_addr })
}

async fn get_ip_address(ctx: &AnyCtx<Config>) -> anyhow::Result<String> {
    let today = Utc::now().date_naive().to_string();
    let cache_key = format!("device_ip_address_redacted_{}", today);

    if let Ok(Some(cached_data)) = database::db_read(ctx, &cache_key).await {
        return Ok(String::from_utf8_lossy(&cached_data).into());
    }

    let ip = fetch_ip_from_service().await?;

    database::db_write(ctx, &cache_key, ip.as_bytes()).await?;

    Ok(ip)
}

async fn fetch_ip_from_service() -> anyhow::Result<String> {
    static SEMAPH: Semaphore = Semaphore::new(1);

    let _guard = SEMAPH.acquire().await;
    // we MUST use ipv4 here, because the server cannot handle Ipv6 addresses yet
    let client = reqwest::Client::builder()
        .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let response = client
        .get("https://checkip.amazonaws.com")
        .send()
        .await?
        .text()
        .await?;

    let ip_str = response.trim();
    if ip_str.is_empty() {
        return Err(anyhow::anyhow!("Failed to parse IP address from response"));
    }

    // Parse as IPv4, zero the last octet, and return
    let addr: Ipv4Addr = ip_str
        .parse()
        .context("invalid IPv4 address from service")?;
    let octs = addr.octets();
    let redacted = Ipv4Addr::new(octs[0], octs[1], octs[2], 0);

    Ok(redacted.to_string())
}
