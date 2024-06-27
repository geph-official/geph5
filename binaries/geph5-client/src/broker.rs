mod aws_lambda;
mod fronted_http;

use anyctx::AnyCtx;
use anyhow::Context;

use fronted_http::FrontedHttpTransport;
use geph5_broker_protocol::BrokerClient;
use nanorpc::DynRpcTransport;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sillad::tcp::TcpDialer;
use std::net::SocketAddr;

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
            BrokerSource::Direct(s) => DynRpcTransport::new(FrontedHttpTransport {
                url: s.clone(),
                host: None,
                client,
            }),
            BrokerSource::DirectTcp(dest_addr) => {
                DynRpcTransport::new(nanorpc_sillad::DialerTransport(TcpDialer {
                    dest_addr: *dest_addr,
                }))
            }
            BrokerSource::Fronted { front, host } => DynRpcTransport::new(FrontedHttpTransport {
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
