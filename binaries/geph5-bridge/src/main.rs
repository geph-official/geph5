mod asn_count;
mod listen_forward;
mod ratelimit;
mod stats;

use std::{
    env::VarError,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use futures_concurrency::future::Race;
use futures_util::future::join_all;
use geph5_broker_protocol::{BridgeDescriptor, Mac};
use geph5_rt::TimeoutExt;
use listen_forward::listen_forward_loop;
use rand::Rng;
use ratelimit::BridgeRateLimiter;
use sillad::{dialer::DialerExt, tcp::TcpDialer};
use sillad_sosistab3::{Cookie, listener::SosistabListener};
use tokio::process::Command;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const BRIDGE_TOTAL_RATELIMIT_ENV: &str = "GEPH5_BRIDGE_TOTAL_RATELIMIT";

#[derive(Clone, Debug)]
struct BridgeInstance {
    advertised_ip: IpAddr,
    bind_addr: SocketAddr,
    control_listen: SocketAddr,
    control_cookie: String,
    pool: String,
}

fn main() {
    let base_pool = std::env::var("GEPH5_BRIDGE_POOL").unwrap();
    let rate_limit_kib = bridge_total_ratelimit_from_env().unwrap();

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5_bridge".parse().unwrap())
                .from_env_lossy(),
        )
        .init();
    geph5_rt::block_on(async move {
        let rate_limiter = match rate_limit_kib {
            Some(rate_limit_kib) => {
                tracing::info!(rate_limit_kib, "configured bridge rate limit");
                BridgeRateLimiter::limited(rate_limit_kib)
            }
            None => {
                tracing::info!("bridge rate limit disabled");
                BridgeRateLimiter::unlimited()
            }
        };
        let auth_token: Arc<str> = std::env::var("GEPH5_BRIDGE_TOKEN").unwrap().into();
        let broker_addr: SocketAddr = std::env::var("GEPH5_BROKER_ADDR").unwrap().parse().unwrap();

        {
            let auth_token = auth_token.clone();
            geph5_rt::spawn(
                async move { stats::stats_flush_loop(&auth_token, broker_addr).await },
            )
            .detach();
        }

        let mut instances = vec![new_bridge_instance(
            discover_public_ipv4().await?.into(),
            base_pool.clone(),
        )];
        if let Some(ipv6) = discover_primary_ipv6().await? {
            instances.push(new_bridge_instance(
                ipv6.into(),
                format!("{base_pool}_ipv6"),
            ));
        }

        join_all(instances.into_iter().map(|instance| {
            run_bridge_instance(
                instance,
                auth_token.clone(),
                broker_addr,
                rate_limiter.clone(),
            )
        }))
        .await;
        anyhow::Ok(())
    })
    .unwrap()
}

fn new_bridge_instance(advertised_ip: IpAddr, pool: String) -> BridgeInstance {
    let port = rand::thread_rng().gen_range(1024..10000);
    let bind_addr = SocketAddr::new(listen_ip_for(advertised_ip), port);
    BridgeInstance {
        advertised_ip,
        bind_addr,
        control_listen: SocketAddr::new(advertised_ip, port),
        control_cookie: format!("bridge-cookie-{}", rand::random::<u128>()),
        pool,
    }
}

fn listen_ip_for(advertised_ip: IpAddr) -> IpAddr {
    match advertised_ip {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    }
}

