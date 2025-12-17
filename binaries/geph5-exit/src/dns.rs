use std::{
    fmt::Debug,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::Context;
use bytes::Bytes;

use globset::{Glob, GlobSet};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use simple_dns::{rdata::RData, Name, Packet, PacketFlag, Question, CLASS, QCLASS, QTYPE, TYPE};

#[derive(Serialize, Deserialize, Clone, Debug, Copy, Default)]
pub struct FilterOptions {
    #[serde(default)]
    pub nsfw: bool,
    #[serde(default)]
    pub ads: bool,
}

impl FilterOptions {
    pub async fn check_host(&self, name: &str) -> anyhow::Result<()> {
        tracing::trace!(filter = debug(self), name, "checking against filter");
        static NSFW_LIST: LazyLock<Cache<(), Arc<GlobSet>>> = LazyLock::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(86400))
                .build()
        });
        static ADS_LIST: LazyLock<Cache<(), Arc<GlobSet>>> = LazyLock::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(86400))
                .build()
        });

        if self.nsfw
            && NSFW_LIST
                .try_get_with((), async move {
                    anyhow::Ok(Arc::new(
                        parse_oisd("https://nsfw.oisd.nl/domainswild").await?,
                    ))
                })
                .await
                .map_err(|e| anyhow::anyhow!(e))?
                .is_match(name)
        {
            anyhow::bail!("blocking NSFW domain")
        }
        if self.ads
            && ADS_LIST
                .try_get_with((), async move {
                    anyhow::Ok(Arc::new(
                        parse_oisd("https://small.oisd.nl/domainswild").await?,
                    ))
                })
                .await
                .map_err(|e| anyhow::anyhow!(e))?
                .is_match(name)
        {
            anyhow::bail!("blocking ads domain")
        }
        Ok(())
    }
}

async fn parse_oisd(url: &str) -> anyhow::Result<GlobSet> {
    let raw = reqwest::get(url).await?.bytes().await?;
    tracing::info!(url, "STARTING TO BUILD an oisd blocklist");

    let mut builder = GlobSet::builder();
    let mut count = 0;
    for line in String::from_utf8_lossy(&raw)
        .lines()
        .filter(|s| !s.contains("#"))
    {
        builder.add(Glob::from_str(line)?);
        builder.add(Glob::from_str(&line.replace("*.", ""))?);
        count += 1;
        if fastrand::f32() < 0.01 {
            tracing::info!(url, count, "LOADING an oisd blocklist");
            smol::future::yield_now().await;
        }
    }
    tracing::info!(url, count, "LOADED an oisd blocklist");
    Ok(builder.build()?)
}

pub async fn raw_dns_respond(req: Bytes, filter: FilterOptions) -> anyhow::Result<Bytes> {
    if let Ok(packet) = Packet::parse(&req) {
        for q in packet.questions.iter() {
            use std::fmt::Write;
            let mut qname_str = String::new();
            write!(&mut qname_str, "{}", q.qname)?;
            filter.check_host(&qname_str).await?;
        }
    }

    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .resolve("dns.mullvad.net", "194.242.2.2:0".parse().unwrap())
            .build()
            .unwrap()
    });

    let start = Instant::now();
    let resp = CLIENT
        .post("https://dns.mullvad.net/dns-query")
        .body(req)
        .header("content-type", "application/dns-message")
        .send()
        .await?;
    tracing::trace!(elapsed = debug(start.elapsed()), "dns-over-https completed");

    Ok(resp.bytes().await?)
}

pub async fn dns_resolve(name: &str, filter: FilterOptions) -> anyhow::Result<Vec<SocketAddr>> {
    static CACHE: LazyLock<Cache<String, Vec<SocketAddr>>> = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(240))
            .build()
    });

    // If 'name' is already an IP:port, just parse and return it
    if let Ok(addr) = SocketAddr::from_str(name) {
        return Ok(vec![addr]);
    }

    // Split "host:port"
    let (host, port_str) = name
        .rsplit_once(':')
        .context("could not split into host:port")?;
    let port: u16 = port_str.parse()?;

    // Check filters
    filter.check_host(host).await?;

    // Use the cache to avoid repetitive lookups
    let addrs = CACHE
        .try_get_with(name.to_string(), async move {
            // Kick off both DNS queries in parallel (A and AAAA)
            let (res_a, res_aaaa) = futures_util::future::join(
                async {
                    let packet = build_dns_query(host, false)?;
                    let resp = raw_dns_respond(packet, filter).await?;
                    parse_dns_response(&resp, port)
                },
                async {
                    let packet = build_dns_query(host, true)?;
                    let resp = raw_dns_respond(packet, filter).await?;
                    parse_dns_response(&resp, port)
                },
            )
            .await;

            // Merge the results
            let mut ips = vec![];
            ips.extend_from_slice(&res_aaaa?);
            ips.extend_from_slice(&res_a?);

            tracing::debug!(name, ?ips, "ips received!");
            anyhow::Ok(ips)
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    if addrs.is_empty() {
        anyhow::bail!("no addrs")
    }

    Ok(addrs)
}

/// Build a simple DNS query packet for both A and AAAA:
fn build_dns_query(host: &str, aaaa: bool) -> anyhow::Result<Bytes> {
    let mut packet = Packet::new_query(rand::random());
    packet.set_flags(PacketFlag::RECURSION_DESIRED);

    // Ask for AAAA
    if aaaa {
        packet.questions.push(Question::new(
            Name::new_unchecked(host),
            QTYPE::TYPE(TYPE::AAAA),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
    } else {
        packet.questions.push(Question::new(
            Name::new_unchecked(host),
            QTYPE::TYPE(TYPE::A),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
    }

    let bytes = packet.build_bytes_vec()?;
    Ok(bytes.into())
}

/// Parse a raw DNS response to gather all A/AAAA records.
fn parse_dns_response(packet_data: &[u8], port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    let packet = Packet::parse(packet_data)?;

    let mut addrs = Vec::new();

    for answer in packet.answers.iter() {
        match &answer.rdata {
            RData::A(ipv4) => addrs.push(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from_bits(ipv4.address)),
                port,
            )),
            RData::AAAA(ipv6) => addrs.push(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from_bits(ipv6.address)),
                port,
            )),
            _ => {}
        }
    }

    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::dns_resolve;

    #[test]
    fn resolve_google() {
        smolscale::block_on(async move {
            let res = dns_resolve(
                "google.com:443",
                super::FilterOptions {
                    nsfw: false,
                    ads: false,
                },
            )
            .await
            .unwrap();
            eprintln!("{:?}", res);
        });
    }
}
