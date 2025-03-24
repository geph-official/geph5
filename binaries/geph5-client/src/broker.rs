mod fronted_http;
mod race;

#[cfg(feature = "aws_lambda")]
mod aws_lambda;

use anyctx::AnyCtx;
use anyhow::Context;

#[cfg(feature = "aws_lambda")]
use aws_lambda::AwsLambdaTransport;
use fronted_http::FrontedHttpTransport;
use geph5_broker_protocol::BrokerClient;
use itertools::Itertools;
use nanorpc::DynRpcTransport;
use race::RaceTransport;

use serde::{Deserialize, Serialize};
use sillad::tcp::TcpDialer;
use std::net::SocketAddr;

use crate::client::{Config, CtxField};

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum BrokerSource {
    Direct(String),
    Fronted {
        front: String,
        host: String,
    },
    DirectTcp(SocketAddr),
    #[cfg(feature = "aws_lambda")]
    AwsLambda {
        function_name: String,
        region: String,
        access_key_id: String,
        secret_access_key: String,
    },
    Race(Vec<BrokerSource>),
}

impl BrokerSource {
    /// Converts to a RpcTransport.
    pub fn rpc_transport(&self) -> DynRpcTransport {
        match self {
            BrokerSource::Direct(s) => DynRpcTransport::new(FrontedHttpTransport {
                url: s.clone(),
                host: None,
            }),
            BrokerSource::DirectTcp(dest_addr) => {
                DynRpcTransport::new(nanorpc_sillad::DialerTransport(TcpDialer {
                    dest_addr: *dest_addr,
                }))
            }
            BrokerSource::Fronted { front, host } => DynRpcTransport::new(FrontedHttpTransport {
                url: front.clone(),
                host: Some(host.clone()),
            }),
            #[cfg(feature = "aws_lambda")]
            BrokerSource::AwsLambda {
                function_name,
                region,
                access_key_id,
                secret_access_key,
            } => DynRpcTransport::new(AwsLambdaTransport {
                function_name: function_name.clone(),
                region: region.clone(),
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
            }),
            BrokerSource::Race(race_between) => {
                let transports = race_between
                    .iter()
                    .map(|bs| bs.rpc_transport())
                    .collect_vec();
                DynRpcTransport::new(RaceTransport::new(transports))
            }
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
