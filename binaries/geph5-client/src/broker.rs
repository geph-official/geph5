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
use geph5_broker_protocol::{BrokerClient, DOMAIN_NET_STATUS, NetStatus};
use itertools::Itertools;
use nanorpc::DynRpcTransport;
use priority_race::PriorityRaceTransport;
use race::RaceTransport;

use serde::{Deserialize, Serialize};
use sillad::tcp::TcpDialer;
use std::{collections::BTreeMap, net::SocketAddr};

use crate::client::{Config, CtxField};

#[derive(Serialize, Clone)]
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
    Other(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawBrokerSource {
    Direct {
        direct: String,
    },
    Fronted {
        fronted: FrontedFields,
    },
    DirectTcp {
        direct_tcp: SocketAddr,
    },
    #[cfg(feature = "aws_lambda")]
    AwsLambda {
        aws_lambda: AwsLambdaFields,
    },
    #[cfg(not(feature = "aws_lambda"))]
    AwsLambdaUnsupported {
        #[serde(rename = "aws_lambda")]
        _aws_lambda: serde::de::IgnoredAny,
    },
    Race {
        race: Vec<BrokerSource>,
    },
    PriorityRace {
        priority_race: BTreeMap<u64, BrokerSource>,
    },
    Other(BTreeMap<String, serde_json::Value>),
}

#[derive(Deserialize)]
struct FrontedFields {
    front: String,
    host: String,
    #[serde(default)]
    override_dns: Option<Vec<SocketAddr>>,
}

#[cfg(feature = "aws_lambda")]
#[derive(Deserialize)]
struct AwsLambdaFields {
    function_name: String,
    region: String,
    obfs_key: String,
}

impl<'de> Deserialize<'de> for BrokerSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawBrokerSource::deserialize(deserializer)?;
        Ok(match raw {
            RawBrokerSource::Direct { direct } => BrokerSource::Direct(direct),
            RawBrokerSource::Fronted { fronted } => BrokerSource::Fronted {
                front: fronted.front,
                host: fronted.host,
                override_dns: fronted.override_dns,
            },
            RawBrokerSource::DirectTcp { direct_tcp } => BrokerSource::DirectTcp(direct_tcp),
            #[cfg(feature = "aws_lambda")]
            RawBrokerSource::AwsLambda { aws_lambda } => BrokerSource::AwsLambda {
                function_name: aws_lambda.function_name,
                region: aws_lambda.region,
                obfs_key: aws_lambda.obfs_key,
            },
            #[cfg(not(feature = "aws_lambda"))]
            RawBrokerSource::AwsLambdaUnsupported { .. } => {
                warn_unknown_broker_source("aws_lambda");
                BrokerSource::Other("aws_lambda".to_string())
            }
            RawBrokerSource::Race { race } => BrokerSource::Race(race),
            RawBrokerSource::PriorityRace { priority_race } => {
                BrokerSource::PriorityRace(priority_race)
            }
            RawBrokerSource::Other(other) => {
                let kind = other
                    .into_iter()
                    .next()
                    .map(|(key, _)| key)
                    .unwrap_or_else(|| "unknown".to_string());
                warn_unknown_broker_source(&kind);
                BrokerSource::Other(kind)
            }
        })
    }
}

fn warn_unknown_broker_source(kind: &str) {
    tracing::warn!(
        broker_source = kind,
        "unknown or unsupported broker source in config; ignoring it until used"
    );
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
            BrokerSource::Other(kind) => DynRpcTransport::new(UnsupportedBrokerTransport {
                kind: kind.clone(),
            }),
        }
    }
}

struct UnsupportedBrokerTransport {
    kind: String,
}

#[async_trait::async_trait]
impl nanorpc::RpcTransport for UnsupportedBrokerTransport {
    type Error = anyhow::Error;

    async fn call_raw(
        &self,
        _req: nanorpc::JrpcRequest,
    ) -> Result<nanorpc::JrpcResponse, Self::Error> {
        Err(anyhow::anyhow!(
            "broker source '{}' is unsupported in this build",
            self.kind
        ))
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

pub async fn get_net_status(ctx: &AnyCtx<Config>) -> anyhow::Result<NetStatus> {
    let broker = broker_client(ctx).context("could not get broker client")?;
    let net_status_response = broker
        .get_net_status()
        .await?
        .map_err(|e| anyhow::anyhow!("broker refused to serve exits: {e}"))?;

    // Verify the broker's signature over the net status:
    let net_status_verified = net_status_response
        .verify(DOMAIN_NET_STATUS, |their_pk| {
            if let Some(broker_pk) = &ctx.init().broker_keys {
                hex::encode(their_pk.as_bytes()) == broker_pk.master
            } else {
                tracing::warn!("trusting netstatus blindly since broker_keys was not provided");
                true
            }
        })
        .context("could not verify net status")?;

    Ok(net_status_verified)
}

#[cfg(test)]
mod tests {
    use super::BrokerSource;

    #[test]
    fn deserializes_unknown_broker_source_as_other() {
        let parsed: BrokerSource =
            serde_json::from_str(r#"{"future_transport":{"foo":"bar"}}"#).unwrap();

        match parsed {
            BrokerSource::Other(kind) => assert_eq!(kind, "future_transport"),
            other => panic!("expected other broker source, got {:?}", kind_of(&other)),
        }
    }

    #[cfg(not(feature = "aws_lambda"))]
    #[test]
    fn deserializes_aws_lambda_as_other_when_feature_disabled() {
        let parsed: BrokerSource = serde_json::from_str(
            r#"{"aws_lambda":{"function_name":"f","region":"us-east-1","obfs_key":"k"}}"#,
        )
        .unwrap();

        match parsed {
            BrokerSource::Other(kind) => assert_eq!(kind, "aws_lambda"),
            other => panic!("expected other broker source, got {:?}", kind_of(&other)),
        }
    }

    fn kind_of(source: &BrokerSource) -> &'static str {
        match source {
            BrokerSource::Direct(_) => "direct",
            BrokerSource::Fronted { .. } => "fronted",
            BrokerSource::DirectTcp(_) => "direct_tcp",
            #[cfg(feature = "aws_lambda")]
            BrokerSource::AwsLambda { .. } => "aws_lambda",
            BrokerSource::Race(_) => "race",
            BrokerSource::PriorityRace(_) => "priority_race",
            BrokerSource::Other(_) => "other",
        }
    }
}
