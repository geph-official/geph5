mod inner;

use std::{
    any::Any,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use anyctx::AnyCtx;

use futures_util::AsyncReadExt as _;
use geph5_misc_rpc::{
    exit::{ClientCryptHello, ClientExitCryptPipe, ClientHello, ExitHello, ExitHelloInner},
    read_prepend_length, write_prepend_length,
};
use picomux::PicoMux;
use serde::{Deserialize, Serialize};
use sillad::{
    dialer::{Dialer, DynDialer},
    listener::Listener,
    Pipe,
};
use smol::future::FutureExt as _;
use socksv5::v5::{
    read_handshake, read_request, write_auth_method, write_request_status, SocksV5AuthMethod,
    SocksV5Host, SocksV5RequestStatus,
};
use stdcode::StdcodeSerializeExt;

use crate::{broker::BrokerSource, exit::ExitConstraint};

use self::inner::client_inner;

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
    loop {
        if let Err(err) = client_inner(ctx.clone()).await {
            tracing::warn!("restarting everything due to {:?}", err);
        }
    }
}
