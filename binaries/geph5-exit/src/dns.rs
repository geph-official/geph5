use std::{
    collections::HashMap,
    net::SocketAddr,
    str::FromStr,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use bytes::Bytes;

use globset::{Glob, GlobSet};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use simple_dns::Packet;
use smol::{
    channel::{Receiver, Sender},
    future::FutureExt as _,
    net::UdpSocket,
};

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

/// DNS resolve a name
pub async fn dns_resolve(name: &str, filter: FilterOptions) -> anyhow::Result<Vec<SocketAddr>> {
    static CACHE: LazyLock<Cache<String, Vec<SocketAddr>>> = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(240))
            .build()
    });
    let (host, port) = name
        .rsplit_once(":")
        .context("could not split into host and port")?;
    let port: u16 = port.parse()?;
    filter.check_host(host).await?;
    let addr = CACHE
        .try_get_with(name.to_string(), async {
            let choices = smol::net::resolve(name).await?;
            anyhow::Ok(choices)
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(addr)
}

pub async fn raw_dns_respond(req: Bytes, filter: FilterOptions) -> anyhow::Result<Bytes> {
    if let Ok(packet) = Packet::parse(&req) {
        for q in packet.questions.iter() {
            let qname = q.qname.to_string();
            filter.check_host(&qname).await?;
        }
    }

    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .pool_max_idle_per_host(1024)
            .build()
            .unwrap()
    });

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
