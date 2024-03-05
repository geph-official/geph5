use anyctx::AnyCtx;
use anyhow::Context;
use async_trait::async_trait;
use ed25519_dalek::VerifyingKey;
use geph5_broker_protocol::{BrokerClient, DOMAIN_EXIT_DESCRIPTOR};
use isocountry::CountryCode;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use sillad::{
    dialer::{Dialer, DialerExt, DynDialer},
    tcp::TcpDialer,
};

use crate::{
    broker::BrokerRpcTransport,
    client::{Config, CtxField},
};

#[derive(Serialize, Deserialize, Clone)]
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
        .map(|ctx| BrokerRpcTransport::new(ctx.clone()).into())
};

fn broker_client(ctx: &AnyCtx<Config>) -> anyhow::Result<&BrokerClient> {
    ctx.get(BROKER_CLIENT).as_ref().context(
        "broker information not provided, so cannot use any broker-dependent functionality",
    )
}

impl ExitConstraint {
    /// Turn this into a sillad Dialer that produces a single, pre-authentication pipe, as well as the public key.
    pub async fn dialer(&self, ctx: &AnyCtx<Config>) -> anyhow::Result<DynDialer> {
        let mut country_constraint = None;
        let mut city_constraint = None;
        match self {
            ExitConstraint::Direct(dir) => {
                let (dir, pubkey) = dir
                    .split_once('/')
                    .context("did not find / in a direct constraint")?;
                let pubkey = VerifyingKey::from_bytes(hex::);
                return Ok(TcpDialer {
                    dest_addr: *smol::net::resolve(dir)
                        .await?
                        .choose(&mut rand::thread_rng())
                        .context("could not resolve destination for direct exit connection")?,
                }
                .dynamic());
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
        todo!()
    }
}
