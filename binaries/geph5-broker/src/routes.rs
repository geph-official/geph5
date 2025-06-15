use anyhow::Context;
use defmac::defmac;
use futures_util::{FutureExt as _, TryFutureExt};
use geph5_broker_protocol::{BridgeDescriptor, ExitDescriptor, RouteDescriptor};
use geph5_misc_rpc::bridge::{B2eMetadata, BridgeControlClient, ObfsProtocol};
use moka::future::Cache;
use nanorpc_sillad::DialerTransport;

use rand::RngCore;
use sillad::tcp::TcpDialer;
use sillad_sosistab3::{dialer::SosistabDialer, Cookie};
use smol_timeout2::TimeoutExt;
use std::{
    net::SocketAddr,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime},
};

pub async fn bridge_to_leaf_route(
    bridge: BridgeDescriptor,
    delay_ms: u32,
    exit: &ExitDescriptor,
    country: &str,
) -> anyhow::Result<RouteDescriptor> {
    // for cache coherence
    let mut bridge = bridge;
    bridge.expiry = 0;

    static CACHE: LazyLock<
        Cache<(BridgeDescriptor, SocketAddr, String), Result<RouteDescriptor, Arc<anyhow::Error>>>,
    > = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(600))
            .build()
    });

    CACHE
        .get_with(
            (bridge.clone(), exit.b2e_listen, country.to_string()),
            async {
                defmac!(tls_route => {
                    bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        ObfsProtocol::Sosistab3New(
                            gencookie(),
                            ObfsProtocol::PlainTls(ObfsProtocol::None.into()).into(),
                        )
                    )
                    .await?
                });
                defmac!(sosistab3_route => {
                    bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        ObfsProtocol::Sosistab3New(gencookie(), ObfsProtocol::None.into())
                    )
                    .await?
                });
                defmac!(legacy_route => {
                    bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        ObfsProtocol::None,
                    )
                    .await?
                });

                defmac!(foofoo_route => {
                    bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        ObfsProtocol::Sosistab3New(gencookie(), ObfsProtocol::ConnTest(ObfsProtocol::None.into()).into()),
                    )
                    .await?
                });

                if bridge.pool.contains("waw")
                    || bridge.pool.contains("ovh_de")
                    || country == "IR"
                    || country == "TM"
                {
                    anyhow::Ok(RouteDescriptor::Delay {
                        milliseconds: delay_ms,
                        lower: tls_route!().into(),
                    })
                } else if !country.is_empty(){
                    anyhow::Ok(RouteDescriptor::Delay {
                        milliseconds: delay_ms,
                        lower: sosistab3_route!().into(),
                    })
                } else {
                    anyhow::Ok(RouteDescriptor::Delay {
                        milliseconds: delay_ms,
                        lower: RouteDescriptor::Fallback(vec![tls_route!().timeout(Duration::from_secs(5)), legacy_route!()])
                            .into(),
                    })
                }
            }
            .map(|res| {
                if let Err(err) = res.as_ref() {
                    tracing::warn!(
                        "failed to find {} => {}: {:?}",
                        bridge.control_listen.ip(),
                        exit.b2e_listen,
                        err
                    )
                }
                res
            })
            .map_err(Arc::new),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
}

fn gencookie() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

async fn bridge_to_leaf_route_inner(
    bridge: BridgeDescriptor,
    exit_b2e: SocketAddr,
    protocol: ObfsProtocol,
) -> anyhow::Result<RouteDescriptor> {
    let cookie = Cookie::new(&bridge.control_cookie);

    let control_dialer = SosistabDialer {
        inner: TcpDialer {
            dest_addr: bridge.control_listen,
        },
        cookie,
    };

    let control_client = BridgeControlClient(DialerTransport(control_dialer));

    let sosistab_addr = control_client
        .tcp_forward(
            exit_b2e,
            B2eMetadata {
                protocol: protocol.clone(),
                expiry: SystemTime::now() + Duration::from_secs(86400),
            },
        )
        .timeout(Duration::from_secs(4))
        .await
        .context("timeout when sosistab")??;
    let final_route = protocol_to_descriptor(protocol, sosistab_addr);

    anyhow::Ok(final_route)
}

fn protocol_to_descriptor(protocol: ObfsProtocol, addr: SocketAddr) -> RouteDescriptor {
    match protocol {
        ObfsProtocol::Sosistab3(cookie) => RouteDescriptor::Sosistab3 {
            cookie,
            lower: RouteDescriptor::Tcp(addr).into(),
        },
        ObfsProtocol::None => RouteDescriptor::Tcp(addr),
        ObfsProtocol::ConnTest(obfs_protocol) => RouteDescriptor::ConnTest {
            ping_count: 1,
            lower: protocol_to_descriptor(*obfs_protocol, addr).into(),
        },
        ObfsProtocol::PlainTls(obfs_protocol) => RouteDescriptor::PlainTls {
            sni_domain: Some("labooyah-squish.be".into()),
            lower: protocol_to_descriptor(*obfs_protocol, addr).into(),
        },
        ObfsProtocol::Sosistab3New(cookie, obfs_protocol) => RouteDescriptor::Sosistab3 {
            cookie,
            lower: protocol_to_descriptor(*obfs_protocol, addr).into(),
        },
    }
}
