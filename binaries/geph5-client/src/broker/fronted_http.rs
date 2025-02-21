use std::time::Instant;

use anyhow::Context;
use async_trait::async_trait;
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use reqwest::Client;

pub struct FrontedHttpTransport {
    pub url: String,
    pub host: Option<String>,
}

#[async_trait]
impl RpcTransport for FrontedHttpTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        tracing::debug!(method = req.method, "calling broker through http");
        let start = Instant::now();
        let mut request_builder = reqwest::Client::new()
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
        tracing::debug!(
            method = req.method,
            resp_len = resp_bytes.len(),
            elapsed = debug(start.elapsed()),
            "response received through http: {:?}",
            String::from_utf8_lossy(&resp_bytes)
        );
        Ok(serde_json::from_slice(&resp_bytes)?)
    }
}
