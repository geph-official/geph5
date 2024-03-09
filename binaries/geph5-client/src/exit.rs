use std::time::Duration;

use anyctx::AnyCtx;
use anyhow::Context;

use ed25519_dalek::VerifyingKey;
use geph5_broker_protocol::{BrokerClient, RouteDescriptor, DOMAIN_EXIT_DESCRIPTOR};
use isocountry::CountryCode;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use sillad::{
    dialer::{DialerExt, DynDialer, FailingDialer},
    tcp::TcpDialer,
};
use sillad_sosistab3::{dialer::SosistabDialer, Cookie};

use crate::client::{Config, CtxField};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ExitConstraint {
    Auto,
    Direct(String),
    Country(CountryCode),
    CountryCity(CountryCode, String),
}

static BROKER_CLIENT: CtxField<Option<BrokerClient>> = |ctx| {
    ctx.init()
        .broker
        .as_ref()
        .map(|src| BrokerClient::from(src.rpc_transport()))
};

fn broker_client(ctx: &AnyCtx<Config>) -> anyhow::Result<&BrokerClient> {
    ctx.get(BROKER_CLIENT).as_ref().context(
        "broker information not provided, so cannot use any broker-dependent functionality",
    )
}

impl ExitConstraint {
    /// Turn this into a sillad Dialer that produces a single, pre-authentication pipe, as well as the public key.
    #[tracing::instrument(skip(ctx))]
    pub async fn dialer(&self, ctx: &AnyCtx<Config>) -> anyhow::Result<(VerifyingKey, DynDialer)> {
        let mut country_constraint = None;
        let mut city_constraint = None;
        match self {
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
                return Ok((
                    pubkey,
                    TcpDialer {
                        dest_addr: *smol::net::resolve(dir)
                            .await?
                            .choose(&mut rand::thread_rng())
                            .context("could not resolve destination for direct exit connection")?,
                    }
                    .dynamic(),
                ));
            }
            ExitConstraint::Country(country) => country_constraint = Some(*country),
            ExitConstraint::CountryCity(country, city) => {
                country_constraint = Some(*country);
                city_constraint = Some(city.clone())
            }
            ExitConstraint::Auto => {}
        }
        tracing::debug!(
            country_constraint = debug(country_constraint),
            city_constraint = debug(&city_constraint),
            "created dialer"
        );

        let broker = broker_client(ctx)?;
        let exits = broker
            .get_exits()
            .await?
            .map_err(|e| anyhow::anyhow!("broker refused to serve exits: {e}"))?;
        // TODO we need to ACTUALLY verify the response!!!
        let exits = exits.verify(DOMAIN_EXIT_DESCRIPTOR, |_| true)?;
        // filter for things that fit
        let (pubkey, exit) = exits
            .all_exits
            .into_iter()
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
                country_pass && city_pass
            })
            .min_by_key(|e| (e.1.load * 1000.0) as u64)
            .context("no exits that fit the criterion")?;
        tracing::debug!(exit = debug(&exit), "narrowed down choice of exit");
        let direct_dialer = TcpDialer {
            dest_addr: exit.c2e_listen,
        };

        // Also obtain the bridges
        let bridge_routes = broker
            .get_routes(exit.b2e_listen)
            .await?
            .map_err(|e| anyhow::anyhow!("broker refused to serve bridge routes: {e}"))?;
        tracing::debug!(
            bridge_routes = debug(&bridge_routes),
            "bridge routes obtained too"
        );

        let bridge_dialer = route_to_dialer(&bridge_routes);

        Ok((
            pubkey,
            direct_dialer
                .race(bridge_dialer.delay(Duration::from_millis(200)))
                .dynamic(),
        ))
    }
}

fn route_to_dialer(route: &RouteDescriptor) -> DynDialer {
    match route {
        RouteDescriptor::Tcp(addr) => TcpDialer { dest_addr: *addr }.dynamic(),
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
    }
}
