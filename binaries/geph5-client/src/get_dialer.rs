use std::time::{Duration, Instant};

use anyctx::AnyCtx;
use anyhow::Context;
use async_native_tls::TlsConnector;
use ed25519_dalek::VerifyingKey;
use geph5_broker_protocol::{
    DOMAIN_EXIT_ROUTE, ExitConstraint, ExitDescriptor, ExitRouteDescriptor, GetExitRouteArgs,
    JsonSigned, RouteDescriptor,
};
use isocountry::CountryCode;
use rand::seq::SliceRandom;
use sillad::{
    dialer::{DialerExt, DynDialer, FailingDialer},
    tcp::TcpDialer,
};
use sillad_conntest::ConnTestDialer;
use sillad_hex::HexDialer;
use sillad_meeklike::MeeklikeDialer;
use sillad_sosistab3::{Cookie, dialer::SosistabDialer};

use crate::{
    auth::get_connect_token,
    broker::broker_client,
    client::Config,
    device_metadata::get_device_metadata,
    route_cache::{read_cached_exit_route, write_cached_exit_route},
    vpn::smart_vpn_whitelist,
};

/// Gets a sillad Dialer that produces a single, pre-authentication pipe, as well as the public key.
pub async fn get_dialer(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<(VerifyingKey, ExitDescriptor, DynDialer)> {
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
        let dest_addr = *smol::net::resolve(dir)
            .await?
            .choose(&mut rand::thread_rng())
            .context("could not resolve destination for direct exit connection")?;
        smart_vpn_whitelist(ctx, dest_addr.ip());
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
            ConnTestDialer {
                ping_count: 1,
                inner: TcpDialer { dest_addr },
            }
            .dynamic(),
        ));
    }

    let res: anyhow::Result<ExitRouteDescriptor> = async {
        let (_level, conn_token, sig) = get_connect_token(ctx)
            .await
            .context("could not get connect token")?;

        let start = Instant::now();
        let metadata = match get_device_metadata(ctx).await {
            Ok(metadata) => {
                tracing::debug!(
                    metadata = debug(&metadata),
                    elapsed = debug(start.elapsed()),
                    "DEVICE METADATA OBTAINED"
                );
                serde_json::to_value(&metadata)?
            }
            Err(err) => {
                tracing::warn!(
                    err = debug(err),
                    "CANNOT GET DEVICE METADATA, PROCEEDING NONETHELESS"
                );
                serde_json::Value::Null
            }
        };

        tracing::debug!(token = %conn_token, "CONN TOKEN");
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

        if let Err(err) =
            write_cached_exit_route(ctx, &ctx.init().exit_constraint, &exit_route).await
        {
            tracing::warn!(err = debug(&err), "could not persist exit route cache");
        }

        Ok(exit_route)
    }
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
                    return Err(
                        err.context("fresh exit route unavailable and no cached route found")
                    );
                }
                Err(cache_err) => {
                    return Err(err.context(format!(
                        "fresh exit route unavailable and cached route lookup failed: {cache_err}"
                    )));
                }
            }
        }
    };

    dialer_from_exit_route(ctx, exit_route)
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

    smart_vpn_whitelist(ctx, exit.c2e_listen.ip());
    tracing::debug!(exit = ?exit, "exit route obtained: {}", serde_json::to_string(&route)?);

    let combined_routes = combine_exit_route(exit.clone(), route, ctx.init().allow_direct);
    let bridge_dialer = route_to_dialer(ctx, &combined_routes);

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
    use sillad_native_tls::TlsDialer;

    match route {
        RouteDescriptor::Tcp(addr) => {
            smart_vpn_whitelist(ctx, addr.ip());
            let addr = *addr;
            TcpDialer { dest_addr: addr }.dynamic()
        }
        RouteDescriptor::Sosistab3 { cookie, lower } => {
            let inner = route_to_dialer(ctx, lower);
            SosistabDialer {
                inner,
                cookie: Cookie::new(cookie),
            }
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
        } => route_to_dialer(ctx, lower)
            .timeout(Duration::from_millis(*milliseconds as _))
            .dynamic(),
        RouteDescriptor::Delay {
            milliseconds,
            lower,
        } => route_to_dialer(ctx, lower)
            .delay(Duration::from_millis((*milliseconds).into()))
            .dynamic(),
        RouteDescriptor::ConnTest { ping_count, lower } => {
            let lower = route_to_dialer(ctx, lower);
            ConnTestDialer {
                inner: lower,
                ping_count: *ping_count as _,
            }
            .dynamic()
        }
        RouteDescriptor::Hex { lower } => {
            let lower = route_to_dialer(ctx, lower);
            HexDialer { inner: lower }.dynamic()
        }
        RouteDescriptor::Other(_) => FailingDialer.dynamic(),
        RouteDescriptor::PlainTls { sni_domain, lower } => {
            let lower = route_to_dialer(ctx, lower);
            TlsDialer::new(
                lower,
                TlsConnector::new()
                    .use_sni(sni_domain.is_some())
                    .danger_accept_invalid_certs(true)
                    .danger_accept_invalid_hostnames(true)
                    .min_protocol_version(None)
                    .max_protocol_version(None),
                sni_domain
                    .clone()
                    .unwrap_or_else(|| "example.com".to_string()),
            )
            .dynamic()
        }
        RouteDescriptor::Meeklike { key, cfg, lower } => {
            let lower = route_to_dialer(ctx, lower);
            MeeklikeDialer {
                inner: lower.into(),
                cfg: *cfg,
                key: *blake3::hash(key.as_bytes()).as_bytes(),
            }
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
            exit_constraint: ExitConstraint::Auto,
            allow_direct: false,
            cache: None,
            broker: None,
            tunneled_broker: None,
            broker_keys: None,
            port_forward: vec![],
            vpn: false,
            vpn_fd: None,
            spoof_dns: false,
            passthrough_china: false,
            dry_run: true,
            credentials: Default::default(),
            sess_metadata: serde_json::Value::Null,
            task_limit: None,
        }
    }

    fn sample_exit() -> ExitDescriptor {
        ExitDescriptor {
            c2e_listen: "127.0.0.1:9000".parse().unwrap(),
            b2e_listen: "127.0.0.1:9001".parse().unwrap(),
            country: CountryCode::CAN,
            city: "Toronto".into(),
            load: 0.1,
            expiry: 1,
        }
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
}
