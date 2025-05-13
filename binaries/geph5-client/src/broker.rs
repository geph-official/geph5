mod fronted_http;
mod priority_race;
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
use priority_race::PriorityRaceTransport;
use race::RaceTransport;

use serde::{Deserialize, Serialize};
use sillad::tcp::TcpDialer;
use std::{collections::BTreeMap, net::SocketAddr};

use crate::client::{Config, CtxField};

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum BrokerSource {
    Direct(String),
    Fronted {
        front: String,
        host: String,
        #[serde(default)]
        override_dns: Option<Vec<SocketAddr>>,
    },
    DirectTcp(SocketAddr),
    #[cfg(feature = "aws_lambda")]
    AwsLambda {
        function_name: String,
        region: String,
        obfs_key: String,
    },
    Race(Vec<BrokerSource>),
    PriorityRace(BTreeMap<u64, BrokerSource>),
}

impl BrokerSource {
    /// Converts to a RpcTransport.
    pub fn rpc_transport(&self) -> DynRpcTransport {
        match self {
            BrokerSource::Direct(s) => DynRpcTransport::new(FrontedHttpTransport {
                url: s.clone(),
                host: None,
                dns: None,
            }),
            BrokerSource::DirectTcp(dest_addr) => {
                DynRpcTransport::new(nanorpc_sillad::DialerTransport(TcpDialer {
                    dest_addr: *dest_addr,
                }))
            }
            BrokerSource::Fronted {
                front,
                host,
                override_dns,
            } => DynRpcTransport::new(FrontedHttpTransport {
                url: front.clone(),
                host: Some(host.clone()),
                dns: override_dns.clone(),
            }),
            #[cfg(feature = "aws_lambda")]
            BrokerSource::AwsLambda {
                function_name,
                region,
                obfs_key,
            } => DynRpcTransport::new(AwsLambdaTransport {
                function_name: function_name.clone(),
                region: region.clone(),
                obfs_key: obfs_key.clone(),
            }),
            BrokerSource::Race(race_between) => {
                let transports = race_between
                    .iter()
                    .map(|bs| bs.rpc_transport())
                    .collect_vec();
                DynRpcTransport::new(RaceTransport::new(transports))
            }
            BrokerSource::PriorityRace(inner) => {
                let inner = inner.iter().map(|(k, v)| (*k, v.rpc_transport())).collect();
                DynRpcTransport::new(PriorityRaceTransport::new(inner))
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
