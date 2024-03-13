use std::net::SocketAddr;

use async_trait::async_trait;
use nanorpc::{DynRpcTransport, JrpcRequest, JrpcResponse, RpcTransport};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use sillad::tcp::TcpDialer;

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum BrokerSource {
    Direct(String),
    DirectTcp(SocketAddr),
}

impl BrokerSource {
    /// Converts to a RpcTransport.
    pub fn rpc_transport(&self) -> DynRpcTransport {
        match self {
            BrokerSource::Direct(s) => DynRpcTransport::new(HttpRpcTransport {
                url: s.clone(),
                client: reqwest::Client::new(),
            }),
            BrokerSource::DirectTcp(dest_addr) => {
                DynRpcTransport::new(nanorpc_sillad::DialerTransport(TcpDialer {
                    dest_addr: *dest_addr,
                }))
            }
        }
    }
}

struct HttpRpcTransport {
    url: String,
    client: reqwest::Client,
}

#[async_trait]
impl RpcTransport for HttpRpcTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        tracing::trace!(req = serde_json::to_string(&req).unwrap(), "calling broker");
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
