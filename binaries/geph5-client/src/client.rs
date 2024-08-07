use anyctx::AnyCtx;

use anyhow::Context;
use clone_macro::clone;
use futures_util::{future::Shared, task::noop_waker, FutureExt, TryFutureExt};
use geph5_broker_protocol::{Credential, ExitList, UserInfo};
use nanorpc::DynRpcTransport;
use smol::future::FutureExt as _;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use smolscale::immortal::{Immortal, RespawnStrategy};

use crate::{
    auth::{auth_loop, get_auth_token},
    broker::{broker_client, BrokerSource},
    client_inner::client_once,
    control_prot::{
        ControlClient, ControlProtocolImpl, ControlService, DummyControlProtocolTransport,
    },
    database::db_read_or_wait,
    http_proxy::run_http_proxy,
    route::ExitConstraint,
    socks5::socks5_loop,
    vpn::vpn_loop,
};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub socks5_listen: Option<SocketAddr>,
    pub http_proxy_listen: Option<SocketAddr>,
    pub stats_listen: Option<SocketAddr>,
    pub control_listen: Option<SocketAddr>,
    pub exit_constraint: ExitConstraint,
    #[serde(default)]
    pub bridge_mode: BridgeMode,
    pub cache: Option<PathBuf>,
    pub broker: Option<BrokerSource>,

    #[serde(default)]
    pub vpn: bool,
    #[serde(default = "true_bool")]
    pub disable_ipv6: bool,
    #[serde(default)]
    pub spoof_dns: bool,
    #[serde(default)]
    pub passthrough_china: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub credentials: Credential,
}

// FIXME: Used to set serde bool fields to a true default.
// There might be a cleaner way to do this.
fn true_bool() -> bool {
    true
}

impl Config {
    /// Create an "inert" version of this config that does not start any processes.
    pub fn inert(&self) -> Self {
        let mut this = self.clone();
        this.dry_run = true;
        this.socks5_listen = None;
        this.http_proxy_listen = None;
        this.stats_listen = None;
        this.control_listen = None;
        this
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BridgeMode {
    Auto,
    ForceBridges,
    ForceDirect,
}

impl Default for BridgeMode {
    fn default() -> Self {
        Self::Auto
    }
}

pub struct Client {
    task: Shared<smol::Task<Result<(), Arc<anyhow::Error>>>>,
    ctx: AnyCtx<Config>,
}

impl Client {
    /// Starts the client logic in the loop, returning the handle.
    pub fn start(cfg: Config) -> Self {
        std::env::remove_var("http_proxy");
        std::env::remove_var("https_proxy");
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("HTTPS_PROXY");
        let ctx = AnyCtx::new(cfg);
        let task = smolscale::spawn(client_main(ctx.clone()).map_err(Arc::new));
        Client {
            task: task.shared(),
            ctx,
        }
    }

    /// Wait until there's an error.
    pub async fn wait_until_dead(self) -> anyhow::Result<()> {
        self.task.await.map_err(|e| anyhow::anyhow!(e))
    }

    /// Check for an error.
    pub fn check_dead(&self) -> anyhow::Result<()> {
        match self
            .task
            .clone()
            .poll(&mut std::task::Context::from_waker(&noop_waker()))
        {
            std::task::Poll::Ready(val) => val.map_err(|e| anyhow::anyhow!(e))?,
            std::task::Poll::Pending => {}
        }

        Ok(())
    }

    /// Get the control protocol client.
    pub fn control_client(&self) -> ControlClient {
        ControlClient(DynRpcTransport::new(DummyControlProtocolTransport(
            ControlService(ControlProtocolImpl {
                ctx: self.ctx.clone(),
            }),
        )))
    }

    /// Gets the user info.
    pub async fn user_info(&self) -> anyhow::Result<UserInfo> {
        let auth_token = get_auth_token(&self.ctx).await?;
        let user_info = broker_client(&self.ctx)?
            .get_user_info(auth_token)
            .await??
            .context("no such user")?;
        Ok(user_info)
    }
}

pub type CtxField<T> = fn(&AnyCtx<Config>) -> T;

async fn client_main(ctx: AnyCtx<Config>) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct DryRunOutput {
        auth_token: String,
        exits: ExitList,
    }

    tracing::info!("loaded config: {}", serde_yaml::to_string(ctx.init())?);

    if ctx.init().dry_run {
        auth_loop(&ctx)
            .race(async {
                let broker_client = broker_client(&ctx)?;
                let exits = broker_client
                    .get_exits()
                    .await?
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let auth_token = db_read_or_wait(&ctx, "auth_token").await?;
                let exits = exits.inner;
                println!(
                    "{}",
                    serde_json::to_string(&DryRunOutput {
                        auth_token: hex::encode(auth_token),
                        exits,
                    })?
                );
                anyhow::Ok(())
            })
            .await
    } else {
        // Disable IPv6 if needed
        if ctx.init().disable_ipv6 {
            crate::ipv6::disable_ipv6()?;
        }

        let vpn_loop = vpn_loop(&ctx);

        let _client_loop = Immortal::respawn(
            RespawnStrategy::JitterDelay(Duration::from_secs(1), Duration::from_secs(5)),
            clone!([ctx], move || client_once(ctx.clone()).inspect_err(
                |e| tracing::warn!("client died and restarted: {:?}", e)
            )),
        );

        let rpc_serve = async {
            if let Some(control_listen) = ctx.init().control_listen {
                nanorpc_sillad::rpc_serve(
                    sillad::tcp::TcpListener::bind(control_listen).await?,
                    ControlService(ControlProtocolImpl { ctx: ctx.clone() }),
                )
                .await?;
                anyhow::Ok(())
            } else {
                smol::future::pending().await
            }
        };

        let result = socks5_loop(&ctx)
            .inspect_err(|e| tracing::error!(err = debug(e), "socks5 loop stopped"))
            .race(vpn_loop.inspect_err(|e| tracing::error!(err = debug(e), "vpn loop stopped")))
            .race(
                run_http_proxy(&ctx)
                    .inspect_err(|e| tracing::error!(err = debug(e), "http proxy stopped")),
            )
            .race(
                auth_loop(&ctx)
                    .inspect_err(|e| tracing::error!(err = debug(e), "auth loop stopped")),
            )
            .race(rpc_serve)
            .await;

        // FIXME: Doesn't work
        // Enable IPv6 again if it was disabled at the start
        if ctx.init().disable_ipv6 {
            crate::ipv6::enable_ipv6()?;
        }

        result
    }
}
