use anyctx::AnyCtx;
use clone_macro::clone;
use futures_util::{
    future::Shared,
    io::{BufReader, BufWriter},
    task::noop_waker,
    AsyncReadExt, AsyncWriteExt, FutureExt, TryFutureExt,
};
use geph5_broker_protocol::{Credential, ExitList};
use smol::future::FutureExt as _;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    task::Context,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use smolscale::immortal::{Immortal, RespawnStrategy};

use crate::{
    auth::auth_loop,
    broker::{broker_client, BrokerSource},
    client_inner::{client_once, open_conn},
    database::db_read_or_wait,
    http_proxy::run_http_proxy,
    route::ExitConstraint,
    socks5::socks5_loop,
    stats::stat_get_num,
    vpn::VpnCapture,
};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub socks5_listen: Option<SocketAddr>,
    pub http_proxy_listen: Option<SocketAddr>,
    pub stats_listen: Option<SocketAddr>,
    pub exit_constraint: ExitConstraint,
    pub cache: Option<PathBuf>,
    pub broker: Option<BrokerSource>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub credentials: Credential,
}

pub struct Client {
    task: Shared<smol::Task<Result<(), Arc<anyhow::Error>>>>,
    ctx: AnyCtx<Config>,
    start_time: Instant,
}

impl Client {
    /// Starts the client logic in the loop, returnign the handle.
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
            start_time: Instant::now(),
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
            .poll(&mut Context::from_waker(&noop_waker()))
        {
            std::task::Poll::Ready(val) => val.map_err(|e| anyhow::anyhow!(e))?,
            std::task::Poll::Pending => {}
        }

        Ok(())
    }

    /// Gets the starting time.
    pub fn start_time(&self) -> Instant {
        self.start_time
    }

    /// Returns the count of all bytes used.
    pub fn bytes_used(&self) -> f64 {
        stat_get_num(&self.ctx, "total_bytes")
    }
}

pub type CtxField<T> = fn(&AnyCtx<Config>) -> T;

async fn client_main(ctx: AnyCtx<Config>) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct DryRunOutput {
        auth_token: String,
        exits: ExitList,
    }

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
        let vpn = VpnCapture::new();

        let vpn_loop = async {
            loop {
                let captured = vpn.ipstack().accept().await?;
                match captured {
                    ipstack_geph::stream::IpStackStream::Tcp(captured) => {
                        let peer_addr = captured.peer_addr();
                        tracing::warn!(
                            local_addr = display(captured.local_addr()),
                            peer_addr = display(peer_addr),
                            "captured a TCP"
                        );
                        let ctx = ctx.clone();

                        smolscale::spawn(async move {
                            let tunneled = open_conn(&ctx, &peer_addr.to_string()).await?;
                            tracing::debug!(peer_addr = display(peer_addr), "dialed through VPN");
                            let (read_tunneled, write_tunneled) = tunneled.split();
                            let (read_captured, write_captured) = captured.split();
                            smol::io::copy(read_tunneled, write_captured)
                                .race(smol::io::copy(read_captured, write_tunneled))
                                .await?;
                            anyhow::Ok(())
                        })
                        .detach();
                    }
                    ipstack_geph::stream::IpStackStream::Udp(captured) => {
                        let peer_addr = captured.peer_addr();
                        tracing::warn!(
                            local_addr = display(captured.local_addr()),
                            peer_addr = display(peer_addr),
                            "captured a UDP"
                        );
                        let ctx = ctx.clone();
                        smolscale::spawn::<anyhow::Result<()>>(async move {
                            let tunneled = open_conn(&ctx, &format!("udp-{peer_addr}")).await?;
                            let (read_tunneled, write_tunneled) = tunneled.split();
                            let up_loop = async {
                                let mut write_tunneled = BufWriter::new(write_tunneled);
                                loop {
                                    let to_up = captured.recv().await?;
                                    write_tunneled
                                        .write_all(&(to_up.len() as u16).to_le_bytes())
                                        .await?;
                                    write_tunneled.write_all(&to_up).await?;
                                    write_tunneled.flush().await?;
                                }
                            };
                            let dn_loop = async {
                                let mut read_tunneled = BufReader::new(read_tunneled);
                                loop {
                                    let mut len_buf = [0u8; 2];
                                    read_tunneled.read_exact(&mut len_buf).await?;
                                    let len = u16::from_le_bytes(len_buf) as usize;
                                    let mut buf = vec![0u8; len];
                                    read_tunneled.read_exact(&mut buf).await?;
                                    captured.send(&buf).await?;
                                }
                            };
                            up_loop.race(dn_loop).await
                        })
                        .detach();
                    }
                    ipstack_geph::stream::IpStackStream::UnknownTransport(_) => {
                        tracing::warn!("captured an UnknownTransport")
                    }
                    ipstack_geph::stream::IpStackStream::UnknownNetwork(_) => {
                        tracing::warn!("captured an UnknownNetwork")
                    }
                }
            }
        };

        let _client_loop = Immortal::respawn(
            RespawnStrategy::JitterDelay(Duration::from_secs(1), Duration::from_secs(5)),
            clone!([ctx], move || client_once(ctx.clone()).inspect_err(
                |e| tracing::warn!("client died and restarted: {:?}", e)
            )),
        );
        socks5_loop(&ctx)
            .race(vpn_loop)
            .race(run_http_proxy(&ctx))
            .race(auth_loop(&ctx))
            .await
    }
}
