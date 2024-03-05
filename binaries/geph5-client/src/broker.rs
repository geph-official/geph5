use async_trait::async_trait;
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use reqwest::Method;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub enum BrokerSource {
    Direct(String),
}

pub struct BrokerRpcTransport {
    url: String,
    client: reqwest::Client,
}

impl BrokerRpcTransport {
    pub fn new(src: BrokerSource) -> Self {
        Self {
            url: match src {
                BrokerSource::Direct(s) => s,
            },
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl RpcTransport for BrokerRpcTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
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
