mod inner;
mod socks5;
use anyctx::AnyCtx;
use clone_macro::clone;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use smolscale::immortal::{Immortal, RespawnStrategy};

use crate::{broker::BrokerSource, exit::ExitConstraint};

use self::{inner::client_inner, socks5::socks5_loop};

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

type CtxField<T> = fn(&AnyCtx<Config>) -> T;

async fn client_main(ctx: AnyCtx<Config>) -> anyhow::Result<()> {
    let _client_loop = Immortal::respawn(
        RespawnStrategy::Immediate,
        clone!([ctx], move || client_inner(ctx.clone())),
    );
    socks5_loop(ctx).await
}
