use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use async_trait::async_trait;
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
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
        tracing::trace!(
            method = req.method,
            url = self.url,
            host = debug(&self.host),
            "calling broker through http"
        );
        let start = Instant::now();
        let mut request_builder = if let Some(dns) = &self.dns {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .dns_resolver(Arc::new(OverrideDnsResolve(dns.clone())))
                .build()
        } else {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
        }
        .unwrap()
        .post(&self.url)
        .header("content-type", "application/json");

        if let Some(host) = &self.host {
            request_builder = request_builder.header("Host", host);
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
