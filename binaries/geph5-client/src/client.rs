mod inner;
mod socks5;
use anyctx::AnyCtx;
use clone_macro::clone;
use futures_util::TryFutureExt;
use std::{net::SocketAddr, time::Duration};

use serde::{Deserialize, Serialize};
use smolscale::immortal::{Immortal, RespawnStrategy};

use crate::{broker::BrokerSource, exit::ExitConstraint};

use self::{inner::client_once, socks5::socks5_loop};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub socks5_listen: SocketAddr,
    pub exit_constraint: ExitConstraint,
    pub broker: Option<BrokerSource>,
}

pub struct Client {
    task: smol::Task<anyhow::Result<()>>,
}

impl Client {
    /// Starts the client logic in the loop, returnign the handle.
    pub fn start(cfg: Config) -> Self {
        let ctx = AnyCtx::new(cfg);
        let task = smolscale::spawn(client_main(ctx));
        Client { task }
    }

    /// Wait until there's an error.
    pub async fn wait_until_dead(self) -> anyhow::Result<()> {
        self.task.await
    }
}

pub type CtxField<T> = fn(&AnyCtx<Config>) -> T;

async fn client_main(ctx: AnyCtx<Config>) -> anyhow::Result<()> {
    let _client_loops: Vec<_> = (0..6)
        .map(|_| {
            Immortal::respawn(
                RespawnStrategy::JitterDelay(Duration::from_secs(1), Duration::from_secs(5)),
                clone!([ctx], move || client_once(ctx.clone())
                    .inspect_err(|e| tracing::warn!("client_inner died: {:?}", e))),
            )
        })
        .collect();
    socks5_loop(ctx).await
}
