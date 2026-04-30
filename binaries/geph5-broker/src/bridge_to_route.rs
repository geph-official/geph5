use anyhow::Context;
use futures_util::{FutureExt as _, TryFutureExt};
use geph5_broker_protocol::{BridgeDescriptor, ExitDescriptor, RouteDescriptor};
use geph5_misc_rpc::bridge::{B2eMetadata, BridgeControlClient, ObfsProtocol};
use moka::future::Cache;
use nanorpc_sillad::DialerTransport;

use rand::{Rng as _, RngCore};
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
    let country_key: Arc<str> = country.to_ascii_uppercase().into();
    let blocked_iran = bridge.pool.contains("NOIR");
    let is_iran = &*country_key == "IR" || (!blocked_iran && bridge.pool.contains("IR"));

    // for cache coherence
    let mut bridge = bridge;
    bridge.expiry = 0;

    static CACHE: LazyLock<
        Cache<
            (BridgeDescriptor, SocketAddr, u32, Arc<str>),
            Result<RouteDescriptor, Arc<anyhow::Error>>,
        >,
    > = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(600))
            .build()
    });

    CACHE
        .get_with(
            (bridge.clone(), exit.b2e_listen, asn, country_key.clone()),
            async {
                if let Ok(version) = semver::Version::parse(version)
                    && VersionReq::parse(">=0.2.72").unwrap().matches(&version)
                    && bridge.pool.contains("meeklike")
                {
                    let fallback_protocol = if is_iran {
                        tls_protocol()
                    } else {
                        sosistab3_protocol()
                    };
                    let fallback_route = bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        fallback_protocol,
                    )
                    .await?;
                    let meeklike_route = bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        meeklike_protocol(),
                    )
                    .await?;
                    return anyhow::Ok(RouteDescriptor::Delay {
                        milliseconds: delay_ms,
                        lower: RouteDescriptor::Race(vec![
                            RouteDescriptor::Delay {
                                milliseconds: 10000,
                                lower: meeklike_route.into(),
                            },
                            fallback_route,
                        ])
                        .into(),
                    });
                }

                // if asn == 4837 || asn == 4808 {
                //     return anyhow::Ok(RouteDescriptor::Delay {
                //         milliseconds: delay_ms,
                //         lower: route.into(),
                //     })
                // } else
                // } else if is_china_mobile_asn(asn) {
                //     return anyhow::Ok(RouteDescriptor::Delay {
                //         milliseconds: delay_ms,
                //         lower: route.into(),
                //     })
                // } else
                // if is_iran {
                //     return anyhow::Ok(RouteDescriptor::Delay {
                //         milliseconds: delay_ms,
                //         lower: route.into(),
                //     });
                // }
                let protocol = if country == "CN" {
                    tls_protocol()
                } else if country == "RU" {
                    tls_protocol()
                } else if !country.is_empty() {
                    // anyhow::Ok(RouteDescriptor::Delay {
                    //     milliseconds: delay_ms,
                    //     lower: route.into(),
                    // })
                    sosistab3_protocol()
                } else {
                    let sosistab3_route = bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        sosistab3_protocol(),
                    )
                    .await?;
                    let legacy_route = bridge_to_leaf_route_inner(
                        bridge.clone(),
                        exit.b2e_listen,
                        legacy_protocol(),
                    )
                    .await?;
                    return anyhow::Ok(RouteDescriptor::Delay {
                        milliseconds: delay_ms,
                        lower: RouteDescriptor::Fallback(vec![
                            RouteDescriptor::Timeout {
                                milliseconds: 5000,
                                lower: sosistab3_route.into(),
                            },
                            legacy_route,
                        ])
                        .into(),
                    });
                };

                let route =
                    bridge_to_leaf_route_inner(bridge.clone(), exit.b2e_listen, protocol).await?;
                anyhow::Ok(RouteDescriptor::Delay {
                    milliseconds: delay_ms,
                    lower: route.into(),
                })
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

fn naked_protocol() -> ObfsProtocol {
    ObfsProtocol::None
}

fn tls_protocol() -> ObfsProtocol {
    ObfsProtocol::ConnTest(
        ObfsProtocol::Sosistab3New(
            gencookie(),
            ObfsProtocol::PlainTls(ObfsProtocol::None.into()).into(),
        )
        .into(),
    )
}

fn sosistab3_protocol() -> ObfsProtocol {
    ObfsProtocol::ConnTest(
        ObfsProtocol::Sosistab3New(gencookie(), ObfsProtocol::None.into()).into(),
    )
}

fn weird_protocol() -> ObfsProtocol {
    ObfsProtocol::ConnTest(
        ObfsProtocol::Sosistab3New(
            gencookie(),
            ObfsProtocol::Sosistab3New(gencookie(), ObfsProtocol::None.into()).into(),
        )
        .into(),
    )
}

fn weirdest_protocol() -> ObfsProtocol {
    let mut rng = rand::thread_rng();
    let layer_count = rng.gen_range(3..=10);
    let mut protocol = ObfsProtocol::None;

    for _ in 0..layer_count {
        protocol = if rng.gen_bool(0.5) {
            ObfsProtocol::Sosistab3New(gencookie(), protocol.into())
        } else {
            ObfsProtocol::PlainTls(protocol.into())
        };
    }

    ObfsProtocol::ConnTest(protocol.into())
}

fn legacy_protocol() -> ObfsProtocol {
    ObfsProtocol::Sosistab3(gencookie())
}

fn meeklike_protocol() -> ObfsProtocol {
    ObfsProtocol::Meeklike(gencookie(), Default::default(), ObfsProtocol::None.into())
}

fn is_china_mobile_asn(asn: u32) -> bool {
    matches!(
        asn,
        // APNIC AS-SET AS9808:AS-CMNET members, plus active China Mobile province
        // and China Mobile International origin ASNs observed in public BGP data.
        9231 | 9808
            | 9812
            | 24059
            | 24137
            | 24311
            | 24400
            | 24409
            | 24422
            | 24444
            | 24445
            | 24547
            | 38019
            | 45108
            | 45112
            | 56040
            | 56041
            | 56042
            | 56043
            | 56044
            | 56045
            | 56046
            | 56047
            | 56048
            | 58453
            | 58807
            | 209141
    )
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
            ping_count: 2,
            lower: protocol_to_descriptor(*obfs_protocol, addr).into(),
        },
        ObfsProtocol::PlainTls(obfs_protocol) => RouteDescriptor::PlainTls {
            sni_domain: Some("lusheeta-toel.yandex.ru".into()),
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
