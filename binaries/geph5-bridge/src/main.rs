mod asn_count;
mod influxdb;
mod listen_forward;

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    thread::available_parallelism,
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use futures_util::future::join_all;
use geph5_broker_protocol::{BridgeDescriptor, Mac};
use listen_forward::listen_forward_loop;
use rand::Rng;
use sillad::{dialer::DialerExt, tcp::TcpDialer};
use sillad_sosistab3::{Cookie, listener::SosistabListener};
use smol::future::FutureExt as _;
use smol::process::Command;

use smol_timeout2::TimeoutExt;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Clone, Debug)]
struct BridgeInstance {
    advertised_ip: IpAddr,
    bind_addr: SocketAddr,
    control_listen: SocketAddr,
    control_cookie: String,
    pool: String,
}

fn main() {
    if std::env::var("GEPH5_BRIDGE_POOL")
        .unwrap()
        .contains("yaofan")
    {
        smolscale::permanently_single_threaded();
        if std::env::var("GEPH5_BRIDGE_CHILD").is_err() {
            for _ in 0..available_parallelism().unwrap().get() {
                std::thread::spawn(|| {
                    let current_exe = std::env::current_exe().unwrap();

                    // Collect the current command-line arguments
                    let args: Vec<String> = std::env::args().collect();

                    // Trim the first argument which is the current binary's path
                    let args_to_pass = &args[1..];

                    // Spawn a new process with the same command
                    let mut child = std::process::Command::new(current_exe)
                        .args(args_to_pass)
                        .env("GEPH5_BRIDGE_CHILD", "1")
                        .spawn()
                        .unwrap();

                    // Wait for the spawned process to finish
                    child.wait().unwrap();
                });
            }
            loop {
                std::thread::park();
            }
        }
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5_bridge".parse().unwrap())
                .from_env_lossy(),
        )
        .init();
    smolscale::block_on(async {
        let auth_token: Arc<str> = std::env::var("GEPH5_BRIDGE_TOKEN").unwrap().into();
        let broker_addr: SocketAddr = std::env::var("GEPH5_BROKER_ADDR").unwrap().parse().unwrap();
        let base_pool = std::env::var("GEPH5_BRIDGE_POOL").unwrap();

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

        join_all(
            instances
                .into_iter()
                .map(|instance| run_bridge_instance(instance, auth_token.clone(), broker_addr)),
        )
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
) {
    tracing::info!(
        pool = %instance.pool,
        advertised_ip = display(instance.advertised_ip),
        control_listen = display(instance.control_listen),
        "starting bridge instance"
    );
    broker_loop(
        instance.control_listen,
        instance.control_cookie.clone(),
        instance.pool.clone(),
        auth_token,
        broker_addr,
    )
    .race(async move {
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
            smol::Timer::after(Duration::from_secs(1)).await;
        }
    })
    .await
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
        smol::Timer::after(Duration::from_secs(10)).await;
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
