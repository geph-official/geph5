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

    static CLIENT: LazyLock<reqwest::Client> =
        LazyLock::new(|| reqwest::Client::builder().build().unwrap());

    let start = Instant::now();
    let resp = CLIENT
        .post("https://cloudflare-dns.com/dns-query")
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

    // Split into "host:port"
    let (host, port_str) = name
        .rsplit_once(":")
        .context("could not split into host:port")?;
    let port: u16 = port_str.parse()?;

    // Check filters
    filter.check_host(host).await?;

    // Use the cache to avoid repetitive lookups
    let addrs = CACHE
        .try_get_with(name.to_string(), async move {
            // Build a DNS query for A and AAAA
            let query_data = build_dns_query(host)?;

            // Dispatch the query via DoH
            let resp_bytes = raw_dns_respond(query_data, filter).await?;

            // Parse out IP addresses
            let ips = parse_dns_response(&resp_bytes, port)?;
            anyhow::Ok(ips)
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    Ok(addrs)
}

/// Build a simple DNS query packet for both A and AAAA:
fn build_dns_query(host: &str) -> anyhow::Result<Bytes> {
    let mut packet = Packet::new_query(rand::random());
    packet.set_flags(PacketFlag::RECURSION_DESIRED);

    // Ask for A
    packet.questions.push(Question::new(
        Name::new_unchecked(host),
        QTYPE::TYPE(TYPE::A),
        QCLASS::CLASS(CLASS::IN),
        false,
    ));

    // Ask for AAAA
    packet.questions.push(Question::new(
        Name::new_unchecked(host),
        QTYPE::TYPE(TYPE::AAAA),
        QCLASS::CLASS(CLASS::IN),
        false,
    ));

    let bytes = packet.build_bytes_vec()?;
    Ok(bytes.into())
}

/// Parse a raw DNS response to gather all A/AAAA records.
fn parse_dns_response(packet_data: &[u8], port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    let packet = Packet::parse(packet_data)?;
    let mut addrs = Vec::new();

    for answer in packet.answers {
        match answer.rdata {
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

    if addrs.is_empty() {
        anyhow::bail!("No A or AAAA records found in DNS response");
    }

    Ok(addrs)
}

// /// A udp-socket-efficient DNS responder.
// pub async fn raw_dns_respond(req: Bytes, filter: FilterOptions) -> anyhow::Result<Bytes> {
//     if let Ok(packet) = Packet::parse(&req) {
//         for q in packet.questions.iter() {
//             let qname = q.qname.to_string();
//             filter.check_host(&qname).await?;
//         }
//     }
//     let (send_resp, recv_resp) = oneshot::channel();
//     DNS_RESPONDER
//         .send((req, send_resp))
//         .await
//         .ok()
//         .context("could not send")?;
//     Ok(recv_resp.await?)
// }

// static DNS_RESPONDER: LazyLock<Sender<(Bytes, oneshot::Sender<Bytes>)>> = LazyLock::new(|| {
//     let (send_req, recv_req) = smol::channel::bounded(1);
//     for _ in 0..500 {
//         smolscale::spawn(dns_respond_loop(recv_req.clone())).detach();
//     }
//     send_req
// });

// async fn dns_respond_loop(recv_req: Receiver<(Bytes, oneshot::Sender<Bytes>)>) {
//     let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
//     socket.connect("8.8.8.8:53").await.unwrap();
//     let outstanding_reqs = Mutex::new(HashMap::new());
//     let upload = async {
//         loop {
//             let (req, send_resp) = recv_req.recv().await.unwrap();
//             if let Ok(packet) = Packet::parse(&req) {
//                 outstanding_reqs
//                     .lock()
//                     .unwrap()
//                     .insert(packet.id(), send_resp);
//                 let _ = socket.send(&req).await;
//             }
//         }
//     };
//     let download = async {
//         let mut buf = [0u8; 65536];
//         loop {
//             let n = socket.recv(&mut buf).await;
//             match n {
//                 Err(err) => tracing::error!(err = debug(err), "cannot receive"),
//                 Ok(n) => {
//                     if let Ok(packet) = Packet::parse(&buf[..n]) {
//                         if let Some(resp) = outstanding_reqs.lock().unwrap().remove(&packet.id()) {
//                             let _ = resp.send(Bytes::copy_from_slice(&buf[..n]));
//                         } else {
//                             tracing::warn!(id = packet.id(), "DNS response with mismatching ID")
//                         }
//                     }
//                 }
//             }
//         }
//     };
//     upload.race(download).await
// }

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
