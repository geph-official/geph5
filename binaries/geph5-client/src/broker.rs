mod bind_forward;
mod fronted_http;
mod priority_race;
mod race;
mod tunneled_http;

#[cfg(feature = "aws_lambda")]
mod aws_lambda;

use anyctx::AnyCtx;
use anyhow::Context;

#[cfg(feature = "aws_lambda")]
use aws_lambda::AwsLambdaTransport;
use fronted_http::FrontedHttpTransport;
use geph5_broker_protocol::{BrokerClient, DOMAIN_NET_STATUS, NetStatus};
use itertools::Itertools;
use nanorpc::{DynRpcTransport, JrpcRequest, JrpcResponse, RpcTransport};
use priority_race::PriorityRaceTransport;
use race::RaceTransport;
use tunneled_http::TunneledHttpTransport;

use crate::{
    bound_dialer::BoundTcpDialer,
    client::CtxField,
    control_prot::CURRENT_CONNECTED_INFOS,
    timeout::{BROKER_RPC_TIMEOUT, RpcTransportExt},
};

// The descriptor *types* live in geph5-misc-rpc (so the manager/CLI can configure
// an engine without depending on it); the behavior that turns them into live
// transports stays here on the `ConfigHelperExt` extension trait. Re-exported so
// `geph5_client::broker` paths keep working.
pub use geph5_misc_rpc::client_config::{BrokerSource, TunneledBrokerSource};
use geph5_misc_rpc::client_config::Config;

/// Engine-side behavior bolted onto the plain config descriptors that live in
/// `geph5-misc-rpc`: turning a broker source into a live RPC transport.
pub(crate) trait ConfigHelperExt {
    /// Build a broker RPC transport from this source. `ctx` is used by sources
    /// that tunnel through the engine; direct sources ignore it.
    fn rpc_transport(&self, ctx: &AnyCtx<Config>) -> DynRpcTransport;
}

impl ConfigHelperExt for BrokerSource {
    fn rpc_transport(&self, ctx: &AnyCtx<Config>) -> DynRpcTransport {
        // In full-tunnel VPN mode, a broker source that needs DNS resolution
        // cannot bootstrap: its `getaddrinfo` isn't physical-NIC-pinned (we can't
        // intercept the system resolver's sockets), so the query would route into
        // the not-yet-established tunnel and hang. Ignore those sources entirely;
        // only DNS-free ones (fronted with `override_dns`, and direct-TCP) can
        // reach the broker before the tunnel is up.
        if crate::bound_dialer::binding_active() {
            let skip = match self {
                BrokerSource::Direct(_) => Some("direct"),
                BrokerSource::Fronted {
                    override_dns: None, ..
                } => Some("fronted-without-override_dns"),
                BrokerSource::AwsLambda { .. } => Some("aws_lambda"),
                _ => None,
            };
            if let Some(kind) = skip {
                tracing::warn!(
                    source = kind,
                    "ignoring DNS-dependent broker source in full-tunnel VPN mode"
                );
                return DynRpcTransport::new(
                    UnsupportedBrokerTransport("DNS-dependent broker source skipped in VPN mode")
                        .timeout(BROKER_RPC_TIMEOUT),
                );
            }
        }
        match self {
            BrokerSource::Direct(s) => DynRpcTransport::new(
                FrontedHttpTransport {
                    url: s.clone(),
                    host: None,
                    dns: None,
                }
                .timeout(BROKER_RPC_TIMEOUT),
            ),
            BrokerSource::DirectTcp(dest_addr) => DynRpcTransport::new(
                nanorpc_sillad::DialerTransport(BoundTcpDialer {
                    dest_addr: *dest_addr,
                })
                .timeout(BROKER_RPC_TIMEOUT),
            ),
            BrokerSource::Fronted {
                front,
                host,
                override_dns,
            } => DynRpcTransport::new(
                FrontedHttpTransport {
                    url: front.clone(),
                    host: Some(host.clone()),
                    dns: override_dns.clone(),
                }
                .timeout(BROKER_RPC_TIMEOUT),
            ),
            #[cfg(feature = "aws_lambda")]
            BrokerSource::AwsLambda {
                function_name,
                region,
                obfs_key,
            } => DynRpcTransport::new(
                AwsLambdaTransport {
                    function_name: function_name.clone(),
                    region: region.clone(),
                    obfs_key: obfs_key.clone(),
                }
                .timeout(BROKER_RPC_TIMEOUT),
            ),
            #[cfg(not(feature = "aws_lambda"))]
            BrokerSource::AwsLambda { .. } => DynRpcTransport::new(
                UnsupportedBrokerTransport("aws_lambda").timeout(BROKER_RPC_TIMEOUT),
            ),
            BrokerSource::Race(race_between) => {
                let transports = race_between
                    .iter()
                    .map(|bs| bs.rpc_transport(ctx))
                    .collect_vec();
                DynRpcTransport::new(RaceTransport::new(transports))
            }
            BrokerSource::PriorityRace(inner) => {
                let inner = inner
                    .iter()
                    .map(|(k, v)| (*k, v.rpc_transport(ctx)))
                    .collect();
                DynRpcTransport::new(PriorityRaceTransport::new(inner))
            }
        }
    }
}

