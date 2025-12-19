use std::{
    io::ErrorKind,
    net::{Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::Context;
use blake3::Hasher;
use futures_concurrency::future::RaceOk;
use ipnet::Ipv6Net;
use once_cell::sync::OnceCell;
use rand::Rng;
use smol::{net::TcpStream, process::Command, Async};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::{session::SessionKey, CONFIG_FILE};

static IPV6_POOL: OnceCell<Vec<Ipv6Addr>> = OnceCell::new();

/// Something that can be used for happy-eyeballs dialing, with its own IPv6 address.
#[derive(Clone, Debug)]
pub struct EyeballDialer {
    inner: Option<Ipv6Addr>,
}

impl EyeballDialer {
    /// Create a new eyeball dialer with a specific IPv6 address (or none).
    pub fn new(ipv6_addr: Option<Ipv6Addr>) -> Self {
        Self { inner: ipv6_addr }
    }

    /// Connect to a given remote.
    pub async fn connect(&self, addrs: Vec<SocketAddr>) -> anyhow::Result<TcpStream> {
        let my_addr = self.inner;
        if my_addr.is_none() {
            Ok(TcpStream::connect(&addrs[..]).await?)
        } else {
            let streams: Vec<_> = addrs
                .into_iter()
                .enumerate()
                .map(|(idx, addr)| async move {
                    if idx > 0 {
                        smol::Timer::after(Duration::from_millis(500 * idx as u64)).await;
                        tracing::debug!(idx, addr = display(addr), "eyeballed to non-ideal");
                    }
                    if addr.is_ipv6()
                        && let Some(my_addr) = my_addr {
                            return connect_from(my_addr, addr).await;
                        }
                    Ok(TcpStream::connect(addr).await?)
                })
                .collect();
            streams.race_ok().await.map_err(|mut e| e.remove(0))
        }
    }
}

/// Pick a deterministic IPv6 from the pool for a session using rendezvous hashing.
pub fn eyeball_addr_for_session(session_key: SessionKey) -> Option<Ipv6Addr> {
    let pool = IPV6_POOL.get()?;
    if pool.is_empty() {
        return None;
    }

    let key_bytes = session_key.as_u128().to_le_bytes();
    let mut best: Option<(u64, Ipv6Addr)> = None;
    for addr in pool {
        let mut hasher = Hasher::new();
        hasher.update(&key_bytes);
        hasher.update(&addr.octets());
        let hash = hasher.finalize();
        let weight = u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap());
        match best {
            None => best = Some((weight, *addr)),
            Some((best_weight, _)) if weight > best_weight => best = Some((weight, *addr)),
            _ => {}
        }
    }
    best.map(|(_, addr)| addr)
}

/// Given an `Ipv6Net`, generate a random IPv6 address within that subnet.
fn random_ipv6_in_net(net: Ipv6Net) -> Ipv6Addr {
    let prefix_len = net.prefix_len();

    // The number of bits we can randomize:
    let host_bits = 128 - prefix_len;

    // Convert the network address to a u128 (big-endian).
    let network_u128 = u128::from_be_bytes(net.network().octets());

    // Maximum number of addresses in this subnet = 2^(host_bits).
    // We'll generate a random offset in [0, 2^host_bits).
    let max_offset = if host_bits == 0 {
        // If the prefix is /128, there's only one address in the subnet (no offset).
        0
    } else {
        (1u128 << host_bits) - 1
    };
    let random_offset = if max_offset == 0 {
        0
    } else {
        rand::thread_rng().gen_range(0..=max_offset)
    };

    // Our randomly chosen address is (network_address + random_offset).
    let addr_u128 = network_u128 + random_offset;
    let addr_octets = addr_u128.to_be_bytes();
    Ipv6Addr::from(addr_octets)
}

/// Connect to a remote IPv6 address using the given IPv6 address.
async fn connect_from(from: Ipv6Addr, remote: SocketAddr) -> anyhow::Result<TcpStream> {
    tracing::debug!(
        from = display(from),
        remote = display(remote),
        "connecting from an ephemeral IPv6"
    );
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    // socket.set_reuse_port(true)?;
    let local_addr = SocketAddr::new(std::net::IpAddr::V6(from), 0);
    socket
        .bind(&SockAddr::from(local_addr))
        .context("cannot bind")?;

    let async_socket = Async::new(socket).context("cannot build Async")?;
    let _ = async_socket.get_ref().connect(&SockAddr::from(remote));
    async_socket
        .writable()
        .await
        .context("cannot wait until socket is writable")?;
    match async_socket.get_ref().connect(&SockAddr::from(remote)) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => anyhow::bail!("cannot finish connect: {:?}", e),
    }
    let socket: std::net::TcpStream = async_socket.into_inner()?.into();
    let socket: TcpStream = socket.try_into()?;
    Ok(socket)
}

