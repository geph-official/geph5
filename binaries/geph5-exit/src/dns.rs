use std::{
    collections::HashMap,
    net::SocketAddr,
    str::FromStr,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
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
    pub nsfw: bool,
    pub ads: bool,
}

impl FilterOptions {
    pub async fn check_host(&self, name: &str) -> anyhow::Result<()> {
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
        let nsfw_list = NSFW_LIST
            .try_get_with((), async move {
                anyhow::Ok(Arc::new(
                    parse_oisd("https://nsfw.oisd.nl/domainswild").await?,
                ))
            })
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let ads_list = ADS_LIST
            .try_get_with((), async move {
                anyhow::Ok(Arc::new(
                    parse_oisd("https://big.oisd.nl/domainswild").await?,
                ))
            })
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        if self.nsfw && nsfw_list.is_match(name) {
            anyhow::bail!("blocking NSFW domain")
        }
        if self.ads && ads_list.is_match(name) {
            anyhow::bail!("blocking ads domain")
        }
        Ok(())
    }
}

async fn parse_oisd(url: &str) -> anyhow::Result<GlobSet> {
    let raw = reqwest::get(url).await?.bytes().await?;
    let mut builder = GlobSet::builder();
    let mut count = 0;
    for line in String::from_utf8_lossy(&raw)
        .lines()
        .filter(|s| !s.contains("#"))
    {
        builder.add(Glob::from_str(line)?);
        count += 1;
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
    filter.check_host(name.split(':').next().unwrap()).await?;
    let addr = CACHE
        .try_get_with(name.to_string(), async {
            let choices = smol::net::resolve(name).await?;
            anyhow::Ok(choices)
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(addr)
}

/// A udp-socket-efficient DNS responder.
pub async fn raw_dns_respond(req: Bytes) -> anyhow::Result<Bytes> {
    let (send_resp, recv_resp) = oneshot::channel();
    DNS_RESPONDER
        .send((req, send_resp))
        .await
        .ok()
        .context("could not send")?;
    Ok(recv_resp.await?)
}

static DNS_RESPONDER: LazyLock<Sender<(Bytes, oneshot::Sender<Bytes>)>> = LazyLock::new(|| {
    let (send_req, recv_req) = smol::channel::bounded(1);
    for _ in 0..500 {
        smolscale::spawn(dns_respond_loop(recv_req.clone())).detach();
    }
    send_req
});

async fn dns_respond_loop(recv_req: Receiver<(Bytes, oneshot::Sender<Bytes>)>) {
    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    socket.connect("8.8.8.8:53").await.unwrap();
    let outstanding_reqs = Mutex::new(HashMap::new());
    let upload = async {
        loop {
            let (req, send_resp) = recv_req.recv().await.unwrap();
            if let Ok(packet) = Packet::parse(&req) {
                outstanding_reqs
                    .lock()
                    .unwrap()
                    .insert(packet.id(), send_resp);
                let _ = socket.send(&req).await;
            }
        }
    };
    let download = async {
        let mut buf = [0u8; 65536];
        loop {
            let n = socket.recv(&mut buf).await.unwrap();
            if let Ok(packet) = Packet::parse(&buf[..n]) {
                if let Some(resp) = outstanding_reqs.lock().unwrap().remove(&packet.id()) {
                    // tracing::debug!(
                    //     id = packet.id(),
                    //     bytes = hex::encode(&buf[..n]),
                    //     packet = debug(packet),
                    //     "DNS response with CORRECT ID"
                    // );
                    let _ = resp.send(Bytes::copy_from_slice(&buf[..n]));
                } else {
                    tracing::warn!(id = packet.id(), "DNS response with mismatching ID")
                }
            }
        }
    };
    upload.race(download).await
}
