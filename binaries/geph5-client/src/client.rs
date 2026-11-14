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
use std::{
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
};
#[cfg(unix)]
use tokio::io::{Interest, unix::AsyncFd};

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
pub use geph5_misc_rpc::client_config::{BrokerKeys, Config, LoggingConfig};

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
        Self::start_in_context(AnyCtx::new(cfg), vpn_fd)
    }

    #[cfg(unix)]
    fn start_in_context(ctx: AnyCtx<Config>, vpn_fd: Option<i32>) -> Self {
        // Initialize logging once we have context so file paths are config-aware.
        let logging_result = logging::init_logging(&ctx);
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
            logging_result
                .map_err(|err| anyhow::anyhow!("could not initialize client logging: {err:#}"))?;
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
        let logging_result = logging::init_logging(&ctx);
        let ((fd_limit, _), _) = binary_search::binary_search((1, ()), (65536, ()), |lim| {
            if rlimit::increase_nofile_limit(lim).unwrap_or_default() >= lim {
                binary_search::Direction::Low(())
            } else {
                binary_search::Direction::High(())
            }
        });
        tracing::info!("raised file descriptor limit to {}", fd_limit);

        let task_ctx = ctx.clone();
        let task = geph5_rt::spawn(
            async move {
                logging_result.map_err(|err| {
                    anyhow::anyhow!("could not initialize client logging: {err:#}")
                })?;
                client_main(task_ctx).await
            }
            .map_err(Arc::new),
        );
        Client {
            task: task.shared(),
            ctx,
        }
    }

    /// Obtain a valid connection token before attempting a tunnel connection.
    ///
    /// Diagnostic callers can use this to distinguish broker authentication
    /// failures from failures on a pinned bridge route. This uses the ordinary
    /// authentication cache and exposes no token or signature to the caller.
    pub async fn prepare_connect_token(&self) -> anyhow::Result<()> {
        crate::auth::get_connect_token(&self.ctx).await?;
        Ok(())
    }

    /// Refresh and export reusable authentication for trusted diagnostic workers.
    ///
    /// Call on an inert client with a persistent cache. Normal broker issuance
    /// and signature verification apply; existing current/next-day tokens are
    /// reused. The returned bytes contain secrets and must never be logged.
    /// Single-use bandwidth tokens are deliberately excluded.
    pub async fn prepare_cached_authentication(&self) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        anyhow::ensure!(
            self.ctx.init().dry_run,
            "authentication exporter requires an inert client"
        );
        let token = get_auth_token(&self.ctx).await?;
        if crate::auth::refresh_conn_token(&self.ctx, &token)
            .await
            .is_err()
        {
            tracing::warn!(
                "authentication refresh failed; checking existing connection credentials"
            );
        }
        let epoch = mizaru2::current_epoch();
        let mut result = vec![];
        for key in [
            "auth_token".to_owned(),
            format!("conn_token_{epoch}"),
            format!("conn_token_{}", epoch + 1),
        ] {
            match crate::database::db_read(&self.ctx, &key).await? {
                Some(value) => result.push((key, value)),
                None if key == format!("conn_token_{}", epoch + 1) => {}
                None => anyhow::bail!("missing current authentication cache entry"),
            }
        }
        Ok(result)
    }

    /// Start a Unix diagnostic client with reusable auth seeded before its loops.
    ///
    /// This is trusted local input from `prepare_cached_authentication`, for the
    /// same account and broker keys. Only current/next-day connection tokens and
    /// the auth token are accepted; routes and single-use bandwidth tokens cannot
    /// be imported. The next-day token is optional. Workers use current cached
    /// credentials immediately and do not refresh account status; bandwidth
    /// token acquisition and exit authentication continue normally.
    #[cfg(unix)]
    pub async fn start_with_cached_authentication(
        cfg: Config,
        entries: &[(String, Vec<u8>)],
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            cfg.diagnostic_route.is_some(),
            "cached authentication requires a diagnostic route"
        );
        let epoch = mizaru2::current_epoch();
        let allowed = [
            "auth_token".to_owned(),
            format!("conn_token_{epoch}"),
            format!("conn_token_{}", epoch + 1),
        ];
        anyhow::ensure!(
            (2..=3).contains(&entries.len())
                && allowed[..2].iter().all(|key| entries
                    .iter()
                    .filter(|(k, v)| k == key && !v.is_empty())
                    .count()
                    == 1)
                && entries.iter().all(|(key, value)| allowed.contains(key)
                    && !value.is_empty()
                    && entries.iter().filter(|(k, _)| k == key).count() == 1),
            "invalid authentication cache entries"
        );
        // Validate every payload before opening or modifying the persistent cache.
        // The input is locally trusted; exit authentication verifies signatures.
        let mut is_plus = false;
        for (key, value) in entries {
            if key == "auth_token" {
                std::str::from_utf8(value).context("invalid cached auth token")?;
                continue;
            }
            let (level, _, signature): (
                geph5_broker_protocol::AccountLevel,
                mizaru2::ClientToken,
                mizaru2::UnblindedSignature,
            ) = stdcode::deserialize(value).context("invalid cached connection token")?;
            let expected_epoch = if key == &allowed[1] { epoch } else { epoch + 1 };
            anyhow::ensure!(
                signature.epoch == expected_epoch,
                "cached connection token epoch mismatch"
            );
            if expected_epoch == epoch {
                is_plus = level == geph5_broker_protocol::AccountLevel::Plus;
            }
        }
        let ctx = AnyCtx::new(cfg);
        crate::database::db_write_batch(&ctx, entries).await?;
        ctx.get(crate::auth::CACHED_AUTHENTICATION)
            .store(true, std::sync::atomic::Ordering::SeqCst);
        ctx.get(crate::auth::IS_PLUS)
            .store(is_plus, std::sync::atomic::Ordering::SeqCst);
        Ok(Self::start_in_context(ctx, None))
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
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("could not get VPN fd flags");
    }
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error()).context("could not make VPN fd nonblocking");
    }
    let async_fd = AsyncFd::new(file).context("could not register VPN fd with Tokio")?;

    let read_task = async {
        let mut buf = vec![0u8; 65535]; // Buffer for reading packets
        loop {
            match async_fd
                .async_io(Interest::READABLE, |mut file| file.read(&mut buf))
                .await
            {
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

            match async_fd
                .async_io(Interest::WRITABLE, |mut file| file.write(&packet))
                .await
            {
                Ok(written) if written == packet.len() => {}
                Ok(written) => {
                    tracing::error!(
                        written,
                        expected = packet.len(),
                        "Partial packet write to VPN fd"
                    );
                    break;
                }
                Err(e) => {
                    tracing::error!("Error writing to VPN fd: {}", e);
                    break;
                }
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

#[cfg(all(test, unix))]
mod authentication_cache_tests {
    use super::*;
    use geph5_broker_protocol::{ExitDescriptor, ExitRouteDescriptor, RouteDescriptor};

    fn config() -> Config {
        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "exit_constraint": "auto", "dry_run": true, "cache": ":memory:",
            "logging": { "disabled": true }
        }))
        .unwrap();
        cfg.diagnostic_route = Some(ExitRouteDescriptor {
            exit_pubkey: ed25519_dalek::SigningKey::from_bytes(&[7; 32]).verifying_key(),
            exit: ExitDescriptor {
                c2e_listen: "127.0.0.1:9000".parse().unwrap(),
                b2e_listen: "127.0.0.1:9001".parse().unwrap(),
                country: isocountry::CountryCode::CAN,
                city: "test".into(),
                load: 0.0,
                expiry: u64::MAX,
            },
            route: RouteDescriptor::Tcp("127.0.0.1:9002".parse().unwrap()),
        });
        cfg
    }

    fn entries() -> Vec<(String, Vec<u8>)> {
        use stdcode::StdcodeSerializeExt;
        let epoch = mizaru2::current_epoch();
        let token = |epoch| {
            (
                geph5_broker_protocol::AccountLevel::Plus,
                mizaru2::ClientToken::random(),
                mizaru2::UnblindedSignature {
                    epoch,
                    used_key: vec![],
                    merkle_branch: vec![],
                    unblinded_sig: vec![],
                },
            )
                .stdcode()
        };
        vec![
            ("auth_token".into(), vec![1]),
            (format!("conn_token_{epoch}"), token(epoch)),
            (format!("conn_token_{}", epoch + 1), token(epoch + 1)),
        ]
    }

    #[tokio::test]
    async fn imports_reusable_auth_before_start_without_copying_bandwidth_tokens() {
        let entries = entries();
        let client = Client::start_with_cached_authentication(config(), &entries)
            .await
            .unwrap();
        for (key, value) in entries {
            assert_eq!(
                crate::database::db_read(&client.ctx, &key).await.unwrap(),
                Some(value)
            );
        }
        let count: i64 =
            sqlx::query_scalar("select count(*) from sqlite_master where name='bw_tokens'")
                .fetch_one(client.ctx.get(crate::database::DATABASE))
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn cached_credentials_work_without_broker_or_account_status_wait() {
        let entries = entries();
        let client = Client::start_with_cached_authentication(config(), &entries[..2])
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            client.prepare_connect_token(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            client
                .ctx
                .get(crate::auth::IS_PLUS)
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        // No broker is configured. Refresh must retain a usable current token.
        assert_eq!(
            client.prepare_cached_authentication().await.unwrap(),
            entries[..2]
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(25),
                crate::auth::auth_loop(&client.ctx)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn diagnostic_auth_loop_never_contacts_configured_broker() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = config();
        cfg.broker = Some(geph5_misc_rpc::client_config::BrokerSource::Direct(
            format!("http://{}/", listener.local_addr().unwrap()),
        ));
        let client = Client::start_with_cached_authentication(cfg, &entries())
            .await
            .unwrap();
        let refresh = crate::auth::auth_loop(&client.ctx);
        tokio::pin!(refresh);
        tokio::select! {
            _ = listener.accept() => panic!("diagnostic authentication contacted broker"),
            _ = &mut refresh => panic!("diagnostic auth loop must remain idle"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
        }
    }

    #[tokio::test]
    async fn rejected_imports_preserve_persistent_credentials() {
        let path =
            std::env::temp_dir().join(format!("geph-auth-import-{}.db", rand::random::<u64>()));
        let mut cfg = config();
        cfg.cache = Some(path.clone());
        let ctx = AnyCtx::new(cfg.clone());
        let original = entries();
        crate::database::db_write_batch(&ctx, &original)
            .await
            .unwrap();

        let mut replacement = entries();
        replacement[0].1 = b"replacement-auth".to_vec();
        let mut invalid = Vec::new();
        for index in [1, 2] {
            let mut malformed = replacement.clone();
            malformed[index].1 = vec![0xff];
            invalid.push(malformed);
            let mut wrong_epoch = replacement.clone();
            wrong_epoch[index].1 = replacement[3 - index].1.clone();
            invalid.push(wrong_epoch);
        }
        let mut invalid_auth = replacement.clone();
        invalid_auth[0].1 = vec![0xff];
        invalid.push(invalid_auth);

        for entries in invalid {
            assert!(
                Client::start_with_cached_authentication(cfg.clone(), &entries)
                    .await
                    .is_err()
            );
            for (key, value) in &original {
                assert_eq!(
                    crate::database::db_read(&ctx, key).await.unwrap().as_ref(),
                    Some(value)
                );
            }
        }

        // Fail after the first write to verify that the whole import rolls back.
        sqlx::query(
            "CREATE TRIGGER reject_connection_import BEFORE INSERT ON misc
                     WHEN NEW.key LIKE 'conn_token_%'
                     BEGIN SELECT RAISE(ABORT, 'injected write failure'); END",
        )
        .execute(ctx.get(crate::database::DATABASE))
        .await
        .unwrap();
        assert!(
            Client::start_with_cached_authentication(cfg, &replacement)
                .await
                .is_err()
        );
        for (key, value) in &original {
            assert_eq!(
                crate::database::db_read(&ctx, key).await.unwrap().as_ref(),
                Some(value)
            );
        }
        ctx.get(crate::database::DATABASE).close().await;
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn rejects_missing_stale_duplicate_and_non_auth_entries() {
        let valid = entries();
        for key in ["route_cache", "bw_tokens", "conn_token_0", "auth_token"] {
            let mut entries = valid.clone();
            entries[2].0 = key.into();
            assert!(
                Client::start_with_cached_authentication(config(), &entries)
                    .await
                    .is_err()
            );
        }
        assert!(
            Client::start_with_cached_authentication(config(), &valid[..1])
                .await
                .is_err()
        );
        let mut cfg = config();
        cfg.diagnostic_route = None;
        assert!(
            Client::start_with_cached_authentication(cfg, &valid)
                .await
                .is_err()
        );
    }
}
