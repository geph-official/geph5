use std::time::{Duration, Instant, SystemTime};

use anyctx::AnyCtx;
use anyhow::Context;

use arrayref::array_ref;
use async_native_tls::TlsConnector;
use ed25519_dalek::VerifyingKey;

use geph5_broker_protocol::{
    AccountLevel, ExitCategory, ExitDescriptor, GetRoutesArgs, NetStatus, RouteDescriptor,
};
use isocountry::CountryCode;
use itertools::Itertools;
use ordered_float::OrderedFloat;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use sillad::{
    dialer::{DialerExt, DynDialer, FailingDialer},
    tcp::TcpDialer,
};
use sillad_conntest::ConnTestDialer;
use sillad_hex::HexDialer;
use sillad_meeklike::MeeklikeDialer;
use sillad_sosistab3::{dialer::SosistabDialer, Cookie};

use crate::{
    auth::get_connect_token,
    broker::{broker_client, get_net_status},
    client::{Config, CtxField},
    device_metadata::get_device_metadata,
    vpn::smart_vpn_whitelist,
};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ExitConstraint {
    Auto,
    Direct(String),
    Hostname(String),
    Country(CountryCode),
    CountryCity(CountryCode, String),
}

/// Gets a sillad Dialer that produces a single, pre-authentication pipe, as well as the public key.
pub async fn get_dialer(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<(VerifyingKey, ExitDescriptor, DynDialer)> {
    static SEMAPH: CtxField<
        smol::lock::Mutex<Option<(VerifyingKey, ExitDescriptor, DynDialer, SystemTime)>>,
    > = |_| smol::lock::Mutex::new(None);
    let mut cached_value = ctx.get(SEMAPH).lock().await;

    if let Some(inner) = cached_value.clone() {
        if inner.3.elapsed()? < Duration::from_secs(120) {
            return Ok((inner.0, inner.1, inner.2));
        }
    }

    let res = get_dialer_inner(ctx).await;
    match res {
        Ok(val) => {
            *cached_value = Some((val.0, val.1.clone(), val.2.clone(), SystemTime::now()));
            Ok((val.0, val.1, val.2))
        }
        Err(err) => {
            tracing::warn!("failed to get dialer: {:?}", err);
            if let Some(val) = cached_value.clone() {
                tracing::warn!("returning stale value instead");
                Ok((val.0, val.1, val.2))
            } else {
                Err(err)
            }
        }
    }
}

async fn get_dialer_inner(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<(VerifyingKey, ExitDescriptor, DynDialer)> {
    // If the user specified a direct constraint, handle that path immediately:
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

    // Otherwise, we need to pick an exit from the broker based on user constraints.
    let (level, conn_token, sig) = get_connect_token(ctx)
        .await
        .context("could not get connect token")?;

    let net_status_verified = get_net_status(ctx).await?;

    tracing::debug!(
        "verified netstatus: {}",
        serde_json::to_string(
            &net_status_verified
                .exits
                .iter()
                .map(|s| &s.1 .1)
                .collect_vec()
        )?
    );

    // Use our new helper function to pick the best exit:
    let rendezvous_key = blake3::hash(serde_json::to_string(&ctx.init().credentials)?.as_bytes());
    let (pubkey, exit) = pick_exit_with_constraint(
        rendezvous_key,
        &ctx.init().exit_constraint,
        level,
        &net_status_verified,
    )?;

    tracing::debug!(exit = ?exit, "narrowed down choice of exit");
    smart_vpn_whitelist(ctx, exit.c2e_listen.ip());

    tracing::debug!(token = %conn_token, "CONN TOKEN");

    let start = Instant::now();
    let metadata = if let Ok(metadata) = get_device_metadata(ctx).await {
        tracing::info!(
            metadata = debug(&metadata),
            elapsed = debug(start.elapsed()),
            "DEVICE METADATA OBTAINED"
        );
        serde_json::to_value(&metadata)?
    } else {
        tracing::warn!("CANNOT GET DEVICE METADATA, PROCEEDING NONETHELESS");
        serde_json::Value::Null
    };

    // Also get potential “bridge routes”:
    let broker = broker_client(ctx)?;
    let bridge_routes = broker
        .get_routes_v2(GetRoutesArgs {
            token: conn_token,
            sig,
            exit_b2e: exit.b2e_listen,
            client_metadata: metadata,
        })
        .await?
        .map_err(|e| anyhow::anyhow!("broker refused to serve bridge routes: {e}"))?;
    tracing::debug!(
        "bridge routes obtained: {}",
        serde_json::to_string(&bridge_routes)?
    );

    let bridge_dialer = route_to_dialer(ctx, &bridge_routes);

    Ok((*pubkey, exit.clone(), bridge_dialer))
}

/// A helper that filters the verified exits by the user’s `ExitConstraint`,
/// then picks the exit with the lowest load.
fn pick_exit_with_constraint<'a>(
    rendezvous_key: blake3::Hash,
    constraint: &ExitConstraint,
    level: AccountLevel,
    net_status: &'a NetStatus,
) -> anyhow::Result<(&'a VerifyingKey, &'a ExitDescriptor)> {
    let all_exits = net_status.exits.values();

    // Figure out which fields we need to match
    let mut country_constraint = None;
    let mut city_constraint = None;
    let mut hostname_constraint = None;

    match constraint {
        ExitConstraint::Hostname(host) => hostname_constraint = Some(host.clone()),
        ExitConstraint::Country(country) => country_constraint = Some(*country),
        ExitConstraint::CountryCity(country, city) => {
            country_constraint = Some(*country);
            city_constraint = Some(city.clone());
        }
        ExitConstraint::Auto => {}
        ExitConstraint::Direct(_) => panic!("should not reach here"),
    }

    let filtered: Vec<_> = all_exits
        .filter(|(_, exit, meta)| {
            let mut pass = match country_constraint {
                Some(c) => exit.country == c,
                None => true,
            };
            pass &= match &city_constraint {
                Some(city) => exit.city == *city,
                None => true,
            };
            pass &= match &hostname_constraint {
                Some(hn) => exit.b2e_listen.ip().to_string() == *hn,
                None => true,
            };
            if matches!(constraint, ExitConstraint::Auto) {
                pass &= meta.category == ExitCategory::Core;
            }
            pass &= meta.allowed_levels.contains(&level);
            pass
        })
        .collect();

    if filtered.is_empty() {
        anyhow::bail!("no exits match the constraints")
    }

    // If any matched, we use load-sensitive rendezvous hashing
    let first = filtered
        .iter()
        .min_by_key(|rh| {
            let (_, exit, _) = **rh;
            let hash = blake3::keyed_hash(
                rendezvous_key.as_bytes(),
                exit.b2e_listen.ip().to_string().as_bytes(),
            );
            let hash = &hash.as_bytes()[..];
            let hash = u64::from_be_bytes(*array_ref![hash, 0, 8]) as f64 / u64::MAX as f64;
            let weight = (1.0 - (exit.load as f64)).powi(2);
            let picker = -hash.ln() / weight;
            OrderedFloat(picker)
        })
        .unwrap();
    Ok((&first.0, &first.1))
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
        RouteDescriptor::Meeklike { key, lower } => {
            let lower = route_to_dialer(ctx, lower);
            MeeklikeDialer {
                inner: lower.into(),
                key: *blake3::hash(key.as_bytes()).as_bytes(),
            }
            .dynamic()
        }
    }
}