pub async fn configure_ipv6_routing() -> anyhow::Result<()> {
    let cfg = CONFIG_FILE.wait();
    let range = cfg.ipv6_subnet;
    if range == Ipv6Net::default() {
        IPV6_POOL.set(Vec::new()).ok();
        return Ok(());
    }
    let iface = detect_ipv6_interface().await?;
    let pool = ensure_ipv6_pool(range, cfg.ipv6_pool_size, &iface).await?;
    IPV6_POOL.set(pool).ok();
    Ok(())
}

async fn ensure_ipv6_pool(
    range: Ipv6Net,
    desired: usize,
    iface: &str,
) -> anyhow::Result<Vec<Ipv6Addr>> {
    let mut pool = existing_ipv6_addresses(range, iface).await?;
    while pool.len() < desired {
        let candidate = random_ipv6_in_net(range);
        if pool.contains(&candidate) {
            continue;
        }
        Command::new("ip")
            .arg("-6")
            .arg("addr")
            .arg("add")
            .arg(format!("{}/128", candidate))
            .arg("dev")
            .arg(iface)
            .spawn()?
            .output()
            .await?;
        pool.push(candidate);
    }
    Ok(pool)
}

async fn existing_ipv6_addresses(range: Ipv6Net, iface: &str) -> anyhow::Result<Vec<Ipv6Addr>> {
    let output = Command::new("ip")
        .args(["-6", "addr", "show", "dev", iface])
        .output()
        .await?;
    let stdout = String::from_utf8(output.stdout)?;
    let mut addrs = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with("inet6 ") {
            continue;
        }
        let mut parts = line.split_whitespace();
        parts.next();
        if let Some(addr_part) = parts.next()
            && let Some(addr_str) = addr_part.split('/').next()
                && let Ok(addr) = addr_str.parse::<Ipv6Addr>()
                    && range.contains(&addr) && !addrs.contains(&addr) {
                        addrs.push(addr);
                    }
    }
    Ok(addrs)
}

async fn detect_ipv6_interface() -> anyhow::Result<String> {
    let output = Command::new("ip").args(["-6", "route"]).output().await?;

    let stdout = String::from_utf8(output.stdout)?;

    // Find the default route
    let default_route = stdout
        .lines()
        .find(|line| line.starts_with("default"))
        .context("No default IPv6 route found")?;

    // Extract the interface name
    let iface = default_route
        .split_whitespace()
        .nth(4)
        .context("Unable to parse interface name from default route")?;

    Ok(iface.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipnet::Ipv6Net;

    #[test]
    fn test_random_ipv6_in_net_basic() {
        // Given a /64
        let cidr_str = "2001:db8::/64";
        let net: Ipv6Net = cidr_str.parse().expect("Failed to parse IPv6 CIDR");

        // When we generate a random address
        let addr = random_ipv6_in_net(net);

        // Then the address should be contained within that net
        // ipnet’s `contains()` checks if the address is in the subnet range
        assert!(net.contains(&addr), "Generated address not in the subnet");
    }

    #[test]
    fn test_random_ipv6_in_net_small_prefix() {
        // Given a /120 (just 8 host bits)
        let cidr_str = "2001:db8::/120";
        let net: Ipv6Net = cidr_str.parse().expect("Failed to parse IPv6 CIDR");

        let addr = random_ipv6_in_net(net);
        assert!(net.contains(&addr), "Generated address not in the subnet");
    }

    #[test]
    fn test_random_ipv6_in_net_full_address() {
        // Given /128 => only one valid host in the subnet
        let cidr_str = "2001:db8::/128";
        let net: Ipv6Net = cidr_str.parse().expect("Failed to parse IPv6 CIDR");

        let addr = random_ipv6_in_net(net);

        // There's only one possible address: 2001:db8::
        assert_eq!(
            addr,
            net.network(),
            "Should be exactly the single /128 address"
        );
    }
}
