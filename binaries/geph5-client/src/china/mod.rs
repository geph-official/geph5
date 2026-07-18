use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
    sync::LazyLock,
    time::Duration,
};

use geph5_rt::TimeoutExt;
use moka::future::Cache;
use once_cell::sync::Lazy;
use prefix_trie::PrefixSet;
use simple_dns::{CLASS, Name, Packet, PacketFlag, QCLASS, QTYPE, Question, TYPE, rdata::RData};

/// List of all Chinese domains.
static DOMAINS: Lazy<HashSet<String>> = Lazy::new(|| {
    let ss = include_str!("china-domains.txt");
    ss.split_ascii_whitespace()
        .filter(|v| v.len() > 1)
        .map(|v| v.to_string())
        .collect()
});

/// List of all Chinese domains.
static IP_ADDRS: Lazy<PrefixSet<ipnet::Ipv4Net>> = Lazy::new(|| {
    let mut set = PrefixSet::new();
    let ss = include_str!("china-ips.txt");
    for line in ss.lines() {
        set.insert(line.parse().unwrap());
    }
    set
});

/// Returns true if the given host is Chinese
pub fn is_chinese_host(host: &str) -> bool {
    if let Ok(ipv4) = Ipv4Addr::from_str(host) {
        let net = ipnet::Ipv4Net::new(ipv4, 24).unwrap();
        return IP_ADDRS.get_lpm(&net).is_some();
    }
    if let Some(host) = psl::domain_str(host) {
        // explode by dots
        let exploded: Vec<_> = host.split('.').collect();
        // join & lookup in loop
        for i in 0..exploded.len() {
            let candidate = (exploded[i..]).join(".");
            if DOMAINS.contains(&candidate) {
                return true;
            }
        }
    }
    false
}

/// AliDNS public resolver (anycast).
const ALIDNS: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(223, 5, 5, 5), 53));

/// Resolve a Chinese domain by querying AliDNS directly over a UDP socket pinned
/// to the physical interface, bypassing the system resolver entirely.
///
/// Exists because in full-tunnel VPN mode with `spoof_dns` on, the system
/// resolver's only reachable upstream is our own fake-DNS responder (the kill
/// switch's DNS-leak guard blocks every other resolver path), so
/// `tokio::net::lookup_host` on the china-passthrough path would hand back a
/// fake-pool address and the "direct" dial would blackhole. Querying a Chinese
/// public resolver also gives the China-side geo-DNS answer the passthrough
/// wants, rather than the exit-side view.
///
/// A records only: the passthrough dials whatever this returns, every Chinese
/// site is v4-reachable, and a v4-pinned socket is the one interface binding we
/// know we have.
pub async fn resolve_via_alidns(host: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    static CACHE: LazyLock<Cache<String, Vec<Ipv4Addr>>> = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(240))
            .max_capacity(10_000)
            .build()
    });
    let ips = CACHE
        .try_get_with(host.to_string(), query_alidns_a(host))
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(ips
        .into_iter()
        .map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
        .collect())
}

async fn query_alidns_a(host: &str) -> anyhow::Result<Vec<Ipv4Addr>> {
    let socket = crate::bound_dialer::udp_socket_v4().await?;
    // Connecting the socket makes the OS discard responses from anyone else.
    socket.connect(ALIDNS).await?;
    let query_id: u16 = rand::random();
    let mut query = Packet::new_query(query_id);
    query.set_flags(PacketFlag::RECURSION_DESIRED);
    query.questions.push(Question::new(
        Name::new(host)?,
        QTYPE::TYPE(TYPE::A),
        QCLASS::CLASS(CLASS::IN),
        false,
    ));
    socket.send(&query.build_bytes_vec()?).await?;

    // One shot: on timeout the whole open_conn fails and the app's own retry
    // (or the next connection) simply asks again.
    let recv_valid = async {
        let mut buf = [0u8; 4096];
        loop {
            let n = match socket.recv(&mut buf).await {
                Ok(n) => n,
                Err(_) => {
                    // e.g. Windows surfaces ICMP unreachable as an error on the
                    // *next* recv; don't hot-spin until the timeout.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let Ok(resp) = Packet::parse(&buf[..n]) else {
                continue;
            };
            if resp.id() != query_id {
                continue;
            }
            let ips: Vec<Ipv4Addr> = resp
                .answers
                .iter()
                .filter_map(|ans| match &ans.rdata {
                    RData::A(a) => Some(Ipv4Addr::from(a.address)),
                    _ => None,
                })
                .collect();
            if !ips.is_empty() {
                return ips;
            }
        }
    };
    let ips = recv_valid
        .timeout(Duration::from_secs(3))
        .await
        .ok_or_else(|| anyhow::anyhow!("no answer from alidns for {host}"))?;
    tracing::debug!(host, ips = debug(&ips), "resolved directly via alidns");
    Ok(ips)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baidu_is_chinese() {
        assert!(is_chinese_host("baidu.com"));
        assert!(is_chinese_host("www.baidu.com"));
        assert!(!is_chinese_host("google.com"));
    }

    /// Network test, mirroring the exit's own `resolve_google` style: resolve a
    /// Chinese domain against real AliDNS and check the answers are plausible.
    #[test]
    fn resolve_baidu_via_alidns() {
        geph5_rt::block_on(async {
            let addrs = resolve_via_alidns("baidu.com", 443).await.unwrap();
            assert!(!addrs.is_empty());
            for addr in addrs {
                assert_eq!(addr.port(), 443);
                let IpAddr::V4(ip) = addr.ip() else {
                    panic!("expected v4")
                };
                // Real public addresses: not in the fake-DNS pool, not
                // private/reserved.
                assert!(!ip.is_private() && !ip.is_loopback() && !ip.is_unspecified());
                assert_ne!(u32::from(ip) & 0xFFFE_0000, u32::from_be_bytes([198, 18, 0, 0]));
            }
        });
    }
}
