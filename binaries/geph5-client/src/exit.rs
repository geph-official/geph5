use anyhow::Context;
use isocountry::CountryCode;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use sillad::{
    dialer::{DialerExt, DynDialer},
    tcp::TcpDialer,
};

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ExitConstraint {
    Auto,
    Direct(String),
    Country(CountryCode),
    CountryCity(CountryCode, String),
}

impl ExitConstraint {
    /// Turn this into a sillad Dialer that produces a single, pre-authentication pipe.
    pub async fn dialer(&self) -> anyhow::Result<DynDialer> {
        match self {
            ExitConstraint::Direct(dir) => Ok(TcpDialer {
                dest_addr: *smol::net::resolve(dir)
                    .await?
                    .choose(&mut rand::thread_rng())
                    .context("could not resolve destination for direct exit connection")?,
            }
            .dynamic()),
            ExitConstraint::Country(_) => todo!(),
            ExitConstraint::CountryCity(_, _) => todo!(),
            ExitConstraint::Auto => todo!(),
        }
    }
}
