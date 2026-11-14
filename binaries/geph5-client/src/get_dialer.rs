use std::time::{Duration, Instant};

use anyctx::AnyCtx;
use anyhow::Context;
use ed25519_dalek::VerifyingKey;
use geph5_broker_protocol::{
    DOMAIN_EXIT_ROUTE, ExitConstraint, ExitDescriptor, ExitRouteDescriptor, GetExitRouteArgs,
    JsonSigned, RouteDescriptor,
};
use isocountry::CountryCode;
use rand::seq::IndexedRandom;
use sillad::dialer::{DialerExt, DynDialer, FailingDialer};

use crate::bound_dialer::BoundTcpDialer;
use sillad_conntest::ConnTestDialer;
use sillad_hex::HexDialer;
use sillad_meeklike::MeeklikeDialer;
use sillad_sosistab3::{Cookie, dialer::SosistabDialer};

use crate::{
    auth::get_connect_token,
    broker::broker_client,
    client::{Config, CtxField},
    device_metadata::get_device_metadata,
    dial_logging::logged,
    early_death::EARLY_DEATHS,
    route_cache::{read_cached_exit_route, write_cached_exit_route},
};

// A client context has a fixed exit constraint. Only a verified broker response
// starts the freshness window. Routes recovered from disk are fallback only.
static RECENT_EXIT_ROUTE: CtxField<moka::future::Cache<(), ExitRouteDescriptor>> = |_| {
    moka::future::Cache::builder()
        .time_to_live(Duration::from_secs(60))
        .build()
};

fn route_subtree_json(route: &RouteDescriptor) -> String {
    serde_json::to_string(route).expect("route subtree must serialize")
}

