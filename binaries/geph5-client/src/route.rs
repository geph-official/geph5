use std::{net::SocketAddr, time::Duration};

use anyctx::AnyCtx;
use anyhow::Context;

use ed25519_dalek::VerifyingKey;
use futures_util::TryFutureExt as _;
use geph5_broker_protocol::{
    AccountLevel, ExitDescriptor, RouteDescriptor, DOMAIN_EXIT_DESCRIPTOR,
};
use isocountry::CountryCode;
use moka::sync::Cache;
use once_cell::sync::Lazy;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use sillad::{
    dialer::{DialerExt, DynDialer, FailingDialer},
    tcp::TcpDialer,
};
use sillad_conntest::ConnTestDialer;
use sillad_sosistab3::{dialer::SosistabDialer, Cookie};

use crate::{
    auth::get_connect_token, broker::broker_client, client::Config, client_inner::CONCURRENCY,
    vpn::vpn_whitelist,
};

static ROUTE_SHITLIST: Lazy<Cache<SocketAddr, usize>> = Lazy::new(|| {
    Cache::builder()
        .time_to_live(Duration::from_secs(60))
        .build()
});

fn shitlist_delay(addr: SocketAddr) -> Duration {
    let recent_deaths = ROUTE_SHITLIST.get(&addr).unwrap_or_default() as f64 / CONCURRENCY as f64;
    Duration::from_secs_f64((recent_deaths - 1.0).max(0.0).powi(2) / 10.0)
}

/// Deprioritizes routes with this address.
pub fn deprioritize_route(addr: SocketAddr) {
    ROUTE_SHITLIST.insert(addr, ROUTE_SHITLIST.get_with(addr, || 1) + 1)
}

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
    let mut country_constraint = None;
    let mut city_constraint = None;
    let mut hostname_constraint = None;
    match &ctx.init().exit_constraint {
        ExitConstraint::Direct(dir) => {
            let (dir, pubkey) = dir
                .split_once('/')
                .context("did not find / in a direct constraint")?;
            let pubkey = VerifyingKey::from_bytes(
                hex::decode(pubkey)
                    .context("cannot decode pubkey as hex")?
                    .as_slice()
                    .try_into()
                    .context("pubkey wrong length")?,
            )?;
            let dest_addr = *smol::net::resolve(dir)
                .await?
                .choose(&mut rand::thread_rng())
                .context("could not resolve destination for direct exit connection")?;
            vpn_whitelist(dest_addr.ip());
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
                TcpDialer { dest_addr }.dynamic(),
            ));
        }
        ExitConstraint::Country(country) => country_constraint = Some(*country),
        ExitConstraint::CountryCity(country, city) => {
            country_constraint = Some(*country);
            city_constraint = Some(city.clone())
        }
        ExitConstraint::Hostname(hostname) => {
            hostname_constraint = Some(hostname.clone());
        }
        ExitConstraint::Auto => {}
    }

    // First get the conn token
    let (level, conn_token, sig) = get_connect_token(ctx)
        .await
        .context("could not get connect token")?;

    let broker = broker_client(ctx).context("could not get broker client")?;
    let exits = match level {
        AccountLevel::Plus => broker.get_exits().await,
        AccountLevel::Free => broker.get_free_exits().await,
    }?
    .map_err(|e| anyhow::anyhow!("broker refused to serve exits: {e}"))?;

    let exits = exits
        .verify(DOMAIN_EXIT_DESCRIPTOR, |their_pk| {
            if let Some(broker_pk) = &ctx.init().broker_keys {
                hex::encode(their_pk.as_bytes()) == broker_pk.master
            } else {
                true
            }
        })
        .context("could not verify")?;
    // filter for things that fit
    let (pubkey, exit) = if let Some(min) = exits
        .all_exits
        .iter()
        .filter(|(_, exit)| {
            let country_pass = if let Some(country) = &country_constraint {
                exit.country == *country
            } else {
                true
            };
            let city_pass = if let Some(city) = &city_constraint {
                &exit.city == city
            } else {
                true
            };
            let hostname_pass = if let Some(hostname) = &hostname_constraint {
                &exit.b2e_listen.ip().to_string() == hostname
            } else {
                true
            };
            country_pass && city_pass && hostname_pass
        })
        .min_by_key(|e| (e.1.load * 1000.0) as u64)
    {
        min
    } else {
        exits
            .all_exits
            .iter()
            .min_by_key(|e| (e.1.load * 1000.0) as u64)
            .context("no exits that fit the criterion")?
    };

    tracing::debug!(exit = debug(&exit), "narrowed down choice of exit");
    vpn_whitelist(exit.c2e_listen.ip());

    let exit_c2e = exit.c2e_listen;
    let direct_dialer = TcpDialer {
        dest_addr: exit_c2e,
    }
    .dyn_delay(move || shitlist_delay(exit_c2e));

    tracing::debug!(token = debug(&conn_token), sig = debug(&sig), "CONN TOKEN");

    // also get bridges
    let bridge_routes = broker
        .get_routes(conn_token, sig, exit.b2e_listen)
        .await?
        .map_err(|e| anyhow::anyhow!("broker refused to serve bridge routes: {e}"))?;
    tracing::debug!(
        bridge_routes = debug(&bridge_routes),
        "bridge routes obtained too"
    );

    let bridge_dialer = route_to_dialer(&bridge_routes);

    let final_dialer = match ctx.init().bridge_mode {
        crate::BridgeMode::Auto => direct_dialer
            .race(bridge_dialer.delay(Duration::from_millis(1000)))
            .dynamic(),
        crate::BridgeMode::ForceBridges => bridge_dialer,
        crate::BridgeMode::ForceDirect => direct_dialer.dynamic(),
    };

    Ok((*pubkey, exit.clone(), final_dialer))
}

