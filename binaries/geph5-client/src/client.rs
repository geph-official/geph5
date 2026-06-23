use anyctx::AnyCtx;

use anyhow::Context;
use bytes::Bytes;
use futures_util::{
    AsyncReadExt, AsyncWriteExt, FutureExt, TryFutureExt, future::Shared, task::noop_waker,
};
use geph5_broker_protocol::UserInfo;
use geph5_misc_rpc::client_control::{ControlClient, ControlService};
use nanorpc::DynRpcTransport;
use sillad::Pipe;
use smol::future::FutureExt as _;
use std::{fs::File, sync::Arc};

use smolscale::immortal::Immortal;

use crate::{
    auth::{auth_loop, get_auth_token},
    broker::broker_client,
    bw_token::bw_token_refresh_loop,
    control_prot::{ControlProtocolImpl, DummyControlProtocolTransport},
    http_proxy::http_proxy_serve,
    logging,
    pac::pac_serve,
    port_forward::port_forward,
    session::{open_conn, run_session},
    socks5::socks5_loop,
    vpn::{recv_vpn_packet, send_vpn_packet, vpn_loop},
};

// `Config` and its broker-source descriptors live in `geph5-misc-rpc` so that
// tools which only configure or drive an engine (the `geph` daemon/CLI) can
// depend on that lightweight crate instead of this whole engine. Re-exported
// here so `geph5_client::Config` etc. keep working. The behavior over
// `BrokerSource` lives in `broker.rs` as extension traits.
pub use geph5_misc_rpc::client_config::{BrokerKeys, Config};

#[derive(Clone)]
pub struct Client {
    task: Shared<smol::Task<Result<(), Arc<anyhow::Error>>>>,
    ctx: AnyCtx<Config>,
}

impl Client {
    /// Starts the client logic in the loop, returning the handle.
    pub fn start(cfg: Config) -> Self {
        Self::start_with_vpn_fd(cfg, None)
    }

    /// Starts the client logic in the loop with an optional platform-VPN file
    /// descriptor wired into the VPN channels. The fd is consumed by this call.
    #[cfg(unix)]
    pub fn start_with_vpn_fd(cfg: Config, vpn_fd: Option<i32>) -> Self {
        let ctx = AnyCtx::new(cfg.clone());
        // Initialize logging once we have context so JSON logs go to SQLite
        let _ = logging::init_logging(&ctx);
        let ((fd_limit, _), _) = binary_search::binary_search((1, ()), (65536, ()), |lim| {
            if rlimit::increase_nofile_limit(lim).unwrap_or_default() >= lim {
                binary_search::Direction::Low(())
            } else {
                binary_search::Direction::High(())
            }
        });
        tracing::info!("raised file descriptor limit to {}", fd_limit);

        if let Some(fd) = vpn_fd {
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

    /// Starts the client logic in the loop. Non-Unix targets never get a VPN fd.
    #[cfg(not(unix))]
    pub fn start_with_vpn_fd(cfg: Config, _vpn_fd: Option<i32>) -> Self {
        let ctx = AnyCtx::new(cfg.clone());
        let _ = logging::init_logging(&ctx);
        let ((fd_limit, _), _) = binary_search::binary_search((1, ()), (65536, ()), |lim| {
            if rlimit::increase_nofile_limit(lim).unwrap_or_default() >= lim {
                binary_search::Direction::Low(())
            } else {
                binary_search::Direction::High(())
            }
        });
        tracing::info!("raised file descriptor limit to {}", fd_limit);

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
    let tcp_rpc_serve = async {
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
    let unix_rpc_serve = async {
        #[cfg(unix)]
        if let Some(path) = ctx.init().control_listen_unix.as_ref() {
            nanorpc_sillad::rpc_serve(
                sillad::unix::UnixListener::bind(path).await?,
                ControlService(ControlProtocolImpl { ctx: ctx.clone() }),
            )
            .await?;
            return anyhow::Ok(());
        }
        smol::future::pending().await
    };
    let pipe_rpc_serve = async {
        #[cfg(windows)]
        if let Some(name) = ctx.init().control_listen_pipe.as_ref() {
            nanorpc_sillad::rpc_serve(
                sillad::windows_pipe::NamedPipeListener::bind(name, None)?,
                ControlService(ControlProtocolImpl { ctx: ctx.clone() }),
            )
            .await?;
            return anyhow::Ok(());
        }
        smol::future::pending().await
    };
    let rpc_serve = tcp_rpc_serve.race(unix_rpc_serve).race(pipe_rpc_serve);
    if ctx.init().dry_run {
        rpc_serve.await
    } else {
        let vpn_loop = vpn_loop(&ctx);

        let _client_loop = Immortal::spawn(run_session(ctx.clone()));

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
            .race(
                bw_token_refresh_loop(&ctx)
                    .inspect_err(|e| tracing::error!(err = debug(e), "bw token loop stopped")),
            )
            .race(rpc_serve)
            .race(pac_serve(&ctx))
            .race(port_forward(&ctx))
            .await
    }
}