impl ConfigHelperExt for TunneledBrokerSource {
    fn rpc_transport(&self, ctx: &AnyCtx<Config>) -> DynRpcTransport {
        match self {
            TunneledBrokerSource::Direct(url) => DynRpcTransport::new(
                TunneledHttpTransport::new(ctx.clone(), url.clone()).timeout(BROKER_RPC_TIMEOUT),
            ),
        }
    }
}

struct UnsupportedBrokerTransport(&'static str);

#[async_trait::async_trait]
impl nanorpc::RpcTransport for UnsupportedBrokerTransport {
    type Error = anyhow::Error;

    async fn call_raw(
        &self,
        _req: nanorpc::JrpcRequest,
    ) -> Result<nanorpc::JrpcResponse, Self::Error> {
        Err(anyhow::anyhow!(
            "broker source '{}' is unsupported in this build",
            self.0
        ))
    }
}

pub fn broker_client(ctx: &AnyCtx<Config>) -> anyhow::Result<&BrokerClient> {
    ctx.get(BROKER_CLIENT).as_ref().context(
        "broker information not provided, so cannot use any broker-dependent functionality",
    )
}

static BROKER_CLIENT: CtxField<Option<BrokerClient>> = |ctx| {
    ctx.init().broker.as_ref().map(|src| {
        let normal = src.rpc_transport(ctx);
        let tunneled = ctx
            .init()
            .tunneled_broker
            .as_ref()
            .map(|src| src.rpc_transport(ctx));
        BrokerClient::from(DynRpcTransport::new(SwitchingBrokerTransport {
            normal,
            tunneled,
            is_connected: std::sync::Arc::new({
                let ctx = ctx.clone();
                move || !ctx.get(CURRENT_CONNECTED_INFOS).lock().is_empty()
            }),
        }))
    })
};

struct SwitchingBrokerTransport {
    normal: DynRpcTransport,
    tunneled: Option<DynRpcTransport>,
    is_connected: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
}

#[async_trait::async_trait]
impl RpcTransport for SwitchingBrokerTransport {
    type Error = anyhow::Error;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        let should_try_tunneled = self.tunneled.is_some() && (self.is_connected)();
        if should_try_tunneled {
            let tunneled = self.tunneled.as_ref().unwrap();
            match tunneled.call_raw(req.clone()).await {
                Ok(response) => return Ok(response),
                Err(err) => {
                    tracing::warn!(
                        err = debug(&err),
                        "tunneled broker RPC failed, falling back"
                    );
                }
            }
        }
        self.normal.call_raw(req).await
    }
}

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
    fn deserializes_aws_lambda_when_feature_disabled() {
        let parsed: BrokerSource = serde_json::from_str(
            r#"{"aws_lambda":{"function_name":"f","region":"us-east-1","obfs_key":"k"}}"#,
        )
        .unwrap();

        match parsed {
            BrokerSource::AwsLambda {
                function_name,
                region,
                obfs_key,
            } => {
                assert_eq!(function_name, "f");
                assert_eq!(region, "us-east-1");
                assert_eq!(obfs_key, "k");
            }
            other => panic!("expected other broker source, got {:?}", kind_of(&other)),
        }
    }

    fn kind_of(source: &BrokerSource) -> &'static str {
        match source {
            BrokerSource::Direct(_) => "direct",
            BrokerSource::Fronted { .. } => "fronted",
            BrokerSource::DirectTcp(_) => "direct_tcp",
            BrokerSource::AwsLambda { .. } => "aws_lambda",
            BrokerSource::Race(_) => "race",
            BrokerSource::PriorityRace(_) => "priority_race",
        }
    }
}