async fn run_bridge_instance(
    instance: BridgeInstance,
    auth_token: Arc<str>,
    broker_addr: SocketAddr,
    rate_limiter: BridgeRateLimiter,
) {
    tracing::info!(
        pool = %instance.pool,
        advertised_ip = display(instance.advertised_ip),
        control_listen = display(instance.control_listen),
        "starting bridge instance"
    );
    let upload = broker_loop(
        instance.control_listen,
        instance.control_cookie.clone(),
        instance.pool.clone(),
        auth_token,
        broker_addr,
    );
    let serve = async move {
        loop {
            let listener = sillad::tcp::TcpListener::bind_with_v6_only(
                instance.bind_addr,
                instance.bind_addr.is_ipv6(),
            )
            .await
            .unwrap();

            let control_listener =
                SosistabListener::new(listener, Cookie::new(&instance.control_cookie));
            if let Err(err) = listen_forward_loop(
                instance.advertised_ip,
                instance.pool.clone(),
                control_listener,
                rate_limiter.clone(),
            )
            .await
            {
                tracing::error!(
                    err = %err,
                    pool = %instance.pool,
                    advertised_ip = display(instance.advertised_ip),
                    "error in listen_forward_loop"
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    (upload, serve).race().await
}

fn bridge_total_ratelimit_from_env() -> anyhow::Result<Option<u32>> {
    let raw = match std::env::var(BRIDGE_TOTAL_RATELIMIT_ENV) {
        Ok(raw) => Some(raw),
        Err(VarError::NotPresent) => None,
        Err(err) => {
            return Err(err).with_context(|| format!("cannot read {BRIDGE_TOTAL_RATELIMIT_ENV}"));
        }
    };
    parse_optional_bridge_total_ratelimit(raw.as_deref())
}

fn parse_optional_bridge_total_ratelimit(raw: Option<&str>) -> anyhow::Result<Option<u32>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    parse_bridge_total_ratelimit(raw).map(Some)
}

fn parse_bridge_total_ratelimit(raw: &str) -> anyhow::Result<u32> {
    let limit = raw.trim().parse::<u32>().with_context(|| {
        format!("{BRIDGE_TOTAL_RATELIMIT_ENV} must be a positive integer KiB/s")
    })?;
    anyhow::ensure!(
        limit > 0,
        "{BRIDGE_TOTAL_RATELIMIT_ENV} must be greater than zero"
    );
    Ok(limit)
}

async fn broker_loop(
    control_listen: SocketAddr,
    control_cookie: String,
    pool: String,
    auth_token: Arc<str>,
    broker_addr: SocketAddr,
) {
    tracing::info!(
        broker_addr = display(broker_addr),
        pool,
        "starting upload loop"
    );

    let broker_rpc = Arc::new(geph5_broker_protocol::BrokerClient(
        nanorpc_sillad::DialerTransport(
            TcpDialer {
                dest_addr: broker_addr,
            }
            .timeout(Duration::from_secs(1)),
        ),
    ));

    loop {
        tracing::info!(broker_addr = display(broker_addr), pool, "uploading...");

        let res = async {
            broker_rpc
                .insert_bridge(Mac::new(
                    BridgeDescriptor {
                        control_listen,
                        control_cookie: control_cookie.clone(),
                        pool: pool.clone(),
                        expiry: SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + 120,
                    },
                    blake3::hash(auth_token.as_bytes()).as_bytes(),
                ))
                .timeout(Duration::from_secs(2))
                .await
                .context("insert bridge timed out")??
                .map_err(|e| anyhow::anyhow!(e))?;
            anyhow::Ok(())
        };
        if let Err(err) = res.await {
            tracing::error!(err = %err, "error in upload_loop");
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn discover_public_ipv4() -> anyhow::Result<Ipv4Addr> {
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
    Ok(response
        .trim()
        .parse()
        .context("invalid IPv4 from checkip")?)
}

async fn discover_primary_ipv6() -> anyhow::Result<Option<Ipv6Addr>> {
    let iface = match default_ipv6_interface().await? {
        Some(iface) => iface,
        None => return Ok(None),
    };
    let output = Command::new("ip")
        .args(["-6", "addr", "show", "dev", &iface, "scope", "global"])
        .output()
        .await?;
    if !output.status.success() {
        tracing::warn!(iface, "could not inspect IPv6 addresses");
        return Ok(None);
    }
    let stdout = String::from_utf8(output.stdout)?;
    Ok(select_primary_ipv6(&stdout))
}

async fn default_ipv6_interface() -> anyhow::Result<Option<String>> {
    let output = Command::new("ip")
        .args(["-6", "route", "show", "default"])
        .output()
        .await?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8(output.stdout)?;
    Ok(parse_default_ipv6_interface(&stdout))
}

fn parse_default_ipv6_interface(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find(|line| line.starts_with("default"))
        .and_then(|line| {
            let mut parts = line.split_whitespace();
            while let Some(part) = parts.next() {
                if part == "dev" {
                    return parts.next().map(str::to_string);
                }
            }
            None
        })
}

fn select_primary_ipv6(stdout: &str) -> Option<Ipv6Addr> {
    let mut fallback = None;

    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with("inet6 ") {
            continue;
        }
        let mut parts = line.split_whitespace();
        parts.next();
        let Some(addr_part) = parts.next() else {
            continue;
        };
        let Some(addr_str) = addr_part.split('/').next() else {
            continue;
        };
        let Ok(addr) = addr_str.parse::<Ipv6Addr>() else {
            continue;
        };
        if !is_public_ipv6(addr) {
            continue;
        }
        if line.contains("deprecated") || line.contains("tentative") || line.contains("dadfailed") {
            continue;
        }
        if line.contains("temporary") || line.contains("mngtmpaddr") {
            fallback.get_or_insert(addr);
            continue;
        }
        return Some(addr);
    }

    fallback
}

fn is_public_ipv6(addr: Ipv6Addr) -> bool {
    !addr.is_loopback()
        && !addr.is_multicast()
        && !addr.is_unspecified()
        && !addr.is_unicast_link_local()
        && (addr.segments()[0] & 0xfe00) != 0xfc00
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_ipv6_interface_finds_dev() {
        let sample = "default via fe80::1 dev eth0 proto ra metric 100\n";
        assert_eq!(
            parse_default_ipv6_interface(sample).as_deref(),
            Some("eth0")
        );
    }

    #[test]
    fn parse_bridge_total_ratelimit_accepts_positive_integer() {
        assert_eq!(parse_bridge_total_ratelimit("125000").unwrap(), 125000);
        assert_eq!(parse_bridge_total_ratelimit(" 42 ").unwrap(), 42);
    }

    #[test]
    fn parse_optional_bridge_total_ratelimit_allows_unset_and_blank() {
        assert_eq!(parse_optional_bridge_total_ratelimit(None).unwrap(), None);
        assert_eq!(
            parse_optional_bridge_total_ratelimit(Some("")).unwrap(),
            None
        );
        assert_eq!(
            parse_optional_bridge_total_ratelimit(Some("   ")).unwrap(),
            None
        );
        assert_eq!(
            parse_optional_bridge_total_ratelimit(Some("125000")).unwrap(),
            Some(125000)
        );
    }

    #[test]
    fn parse_bridge_total_ratelimit_rejects_zero_and_invalid_values() {
        assert!(parse_bridge_total_ratelimit("0").is_err());
        assert!(parse_bridge_total_ratelimit("").is_err());
        assert!(parse_bridge_total_ratelimit("12.5").is_err());
    }

    #[test]
    fn select_primary_ipv6_prefers_stable_global() {
        let sample = "\
    inet6 2001:db8::1234/64 scope global temporary dynamic\n\
    inet6 2001:db8::1/64 scope global dynamic\n";
        assert_eq!(
            select_primary_ipv6(sample),
            Some("2001:db8::1".parse().unwrap())
        );
    }

    #[test]
    fn select_primary_ipv6_skips_unique_local() {
        let sample = "    inet6 fd00::1/64 scope global dynamic\n";
        assert_eq!(select_primary_ipv6(sample), None);
    }
}
