use anyctx::AnyCtx;

use anyhow::Context;
use bytes::Bytes;
use futures_util::{
    future::Shared, task::noop_waker, AsyncReadExt, AsyncWriteExt, FutureExt, TryFutureExt,
};
use geph5_broker_protocol::{Credential, ExitList, UserInfo};
use nanorpc::DynRpcTransport;
use sillad::Pipe;
use smol::future::FutureExt as _;
use std::{fs::File, net::SocketAddr, path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use smolscale::immortal::Immortal;

use crate::{
    auth::{auth_loop, get_auth_token},
    broker::{broker_client, BrokerSource},
    client_inner::{client_inner, open_conn},
    control_prot::{
        ControlClient, ControlProtocolImpl, ControlService, DummyControlProtocolTransport,
    },
    http_proxy::http_proxy_serve,
    pac::pac_serve,
    route::ExitConstraint,
    socks5::socks5_loop,
    vpn::{recv_vpn_packet, send_vpn_packet, vpn_loop},
};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub socks5_listen: Option<SocketAddr>,
    pub http_proxy_listen: Option<SocketAddr>,
    pub pac_listen: Option<SocketAddr>,

    pub control_listen: Option<SocketAddr>,
    pub exit_constraint: ExitConstraint,
    #[serde(default)]
    pub bridge_mode: BridgeMode,
    pub cache: Option<PathBuf>,

    pub broker: Option<BrokerSource>,
    pub broker_keys: Option<BrokerKeys>,

    #[serde(default)]
    pub vpn: bool,
    #[serde(default)]
    pub vpn_fd: Option<i32>,
    #[serde(default)]
    pub spoof_dns: bool,
    #[serde(default)]
    pub passthrough_china: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub credentials: Credential,

    #[serde(default)]
    pub sess_metadata: serde_json::Value,
    pub task_limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone)]
/// Broker keys, in hexadecimal format.
pub struct BrokerKeys {
    pub master: String,
    pub mizaru_free: String,
    pub mizaru_plus: String,
}

impl Config {
    /// Create an "inert" version of this config that does not start any processes.
    pub fn inert(&self) -> Self {
        let mut this = self.clone();
        this.dry_run = true;
        this.socks5_listen = None;
        this.http_proxy_listen = None;
        this.pac_listen = None;
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
        let ctx = AnyCtx::new(cfg.clone());

        #[cfg(unix)]
        if let Some(fd) = cfg.vpn_fd {
            let ctx_clone = ctx.clone();
            smolscale::spawn(async move {
                // Create an async file descriptor from the raw fd
                let async_fd: smol::Async<File> =
                    smol::Async::new(unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) })
                        .expect("could not wrap VPN fd in Async");

                // Split the file descriptor for reading and writingz
                let (mut reader, mut writer) = async_fd.split();

                // Spawn a task for reading from fd and sending to VPN
                let read_task = async {
                    let mut buf = vec![0u8; 65535]; // Buffer for reading packets
                    loop {
                        match reader.read(&mut buf).await {
                            Ok(n) if n > 0 => {
                                // Send the packet to the VPN
                                send_vpn_packet(
                                    &ctx_clone,
                                    bytes::Bytes::copy_from_slice(&buf[..n]),
                                )
                                .await;
                            }
                            Ok(0) => {
                                // EOF
                                tracing::warn!("VPN fd reached EOF");
                                break;
                            }
                            Err(e) => {
                                tracing::error!("Error reading from VPN fd: {}", e);
                                break;
                            }
                            _ => break,
                        }
                    }
                    anyhow::Ok(())
                };

                // Spawn a task for receiving from VPN and writing to fd
                let write_task = async {
                    loop {
                        // Receive a packet from the VPN
                        let packet = recv_vpn_packet(&ctx_clone).await;

                        // Write the packet to the file descriptor
                        if let Err(e) = writer.write_all(&packet).await {
                            tracing::error!("Error writing to VPN fd: {}", e);
                            break;
                        }

                        if let Err(e) = writer.flush().await {
                            tracing::error!("Error flushing VPN fd: {}", e);
                            break;
                        }
                    }
                    anyhow::Ok(())
                };

                // Wait for either task to complete (or fail)
                let _ = read_task.race(write_task).await;
                tracing::warn!("VPN fd handler exited");
            })
            .detach();
        }
        let task = smolscale::spawn(client_main(ctx.clone()).map_err(Arc::new));
        Client {
            task: task.shared(),
            ctx,
        }
    }

    /// Opens a connection through the tunnel.
    pub async fn open_conn(&self, remote: &str) -> anyhow::Result<Box<dyn Pipe>> {
        open_conn(&self.ctx, "tcp", remote).await
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

    /// Force a particular packet to be sent through VPN mode, regardless of whether VPN mode is on.
    pub async fn send_vpn_packet(&self, bts: Bytes) -> anyhow::Result<()> {
        send_vpn_packet(&self.ctx, bts).await;
        Ok(())
    }

    /// Receive a packet from VPN mode, regardless of whether VPN mode is on.
    pub async fn recv_vpn_packet(&self) -> anyhow::Result<Bytes> {
        let packet = recv_vpn_packet(&self.ctx).await;
        Ok(packet)
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
        smol::future::pending().await
    } else {
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

        let vpn_loop = vpn_loop(&ctx);

        let _client_loop = Immortal::spawn(client_inner(ctx.clone()));

        socks5_loop(&ctx)
            .inspect_err(|e| tracing::error!(err = debug(e), "socks5 loop stopped"))
            .race(vpn_loop.inspect_err(|e| tracing::error!(err = debug(e), "vpn loop stopped")))
            .race(
                http_proxy_serve(&ctx)
                    .inspect_err(|e| tracing::error!(err = debug(e), "http proxy stopped")),
            )
            .race(
                auth_loop(&ctx)
                    .inspect_err(|e| tracing::error!(err = debug(e), "auth loop stopped")),
            )
            .race(rpc_serve)
            .race(pac_serve(&ctx))
            .await
    }
}
