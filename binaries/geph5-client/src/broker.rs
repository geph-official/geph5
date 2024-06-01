use anyctx::AnyCtx;
use anyhow::Context;
use async_trait::async_trait;
use geph5_broker_protocol::BrokerClient;
use nanorpc::{DynRpcTransport, JrpcRequest, JrpcResponse, RpcTransport};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sillad::tcp::TcpDialer;
use std::{net::SocketAddr, time::Instant};

use crate::client::{Config, CtxField};

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum BrokerSource {
    Direct(String),
    Fronted { front: String, host: String },
    DirectTcp(SocketAddr),
}

impl BrokerSource {
    /// Converts to a RpcTransport.
    pub fn rpc_transport(&self) -> DynRpcTransport {
        let client = Client::builder().no_proxy().build().unwrap();
        match self {
            BrokerSource::Direct(s) => DynRpcTransport::new(HttpRpcTransport {
                url: s.clone(),
                host: None,
                client,
            }),
            BrokerSource::DirectTcp(dest_addr) => {
                DynRpcTransport::new(nanorpc_sillad::DialerTransport(TcpDialer {
                    dest_addr: *dest_addr,
                }))
            }
            BrokerSource::Fronted { front, host } => DynRpcTransport::new(HttpRpcTransport {
                url: front.clone(),
                host: Some(host.clone()),
                client,
            }),
        }
    }
}

pub fn broker_client(ctx: &AnyCtx<Config>) -> anyhow::Result<&BrokerClient> {
    ctx.get(BROKER_CLIENT).as_ref().context(
        "broker information not provided, so cannot use any broker-dependent functionality",
    )
}

static BROKER_CLIENT: CtxField<Option<BrokerClient>> = |ctx| {
    ctx.init()
        .broker
        .as_ref()
        .map(|src| BrokerClient::from(src.rpc_transport()))
};

struct HttpRpcTransport {
    url: String,
    host: Option<String>,
    client: Client,
}

#[async_trait]
impl RpcTransport for HttpRpcTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        tracing::trace!(method = req.method, "calling broker");
        let start = Instant::now();
        let mut request_builder = self
            .client
            .post(&self.url)
            .header("content-type", "application/json");

        if let Some(host) = &self.host {
            request_builder = request_builder.header("Host", host);
        }

        let request_body = serde_json::to_vec(&req)?;
        let response = request_builder.body(request_body).send().await?;

        let resp_bytes = response.bytes().await?;
        tracing::trace!(
            method = req.method,
            resp_len = resp_bytes.len(),
            elapsed = debug(start.elapsed()),
            "response received"
        );
        Ok(serde_json::from_slice(&resp_bytes)?)
    }
}
