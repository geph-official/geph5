use anyhow::Context;
use defmac::defmac;
use futures_util::{FutureExt as _, TryFutureExt};
use geph5_broker_protocol::{BridgeDescriptor, ExitDescriptor, RouteDescriptor};
use geph5_misc_rpc::bridge::{B2eMetadata, BridgeControlClient, ObfsProtocol};
use moka::future::Cache;
use nanorpc_sillad::DialerTransport;

use rand::RngCore;
use semver::VersionReq;
use sillad::tcp::TcpDialer;
use sillad_sosistab3::{Cookie, dialer::SosistabDialer};
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
    asn: u32,
    version: &str,
) -> anyhow::Result<RouteDescriptor> {
    // for cache coherence
    let mut bridge = bridge;
    bridge.expiry = 0;

    static CACHE: LazyLock<
        Cache<(BridgeDescriptor, SocketAddr, u32), Result<RouteDescriptor, Arc<anyhow::Error>>>,
    > = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(600))
            .build()
    });

    CACHE
        .get_with(
            (bridge.clone(), exit.b2e_listen, asn),
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

                });
                defmac!(sosistab3_route => {
                    bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        ObfsProtocol::ConnTest(ObfsProtocol::Sosistab3New(gencookie(), ObfsProtocol::None.into()).into())
                    )

                });
                defmac!(legacy_route => {
                    bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        ObfsProtocol::Sosistab3(gencookie()),
                    )
                });
                defmac!(plain_route => {
                    bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        ObfsProtocol::ConnTest(ObfsProtocol::None.into()).into(),
                    )
                });
                defmac!(meeklike_route => {
                    bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        ObfsProtocol::Meeklike(gencookie(), Default::default(), ObfsProtocol::None.into()),
                    )
                });

                if let Ok(version) = semver::Version::parse(version) &&
                VersionReq::parse(">=0.2.72").unwrap().matches(&version) &&
                bridge.pool.contains("ovh_de") && // only have one group do this
                (
                    asn == 197207 || // hamrah-e avval
                    asn == 44244 || // irancell
                    asn == 58244 // TCI
                )
                  {
                    return anyhow::Ok(RouteDescriptor::Delay {
                        milliseconds: delay_ms,
                        lower: RouteDescriptor::Race(vec![RouteDescriptor::Delay{milliseconds: 10000, lower: meeklike_route!().await?.into()}, tls_route!().await?, sosistab3_route!().await?]).into(),
                    })
                }

                // if asn == 9808 || asn == 56044 || asn == 56047 || asn == 58807 || asn == 56048 || asn == 56040 || asn == 56047 {
                //      anyhow::Ok(RouteDescriptor::Delay {
                //         milliseconds: delay_ms,
                //         lower: tls_route!().await?.into(),
                //     })
                // } else
                if !country.is_empty(){
                    anyhow::Ok(RouteDescriptor::Delay {
                        milliseconds: delay_ms,
                        lower: tls_route!().await?.into(),
                    })
                    // anyhow::Ok(RouteDescriptor::Delay {
                    //     milliseconds: delay_ms,
                    //     lower: sosistab3_route!().await?.into(),
                    // })
                } else {
                    anyhow::Ok(RouteDescriptor::Delay {
                        milliseconds: delay_ms,
                        lower: RouteDescriptor::Fallback(vec![RouteDescriptor::Timeout{milliseconds: 5000, lower: sosistab3_route!().await?.into()}, legacy_route!().await?])
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
        ObfsProtocol::Meeklike(key, cfg, obfs_protocol) => RouteDescriptor::Meeklike {
            key,
            cfg,
            lower: protocol_to_descriptor(*obfs_protocol, addr).into(),
        },
        ObfsProtocol::Hex(obfs_protocol) => RouteDescriptor::Hex {
            lower: protocol_to_descriptor(*obfs_protocol, addr).into(),
        },
    }
}