// async fn reachability_test(
//     ctx: AnyCtx<Config>,
//     dialers: BTreeMap<String, DynDialer>,
// ) -> anyhow::Result<()> {
//     let nfo = IP_INFO.get().unwrap();
//     let country = nfo["country"].as_str().context("country code not found")?;
//     let asn = nfo["org"]
//         .as_str()
//         .context("no org")?
//         .split_ascii_whitespace()
//         .next()
//         .unwrap();

//     for (name, dialer) in dialers {
//         tracing::debug!(name = display(&name), "doing a reachability test");
//         let ctx = ctx.clone();
//         smolscale::spawn(async move {
//             let broker = broker_client(&ctx).context("could not get broker client")?;
//             let success = if let Err(err) = dialer.timeout(Duration::from_secs(10)).dial().await {
//                 tracing::warn!(
//                     name = display(&name),
//                     err = debug(err),
//                     "could not reach something"
//                 );
//                 false
//             } else {
//                 true
//             };
//             broker
//                 .upload_available(AvailabilityData {
//                     listen: name,
//                     country: country.to_string(),
//                     asn: asn.to_string(),
//                     success,
//                 })
//                 .await?;
//             anyhow::Ok(())
//         })
//         .detach();
//     }

//     Ok(())
// }

// fn route_to_flat_dialers(route: &RouteDescriptor) -> BTreeMap<String, DynDialer> {
//     match route {
//         RouteDescriptor::Tcp(socket_addr) => std::iter::once((
//             socket_addr.ip().to_string(),
//             TcpDialer {
//                 dest_addr: *socket_addr,
//             }
//             .dynamic(),
//         ))
//         .collect(),
//         RouteDescriptor::Sosistab3 { cookie, lower } => route_to_flat_dialers(lower)
//             .into_iter()
//             .map(|(k, inner)| {
//                 (
//                     k,
//                     SosistabDialer {
//                         inner,
//                         cookie: Cookie::new(cookie),
//                     }
//                     .dynamic(),
//                 )
//             })
//             .collect(),
//         RouteDescriptor::Race(vec) | RouteDescriptor::Fallback(vec) => vec
//             .iter()
//             .flat_map(|v| route_to_flat_dialers(v).into_iter())
//             .collect(),
//         RouteDescriptor::Timeout {
//             milliseconds: _,
//             lower,
//         }
//         | RouteDescriptor::Delay {
//             milliseconds: _,
//             lower,
//         } => route_to_flat_dialers(lower),

//         _ => BTreeMap::new(),
//     }
// }

fn route_to_dialer(route: &RouteDescriptor) -> DynDialer {
    match route {
        RouteDescriptor::Tcp(addr) => {
            vpn_whitelist(addr.ip());
            let addr = *addr;
            TcpDialer { dest_addr: addr }
                .dyn_delay(move || shitlist_delay(addr))
                .dynamic()
        }
        RouteDescriptor::Sosistab3 { cookie, lower } => {
            let inner = route_to_dialer(lower);
            SosistabDialer {
                inner,
                cookie: Cookie::new(cookie),
            }
            .dynamic()
        }
        RouteDescriptor::Race(inside) => inside
            .iter()
            .map(route_to_dialer)
            .reduce(|a, b| a.race(b).dynamic())
            .unwrap_or_else(|| FailingDialer.dynamic()),
        RouteDescriptor::Fallback(a) => a
            .iter()
            .map(route_to_dialer)
            .reduce(|a, b| a.fallback(b).dynamic())
            .unwrap_or_else(|| FailingDialer.dynamic()),
        RouteDescriptor::Timeout {
            milliseconds,
            lower,
        } => route_to_dialer(lower)
            .timeout(Duration::from_millis(*milliseconds as _))
            .dynamic(),
        RouteDescriptor::Delay {
            milliseconds,
            lower,
        } => route_to_dialer(lower)
            .delay(Duration::from_millis((*milliseconds).into()))
            .dynamic(),
        RouteDescriptor::ConnTest { ping_count, lower } => {
            let lower = route_to_dialer(lower);
            ConnTestDialer {
                inner: lower,
                ping_count: *ping_count as _,
            }
            .dynamic()
        }

        RouteDescriptor::Other(_) => FailingDialer.dynamic(),
    }
}
