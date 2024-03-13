use async_trait::async_trait;
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use reqwest::Method;

pub struct BrokerRpcTransport {
    url: String,
    client: reqwest::Client,
}

impl BrokerRpcTransport {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl RpcTransport for BrokerRpcTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        tracing::debug!(method = req.method, "calling binder");
        let resp = self
            .client
            .request(Method::POST, &self.url)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&req).unwrap())
            .send()
            .await?;
        Ok(serde_json::from_slice(&resp.bytes().await?)?)
    }
}
