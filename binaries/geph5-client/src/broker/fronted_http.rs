use std::{
    net::SocketAddr,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::Context;
use async_trait::async_trait;
use base64::{Engine as _, prelude::BASE64_STANDARD_NO_PAD};
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use rand::Rng as _;
use reqwest::dns::Resolve;

pub struct FrontedHttpTransport {
    pub url: String,
    pub host: Option<String>,
    pub dns: Option<Vec<SocketAddr>>,
}

#[async_trait]
impl RpcTransport for FrontedHttpTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        static POOL: LazyLock<reqwest::Client> = LazyLock::new(|| {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap()
        });

        tracing::debug!(
            method = req.method,
            url = self.url,
            host = debug(&self.host),
            "calling broker through http"
        );
        let start = Instant::now();
        let mut request_builder = if crate::bound_dialer::binding_active() {
            // Windows full-tunnel: route this connection through a loopback
            // forwarder whose upstream is dialed via the bound dialer
            // (physical-NIC-pinned), so only *our* socket to the shared front IP
            // bypasses the tunnel. Reached only for fronted sources with fixed
            // `override_dns` addresses (others are ignored in VPN mode).
            let dests = self
                .dns
                .clone()
                .context("fronted broker source without override_dns is unusable in VPN mode")?;
            if reqwest::Url::parse(&self.url).ok().and_then(|u| u.port()).is_some() {
                tracing::warn!(
                    url = self.url,
                    "front URL has an explicit port; the loopback egress forwarder may be bypassed"
                );
            }
            let loopback = super::bind_forward::forward_addrs(dests)
                .await
                .context("could not set up broker egress forwarder")?;
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(60))
                .dns_resolver(Arc::new(OverrideDnsResolve(vec![loopback])))
                .build()
                .context("could not build bound broker client")?
                .post(&self.url)
                .header("content-type", "application/json")
        } else if let Some(dns) = &self.dns {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(60))
                .dns_resolver(Arc::new(OverrideDnsResolve(dns.clone())))
                .build()
                .unwrap()
                .post(&self.url)
                .header("content-type", "application/json")
        } else {
            POOL.post(&self.url)
                .header("content-type", "application/json")
        };

        if let Some(host) = &self.host {
            request_builder = request_builder
                .header("Host", host)
                .header("X-Padding", random_padding_header());
        }

        let request_body = serde_json::to_vec(&req)?;
        let response = request_builder
            .body(request_body)
            .send()
            .await
            .context("cannot send request to front")?;

        let resp_bytes = response.bytes().await?;
        tracing::trace!(
            method = req.method,
            url = self.url,
            host = debug(&self.host),
            resp_len = resp_bytes.len(),
            elapsed = debug(start.elapsed()),
            "response received through http",
        );
        Ok(serde_json::from_slice(&resp_bytes)?)
    }
}

fn random_padding_header() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = vec![0u8; rng.gen_range(7..=375)];
    rng.fill(bytes.as_mut_slice());
    BASE64_STANDARD_NO_PAD.encode(bytes)
}

struct OverrideDnsResolve(Vec<SocketAddr>);

impl Resolve for OverrideDnsResolve {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let addrs = self.0.clone();
        Box::pin(async move {
            let b: Box<dyn Iterator<Item = SocketAddr> + Send + 'static> =
                Box::new(addrs.into_iter());
            Ok(b)
        })
    }
}
