use anyhow::Context;
use futures_util::TryFutureExt;
use geph5_broker_protocol::{BridgeDescriptor, RouteDescriptor};
use geph5_misc_rpc::bridge::{B2eMetadata, BridgeControlClient, ObfsProtocol};
use moka::future::Cache;
use nanorpc_sillad::DialerTransport;
use once_cell::sync::Lazy;
use sillad::tcp::TcpDialer;
use sillad_sosistab3::{dialer::SosistabDialer, Cookie};
use smol_timeout2::TimeoutExt;
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

pub async fn bridge_to_leaf_route(
    bridge: BridgeDescriptor,
    delay_ms: u32,
    exit_b2e: SocketAddr,
) -> anyhow::Result<RouteDescriptor> {
    // let test = bridge_to_leaf_route_inner(bridge.clone(), delay_ms, exit_b2e, true).await?;
    let no_test = bridge_to_leaf_route_inner(bridge, delay_ms, exit_b2e, false).await?;
    // Ok(RouteDescriptor::Fallback(vec![no_test]))
    Ok(no_test)
}

async fn bridge_to_leaf_route_inner(
    bridge: BridgeDescriptor,
    delay_ms: u32,
    exit_b2e: SocketAddr,
    conn_test: bool,
) -> anyhow::Result<RouteDescriptor> {
    static CACHE: Lazy<
        Cache<(SocketAddr, SocketAddr), Result<RouteDescriptor, Arc<anyhow::Error>>>,
    > = Lazy::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(60))
            .build()
    });

    let cookie = Cookie::new(&bridge.control_cookie);

    let wrap = |s: ObfsProtocol| {
        if conn_test {
            ObfsProtocol::ConnTest(s.into())
        } else {
            s
        }
    };

    CACHE
        .get_with(
            (bridge.control_listen, exit_b2e),
            async {
                let dialer = SosistabDialer {
                    inner: TcpDialer {
                        dest_addr: bridge.control_listen,
                    },
                    cookie,
                };

                let cookie = format!("exit-cookie-{}", rand::random::<u128>());
                let control_client = BridgeControlClient(DialerTransport(dialer));

                let sosistab_addr = control_client
                    .tcp_forward(
                        exit_b2e,
                        B2eMetadata {
                            protocol: wrap(ObfsProtocol::Sosistab3(cookie.clone())),
                            expiry: SystemTime::now() + Duration::from_secs(86400),
                        },
                    )
                    .timeout(Duration::from_secs(1))
                    .await
                    .context("timeout")??;
                let sosis_route = RouteDescriptor::Sosistab3 {
                    cookie,
                    lower: RouteDescriptor::Tcp(sosistab_addr).into(),
                };

                let final_route = if bridge.pool.contains("ovh") {
                    let plain_addr = control_client
                        .tcp_forward(
                            exit_b2e,
                            B2eMetadata {
                                protocol: wrap(ObfsProtocol::None),
                                expiry: SystemTime::now() + Duration::from_secs(86400),
                            },
                        )
                        .timeout(Duration::from_secs(1))
                        .await
                        .context("timeout")??;
                    let plain_route = RouteDescriptor::Delay {
                        milliseconds: 0,
                        lower: RouteDescriptor::Tcp(plain_addr).into(),
                    };
                    RouteDescriptor::Delay {
                        milliseconds: 500,
                        lower: RouteDescriptor::Race(vec![plain_route, sosis_route]).into(),
                    }
                } else {
                    sosis_route
                };

                let final_route = if delay_ms > 0 {
                    RouteDescriptor::Delay {
                        milliseconds: delay_ms,
                        lower: final_route.into(),
                    }
                } else {
                    final_route
                };

                anyhow::Ok(final_route)
            }
            .map_err(Arc::new),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
}