/// Gets a sillad Dialer that produces a single, pre-authentication pipe, as well as the public key.
pub async fn get_dialer(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<(VerifyingKey, ExitDescriptor, DynDialer)> {
    if let Some(forced) = &ctx.init().diagnostic_route {
        // Never consult the route cache, request another route, or add a direct
        // path when a caller explicitly pins a diagnostic route.
        return Ok((
            forced.exit_pubkey,
            forced.exit.clone(),
            route_to_dialer(ctx, &forced.route),
        ));
    }
    if let ExitConstraint::Direct(dir) = &ctx.init().exit_constraint {
        let (dir, pubkey_hex) = dir
            .split_once('/')
            .context("did not find / in a direct constraint")?;
        let pubkey = VerifyingKey::from_bytes(
            hex::decode(pubkey_hex)
                .context("cannot decode pubkey as hex")?
                .as_slice()
                .try_into()
                .context("pubkey wrong length")?,
        )?;
        let dest_addr = *tokio::net::lookup_host(dir)
            .await?
            .collect::<Vec<_>>()
            .choose(&mut rand::rng())
            .context("could not resolve destination for direct exit connection")?;
        let direct_route = RouteDescriptor::ConnTest {
            ping_count: 1,
            lower: Box::new(RouteDescriptor::Tcp(dest_addr)),
        };
        return Ok((
            pubkey,
            ExitDescriptor {
                c2e_listen: "0.0.0.0:0".parse()?,
                b2e_listen: "0.0.0.0:0".parse()?,
                country: CountryCode::ABW,
                city: "".to_string(),
                load: 0.0,
                expiry: 0,
            },
            logged(
                "overall",
                route_subtree_json(&direct_route),
                route_to_dialer(ctx, &direct_route),
            )
            .dynamic(),
        ));
    }

    let res = ctx
        .get(RECENT_EXIT_ROUTE)
        .try_get_with((), fetch_exit_route(ctx))
        .await;

    let exit_route = match res {
        Ok(val) => val,
        Err(err) => {
            tracing::warn!(err = %err, "failed to get fresh exit route");
            match read_cached_exit_route(ctx, &ctx.init().exit_constraint).await {
                Ok(Some(cached)) => {
                    tracing::warn!("returning cached exit route instead");
                    cached
                }
                Ok(None) => {
                    anyhow::bail!(
                        "fresh exit route unavailable and no cached route found: {err:#}"
                    );
                }
                Err(cache_err) => {
                    anyhow::bail!(
                        "fresh exit route unavailable ({err:#}) and cached route lookup failed: {cache_err:#}"
                    );
                }
            }
        }
    };

    dialer_from_exit_route(ctx, exit_route)
}

async fn fetch_exit_route(ctx: &AnyCtx<Config>) -> anyhow::Result<ExitRouteDescriptor> {
    let (_level, conn_token, sig) = get_connect_token(ctx)
        .await
        .context("could not get connect token")?;

    let start = Instant::now();
    // `get_device_metadata` fetches the device's public IP (for the broker's
    // ASN/country bridge tuning). In full-tunnel VPN mode it now resolves
    // checkip over the physical NIC's own DNS servers (see
    // `device_metadata`/`china::resolve_a_physical`), so it succeeds fast even
    // mid-reconnect and is cached per day — no more ~10s `getaddrinfo` hang.
    // The timeout is only a backstop; on any miss we still send at least the
    // `version` (which independently gates a bridge route), never bare null, so
    // a momentary IP miss doesn't also drop the version.
    const METADATA_BUDGET: Duration = Duration::from_secs(2);
    let version_only = || serde_json::json!({ "version": env!("CARGO_PKG_VERSION") });
    let metadata = match tokio::time::timeout(METADATA_BUDGET, get_device_metadata(ctx)).await {
        Ok(Ok(metadata)) => {
            tracing::debug!(
                metadata = debug(&metadata),
                elapsed = debug(start.elapsed()),
                "DEVICE METADATA OBTAINED"
            );
            serde_json::to_value(&metadata)?
        }
        Ok(Err(err)) => {
            tracing::warn!(
                err = debug(err),
                "could not get device metadata, proceeding with version only"
            );
            version_only()
        }
        Err(_elapsed) => {
            tracing::warn!(
                budget = debug(METADATA_BUDGET),
                "device metadata timed out, proceeding with version only"
            );
            version_only()
        }
    };

    let broker = broker_client(ctx)?;
    let signed_exit_route = broker
        .get_exit_route(GetExitRouteArgs {
            token: conn_token,
            sig,
            exit_constraint: ctx.init().exit_constraint.clone(),
            client_metadata: metadata,
        })
        .await?
        .map_err(|e| anyhow::anyhow!("broker refused to serve exit routes: {e}"))?;
    let exit_route = verify_exit_route(ctx, signed_exit_route)?;

    if let Err(err) = write_cached_exit_route(ctx, &ctx.init().exit_constraint, &exit_route).await {
        tracing::warn!(err = debug(&err), "could not persist exit route cache");
    }

    Ok(exit_route)
}

fn dialer_from_exit_route(
    ctx: &AnyCtx<Config>,
    exit_route: ExitRouteDescriptor,
) -> anyhow::Result<(VerifyingKey, ExitDescriptor, DynDialer)> {
    let ExitRouteDescriptor {
        exit_pubkey,
        exit,
        route,
    } = exit_route;

    tracing::debug!(exit = ?exit, "exit route obtained: {}", serde_json::to_string(&route)?);

    let combined_routes = combine_exit_route(exit.clone(), route, ctx.init().allow_direct);
    let bridge_dialer = logged(
        "overall",
        route_subtree_json(&combined_routes),
        route_to_dialer(ctx, &combined_routes),
    )
    .dynamic();

    Ok((exit_pubkey, exit, bridge_dialer))
}

fn verify_exit_route(
    ctx: &AnyCtx<Config>,
    signed: JsonSigned<ExitRouteDescriptor>,
) -> anyhow::Result<ExitRouteDescriptor> {
    signed
        .verify(DOMAIN_EXIT_ROUTE, |their_pk| {
            if let Some(broker_pk) = &ctx.init().broker_keys {
                hex::encode(their_pk.as_bytes()) == broker_pk.master
            } else {
                tracing::warn!("trusting exit route blindly since broker_keys was not provided");
                true
            }
        })
        .context("could not verify exit route")
}

fn combine_exit_route(
    exit: ExitDescriptor,
    route: RouteDescriptor,
    allow_direct: bool,
) -> RouteDescriptor {
    if allow_direct {
        RouteDescriptor::Race(vec![
            RouteDescriptor::ConnTest {
                ping_count: 1,
                lower: Box::new(RouteDescriptor::Tcp(exit.c2e_listen)),
            },
            RouteDescriptor::Delay {
                milliseconds: 1000,
                lower: Box::new(route),
            },
        ])
    } else {
        route
    }
}

fn route_to_dialer(ctx: &AnyCtx<Config>, route: &RouteDescriptor) -> DynDialer {
    // Each race or fallback child is built as a separate branch. Place the delay
    // outside the endpoint's own handshake and timeout.
    let mut endpoint = None;
    let dialer = build_dialer(ctx, route, &mut endpoint);
    match endpoint.and_then(|addr| ctx.get(EARLY_DEATHS).delay(addr)) {
        Some(delay) => dialer.delay(delay).dynamic(),
        None => dialer,
    }
}

fn build_dialer(
    ctx: &AnyCtx<Config>,
    route: &RouteDescriptor,
    endpoint: &mut Option<std::net::SocketAddr>,
) -> DynDialer {
    use sillad_native_tls::TlsDialer;

    match route {
        RouteDescriptor::Tcp(addr) => {
            let addr = *addr;
            *endpoint = Some(addr);
            logged(
                "tcp",
                route_subtree_json(route),
                BoundTcpDialer { dest_addr: addr },
            )
            .dynamic()
        }
        RouteDescriptor::Sosistab3 { cookie, lower } => {
            let inner = build_dialer(ctx, lower, endpoint);
            logged(
                "sosistab3",
                route_subtree_json(route),
                SosistabDialer {
                    inner,
                    cookie: Cookie::new(cookie),
                },
            )
            .dynamic()
        }
        RouteDescriptor::Race(inside) => inside
            .iter()
            .map(|s| route_to_dialer(ctx, s))
            .reduce(|a, b| a.race(b).dynamic())
            .unwrap_or_else(|| FailingDialer.dynamic()),
        RouteDescriptor::Fallback(a) => a
            .iter()
            .map(|s| route_to_dialer(ctx, s))
            .reduce(|a, b| a.fallback(b).dynamic())
            .unwrap_or_else(|| FailingDialer.dynamic()),
        RouteDescriptor::Timeout {
            milliseconds,
            lower,
        } => build_dialer(ctx, lower, endpoint)
            .timeout(Duration::from_millis(*milliseconds as _))
            .dynamic(),
        RouteDescriptor::Delay {
            milliseconds,
            lower,
        } => build_dialer(ctx, lower, endpoint)
            .delay(Duration::from_millis((*milliseconds).into()))
            .dynamic(),
        RouteDescriptor::ConnTest { ping_count, lower } => {
            let lower = build_dialer(ctx, lower, endpoint);
            logged(
                "conntest",
                route_subtree_json(route),
                ConnTestDialer {
                    inner: lower,
                    ping_count: *ping_count as _,
                },
            )
            .dynamic()
        }
        RouteDescriptor::Hex { lower } => {
            let lower = build_dialer(ctx, lower, endpoint);
            logged("hex", route_subtree_json(route), HexDialer { inner: lower }).dynamic()
        }
        RouteDescriptor::Other(_) => FailingDialer.dynamic(),
        RouteDescriptor::PlainTls { sni_domain, lower } => {
            let lower = build_dialer(ctx, lower, endpoint);
            let mut builder = native_tls::TlsConnector::builder();
            builder
                .use_sni(sni_domain.is_some())
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .min_protocol_version(None)
                .max_protocol_version(Some(native_tls::Protocol::Tlsv12));
            let connector = tokio_native_tls::TlsConnector::from(
                builder.build().expect("could not build TLS connector"),
            );
            logged(
                "tls",
                route_subtree_json(route),
                TlsDialer::new(
                    lower,
                    connector,
                    sni_domain
                        .clone()
                        .unwrap_or_else(|| "example.com".to_string()),
                ),
            )
            .dynamic()
        }
        RouteDescriptor::Meeklike { key, cfg, lower } => {
            let lower = build_dialer(ctx, lower, endpoint);
            logged(
                "meeklike",
                route_subtree_json(route),
                MeeklikeDialer {
                    inner: lower.into(),
                    cfg: *cfg,
                    key: *blake3::hash(key.as_bytes()).as_bytes(),
                },
            )
            .dynamic()
        }
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::client::{BrokerKeys, Config};

    fn test_config() -> Config {
        Config {
            socks5_listen: None,
            http_proxy_listen: None,
            pac_listen: None,
            control_listen: None,
            control_listen_unix: None,
            control_listen_pipe: None,
            exit_constraint: ExitConstraint::Auto,
            allow_direct: false,
            diagnostic_route: None,
            cache: None,
            logging: Default::default(),
            broker: None,
            tunneled_broker: None,
            broker_keys: None,
            port_forward: vec![],
            spoof_dns: false,
            passthrough_china: false,
            allow_lan: true,
            dry_run: true,
            credentials: Default::default(),
            sess_metadata: serde_json::Value::Null,
            task_limit: None,
        }
    }

    fn sample_exit_with_expiry(expiry: u64) -> ExitDescriptor {
        ExitDescriptor {
            c2e_listen: "127.0.0.1:9000".parse().unwrap(),
            b2e_listen: "127.0.0.1:9001".parse().unwrap(),
            country: CountryCode::CAN,
            city: "Toronto".into(),
            load: 0.1,
            expiry,
        }
    }

    fn sample_exit() -> ExitDescriptor {
        sample_exit_with_expiry(1)
    }

    fn sample_exit_route(expiry: u64) -> ExitRouteDescriptor {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        ExitRouteDescriptor {
            exit_pubkey: signing_key.verifying_key(),
            exit: sample_exit_with_expiry(expiry),
            route: RouteDescriptor::Tcp("127.0.0.1:9002".parse().unwrap()),
        }
    }

    #[test]
    fn ordinary_config_omits_diagnostic_route_for_older_engines() {
        let value = serde_json::to_value(test_config()).unwrap();
        assert!(value.get("diagnostic_route").is_none());
        let decoded: Config = serde_json::from_value(value).unwrap();
        assert!(decoded.diagnostic_route.is_none());
    }

    #[tokio::test]
    async fn diagnostic_route_bypasses_broker_cache_and_direct_constraint() {
        use sillad::dialer::Dialer;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut forced = sample_exit_route(0);
        forced.route = RouteDescriptor::Tcp(listener.local_addr().unwrap());
        let mut cfg = test_config();
        cfg.allow_direct = true;
        cfg.exit_constraint = ExitConstraint::Direct("intentionally invalid".into());
        cfg.diagnostic_route = Some(forced.clone());
        let (key, exit, dialer) = get_dialer(&AnyCtx::new(cfg)).await.unwrap();
        assert_eq!(key, forced.exit_pubkey);
        assert_eq!(exit, forced.exit);
        assert!(dialer.dial().await.is_ok());
    }

    #[tokio::test]
    async fn early_deaths_delay_only_the_affected_race_branch() {
        use sillad::{Pipe, dialer::Dialer};

        let ctx = AnyCtx::new(test_config());
        let bad = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bad_addr = bad.local_addr().unwrap();
        let good_addr = good.local_addr().unwrap();
        for _ in 0..3 {
            ctx.get(EARLY_DEATHS)
                .record(bad_addr, Duration::from_secs(1));
        }

        // The TCP timeout starts after the penalty, so the branch still works alone.
        let branch = RouteDescriptor::Timeout {
            milliseconds: 500,
            lower: Box::new(RouteDescriptor::Tcp(bad_addr)),
        };
        let race = RouteDescriptor::Race(vec![
            branch.clone(),
            RouteDescriptor::Delay {
                milliseconds: 50,
                lower: Box::new(RouteDescriptor::Tcp(good_addr)),
            },
        ]);
        let winner = route_to_dialer(&ctx, &race).dial().await.unwrap();
        assert_eq!(winner.remote_addr().unwrap().parse(), Ok(good_addr));
        let started = Instant::now();
        route_to_dialer(&ctx, &branch).dial().await.unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn failed_diagnostic_route_cannot_fall_back_to_direct_exit() {
        use sillad::dialer::Dialer;
        let direct = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_addr = closed.local_addr().unwrap();
        drop(closed);
        let mut forced = sample_exit_route(u64::MAX);
        forced.exit.c2e_listen = direct.local_addr().unwrap();
        forced.route = RouteDescriptor::Tcp(closed_addr);
        let mut cfg = test_config();
        cfg.allow_direct = true;
        cfg.diagnostic_route = Some(forced);
        let (_, _, dialer) = get_dialer(&AnyCtx::new(cfg)).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), dialer.dial())
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), direct.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn verify_exit_route_rejects_bad_signature() {
        let trusted = SigningKey::from_bytes(&[4; 32]);
        let attacker = SigningKey::from_bytes(&[5; 32]);
        let mut cfg = test_config();
        cfg.broker_keys = Some(BrokerKeys {
            master: hex::encode(trusted.verifying_key().as_bytes()),
            mizaru_free: String::new(),
            mizaru_plus: String::new(),
            mizaru_bw: String::new(),
        });
        let ctx = AnyCtx::new(cfg);
        let signed = JsonSigned::new(
            ExitRouteDescriptor {
                exit_pubkey: attacker.verifying_key(),
                exit: sample_exit(),
                route: RouteDescriptor::Tcp("127.0.0.1:9002".parse().unwrap()),
            },
            DOMAIN_EXIT_ROUTE,
            &attacker,
        );
        assert!(verify_exit_route(&ctx, signed).is_err());
    }

    #[test]
    fn combine_exit_route_wraps_direct_path() {
        let exit = sample_exit();
        let combined = combine_exit_route(
            exit.clone(),
            RouteDescriptor::Tcp("127.0.0.1:9002".parse().unwrap()),
            true,
        );
        match combined {
            RouteDescriptor::Race(routes) => {
                assert_eq!(routes.len(), 2);
                match &routes[0] {
                    RouteDescriptor::ConnTest { lower, .. } => match lower.as_ref() {
                        RouteDescriptor::Tcp(addr) => assert_eq!(*addr, exit.c2e_listen),
                        _ => panic!("expected direct tcp route"),
                    },
                    _ => panic!("expected direct route"),
                }
            }
            _ => panic!("expected race route"),
        }
    }

    #[tokio::test]
    async fn recently_fetched_route_is_reused_with_skewed_clocks() {
        let ctx = AnyCtx::new(test_config());
        for expiry in [0, u64::MAX] {
            ctx.get(RECENT_EXIT_ROUTE)
                .insert((), sample_exit_route(expiry))
                .await;
            let (_, exit, _) = get_dialer(&ctx).await.unwrap();
            assert_eq!(exit.expiry, expiry);
        }
    }
}
