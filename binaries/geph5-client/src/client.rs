use anyctx::AnyCtx;

use anyhow::Context;
use bytes::Bytes;
use futures_concurrency::future::Race as _;
use futures_util::{FutureExt, TryFutureExt, future::Shared, task::noop_waker};
use geph5_broker_protocol::UserInfo;
use geph5_misc_rpc::client_control::{ControlClient, ControlService};
use geph5_rt::Immortal;
use nanorpc::DynRpcTransport;
use sillad::Pipe;
use std::sync::Arc;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    task: Shared<geph5_rt::Task<Result<(), Arc<anyhow::Error>>>>,
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

        let client_ctx = ctx.clone();
        // Race the platform-VPN packet pump against the main client logic rather
        // than detaching it. A detached task's panic (e.g. an unwrappable fd) was
        // swallowed by the runtime, leaving the engine reporting healthy while no
        // packets flowed; folding the pump into the client task surfaces any such
        // failure through wait_until_dead / check_dead.
        let combined = async move {
            let main_fut = client_main(ctx.clone());
            match vpn_fd {
                Some(fd) => (main_fut, run_vpn_fd_handler(ctx, fd)).race().await,
                None => main_fut.await,
            }
        };
        let task = geph5_rt::spawn(combined.map_err(Arc::new));
        Client {
            task: task.shared(),
            ctx: client_ctx,
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

        let task = geph5_rt::spawn(client_main(ctx.clone()).map_err(Arc::new));
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
            .poll_unpin(&mut std::task::Context::from_waker(&noop_waker()))
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

/// Pump packets between a platform-supplied VPN file descriptor and the engine's
/// VPN channels until one direction ends. Returns an error if the fd cannot be
/// wrapped for async I/O, so the caller (which races this against the main
/// client logic) can surface the failure rather than losing it in a detached
/// task.
#[cfg(unix)]
async fn run_vpn_fd_handler(ctx: AnyCtx<Config>, fd: i32) -> anyhow::Result<()> {
    // Create an async file descriptor from the raw fd.
    let file: std::fs::File = unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) };
    let async_fd = geph5_rt::asyncfd::AsyncFdStream::new(file)
        .context("could not wrap VPN fd in AsyncFdStream")?;

    // Split the file descriptor for reading and writing.
    let (mut reader, mut writer) = tokio::io::split(async_fd);

    let read_task = async {
        let mut buf = vec![0u8; 65535]; // Buffer for reading packets
        loop {
            match reader.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    // macOS utun prepends a 4-byte address-family header to every
                    // packet; strip it to recover the raw IP packet the IP stack
                    // expects.
                    #[cfg(target_os = "macos")]
                    let pkt = {
                        if n <= 4 {
                            continue;
                        }
                        bytes::Bytes::copy_from_slice(&buf[4..n])
                    };
                    #[cfg(not(target_os = "macos"))]
                    let pkt = bytes::Bytes::copy_from_slice(&buf[..n]);
                    send_vpn_packet(&ctx, pkt).await;
                }
                Ok(0) => {
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

    let write_task = async {
        loop {
            let packet = recv_vpn_packet(&ctx).await;

            // macOS utun expects a 4-byte address-family header before the IP
            // packet, written as a single datagram. Pick AF from the IP version
            // nibble (AF_INET=2, AF_INET6=30).
            #[cfg(target_os = "macos")]
            let packet = {
                let af: u32 = if packet.first().map(|b| b >> 4) == Some(6) {
                    30
                } else {
                    2
                };
                let mut framed = Vec::with_capacity(4 + packet.len());
                framed.extend_from_slice(&af.to_be_bytes());
                framed.extend_from_slice(&packet);
                bytes::Bytes::from(framed)
            };

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

    let res = (read_task, write_task).race().await;
    tracing::warn!("VPN fd handler exited");
    res
}

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
            std::future::pending().await
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
        std::future::pending().await
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
        std::future::pending().await
    };
    let rpc_serve = (tcp_rpc_serve, unix_rpc_serve, pipe_rpc_serve).race();
    if ctx.init().dry_run {
        rpc_serve.await
    } else {
        let vpn_loop = vpn_loop(&ctx);

        let _client_loop = Immortal::spawn(run_session(ctx.clone()));

        (
            socks5_loop(&ctx)
                .inspect_err(|e| tracing::error!(err = debug(e), "socks5 loop stopped")),
            vpn_loop.inspect_err(|e| tracing::error!(err = debug(e), "vpn loop stopped")),
            http_proxy_serve(&ctx)
                .inspect_err(|e| tracing::error!(err = debug(e), "http proxy stopped")),
            auth_loop(&ctx).inspect_err(|e| tracing::error!(err = debug(e), "auth loop stopped")),
            bw_token_refresh_loop(&ctx)
                .inspect_err(|e| tracing::error!(err = debug(e), "bw token loop stopped")),
            rpc_serve,
            pac_serve(&ctx),
            port_forward(&ctx),
        )
            .race()
            .await
    }
}
