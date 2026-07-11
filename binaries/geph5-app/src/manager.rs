//! The `geph manager` supervisor: owns persisted settings and two child
//! `geph5-client` engines — a permanent, secret-less *query* engine that always
//! answers broker RPCs, and an ephemeral *tunnel* engine that exists only while
//! connected — and implements the `GephCtlProtocol` the CLI/GUI talks to.

use std::{
    process::Child,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use geph5_broker_protocol::{Credential, UserInfo};
use geph5_misc_rpc::client_control::{ConnInfo, ControlClient};
use nanorpc::RpcTransport;
use tokio::sync::Mutex;

use crate::{
    protocol::{
        AccountInfo, ConnState, ExitInfo, GephCtlProtocol, GephCtlService, ProxySettings,
        SessionContext, SettingsView, Status,
    },
    proxy,
    supervisor::{self, Settings},
};
#[cfg(all(unix, not(target_os = "macos")))]
use crate::vpn_linux as vpn;
#[cfg(target_os = "macos")]
use crate::vpn_macos as vpn;
#[cfg(windows)]
use crate::vpn_windows as vpn;

const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(30);

struct Inner {
    settings: Settings,
    /// Permanent, secret-less, dry-run engine that answers broker RPCs (exit
    /// list, account, login validation) regardless of connection state. Spawned
    /// once at startup and never restarted — so broker queries never gap.
    query: Option<Child>,
    /// The credentialed tunnel engine. Present only while connected.
    tunnel: Option<Child>,
    /// Live full-tunnel VPN state (routing/kill-switch + the tun device), when
    /// connected in VPN mode. Held here so it survives tunnel-engine restarts.
    vpn: Option<vpn::VpnHandle>,
    /// Windows only: the WinTUN<->engine stdio packet pump. Re-created per child
    /// (its session is cycled on each restart); the device/routes/kill-switch in
    /// `vpn` persist underneath it.
    #[cfg(windows)]
    vpn_pump: Option<vpn::Pump>,
}

pub struct ManagerImpl {
    // Arc so `run_manager` can hand a clone to the shutdown-signal task (which tears
    // the VPN down on SIGINT/SIGTERM — `Drop` alone never runs on a signal).
    inner: std::sync::Arc<Mutex<Inner>>,
}

impl ManagerImpl {
    /// Load settings, spawn the permanent query engine and (if persisted as
    /// connected) the tunnel engine, then return the manager.
    pub async fn start() -> anyhow::Result<Self> {
        // Purge any VPN state stranded by a prior crash / `kill -9` (whose Drop
        // never ran), so we never start on a half-configured, blackholed machine.
        #[cfg(target_os = "macos")]
        vpn::cleanup_stale();
        let settings = Settings::load()?;
        let this = ManagerImpl {
            inner: std::sync::Arc::new(Mutex::new(Inner {
                settings,
                query: None,
                tunnel: None,
                vpn: None,
                #[cfg(windows)]
                vpn_pump: None,
            })),
        };
        let mut inner = this.inner.lock().await;
        // The query engine must be up for broker queries; bring it up first.
        // Non-fatal if it fails — the manager still serves settings/connect, and
        // query calls will surface a clear error until it recovers.
        if let Err(e) = this.ensure_query_engine(&mut inner).await {
            tracing::warn!(err = %e, "query engine failed to start; broker queries unavailable");
        }
        // Restore connection state (spawns the tunnel + VPN kill-switch if the
        // user was connected). If that fails, fall back to a clean disconnected
        // state rather than refusing to start.
        if let Err(e) = Self::reconcile_tunnel(&mut inner).await {
            tracing::warn!(err = %e, "could not restore persisted state; starting disconnected");
            inner.settings.connected = false;
            let _ = inner.settings.save();
            let _ = Self::reconcile_tunnel(&mut inner).await;
        }
        drop(inner);
        Ok(this)
    }

    /// Spawn the permanent query engine if it is not already running.
    async fn ensure_query_engine(&self, inner: &mut Inner) -> Result<(), String> {
        if inner.query.is_some() {
            return Ok(());
        }
        // Run it as the service user (like the tunnel engine) so its broker
        // traffic is unmarked and the VPN kill switch lets it out. (No service
        // user on Windows — loop prevention there is socket binding, not uid.)
        #[cfg(unix)]
        let service_user = vpn::ensure_service_user().ok();
        #[cfg(windows)]
        let service_user: Option<(u32, u32)> = None;
        let child = supervisor::spawn_query(service_user).map_err(|e| format!("{e:?}"))?;
        inner.query = Some(child);
        supervisor::wait_control_ready(&supervisor::query_control(), CHILD_READY_TIMEOUT)
            .await
            .map_err(|e| format!("{e:?}"))
    }

    /// Bring the **tunnel** engine into line with `settings.connected`: kill any
    /// existing one, reconcile the VPN tunnel/kill-switch, then (if connected)
    /// spawn a fresh tunnel engine. The query engine is untouched.
    async fn reconcile_tunnel(inner: &mut Inner) -> Result<(), String> {
        // Kill the tunnel FIRST, before reconcile_vpn possibly tears the tun
        // device down. Otherwise the still-running engine's read on the tun fd
        // fails with EBADFD ("File descriptor in bad state") as the device
        // vanishes, logging a spurious error on every disconnect.
        if let Some(child) = inner.tunnel.take() {
            supervisor::kill_child(child);
        }
        // Windows: stop the old packet pump (closing its WinTUN session and
        // joining its threads) before reconciling the device. The device,
        // routes, and kill switch persist underneath in `inner.vpn`.
        #[cfg(windows)]
        {
            inner.vpn_pump = None;
        }
        // First VPN bring-up is the last point where host DNS is available before
        // the kill switch/routes go up, so pre-resolve fronted broker sources now.
        let pre_resolve_broker_fronts =
            inner.settings.connected && inner.settings.vpn && inner.vpn.is_none();
        let tunnel_config = if inner.settings.connected {
            Some(
                supervisor::build_tunnel_config(&inner.settings, pre_resolve_broker_fronts)
                    .map_err(|e| format!("{e:#}"))?,
            )
        } else {
            None
        };
        let service_user = reconcile_vpn(inner)?;
        if !inner.settings.connected {
            return Ok(());
        }
        let tunnel_config = tunnel_config.expect("connected tunnel has a prepared config");
        #[cfg(not(windows))]
        let child = {
            let vpn_fd = inner.vpn.as_ref().map(|h| h.tun_fd());
            // macOS pins the engine's own sockets to the physical NIC via
            // IP_BOUND_IF (no uid-based loop prevention); Linux uses uid marking
            // and needs no bind indices.
            #[cfg(target_os = "macos")]
            let binds = inner.vpn.as_ref().map(|h| h.bind_indices());
            #[cfg(not(target_os = "macos"))]
            let binds: Option<(u32, u32)> = None;
            supervisor::spawn_tunnel(tunnel_config, service_user, vpn_fd, binds)
                .map_err(|e| format!("{e:?}"))?
        };
        #[cfg(windows)]
        let child = {
            let _ = service_user;
            let binds = inner.vpn.as_ref().map(|h| h.bind_indices());
            let (child, stdio) = supervisor::spawn_tunnel_windows(tunnel_config, binds)
                .map_err(|e| format!("{e:?}"))?;
            // In VPN mode, wire the child's stdio to a fresh pump on the device.
            if let (Some((cin, cout)), Some(handle)) = (stdio, inner.vpn.as_ref()) {
                let session = handle.start_session().map_err(|e| format!("{e:#}"))?;
                inner.vpn_pump = Some(vpn::Pump::start(session, cin, cout));
            }
            child
        };
        inner.tunnel = Some(child);
        supervisor::wait_control_ready(&supervisor::live_control(), CHILD_READY_TIMEOUT)
            .await
            .map_err(|e| format!("{e:?}"))
    }

    /// Ask the broker (via the permanent query engine) for the account behind a
    /// secret. The credential is passed per-call, so this works regardless of
    /// connection state and never needs the tunnel engine.
    async fn account_for_secret(&self, secret: &str) -> Result<AccountInfo, String> {
        let cred = Credential::Secret(secret.to_string());
        let params = vec![serde_json::to_value(&cred).map_err(|e| e.to_string())?];
        let raw = supervisor::query_control()
            .broker_rpc("get_user_info_by_cred".into(), params)
            .await
            .map_err(|e| format!("could not reach broker: {e:?}"))??;
        let info: Option<UserInfo> = serde_json::from_value(raw).map_err(|e| e.to_string())?;
        let info = info.ok_or_else(|| "incorrect secret".to_string())?;
        Ok(account_info_from(info))
    }

    /// Forward a raw JSON-RPC call to the engine that should answer it: the
    /// tunnel engine when connected (so connection state is real), falling back
    /// to the query engine if it is momentarily unreachable (e.g. mid
    /// server-switch); the query engine when disconnected.
    async fn forward_raw(
        &self,
        req: nanorpc::JrpcRequest,
    ) -> Result<nanorpc::JrpcResponse, String> {
        let connected = self.inner.lock().await.settings.connected;
        if connected {
            match supervisor::live_control().0.call_raw(req.clone()).await {
                Ok(resp) => return Ok(resp),
                Err(e) => tracing::debug!(
                    err = ?e,
                    "tunnel engine unreachable; falling back to query engine"
                ),
            }
        }
        supervisor::query_control()
            .0
            .call_raw(req)
            .await
            .map_err(|e| format!("could not reach engine: {e:?}"))
    }
}

/// Bring the VPN tunnel/kill-switch into line with the desired state
/// (`connected && vpn`). On Unix it also resolves the service user the child runs
/// as; on Windows there is no service user (it returns `None`).
fn reconcile_vpn(inner: &mut Inner) -> Result<Option<(u32, u32)>, String> {
    let want_vpn = inner.settings.connected && inner.settings.vpn;
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if want_vpn {
            let (uid, gid) =
                vpn::ensure_service_user().map_err(|e| format!("vpn service user: {e:#}"))?;
            if inner.vpn.is_none() {
                inner.vpn = Some(vpn::setup(uid).map_err(|e| format!("vpn setup: {e:#}"))?);
            }
            Ok(Some((uid, gid)))
        } else {
            // Tear down the tunnel/kill-switch when not in active VPN mode so
            // normal (disconnected or proxy-mode) connectivity is restored.
            if inner.vpn.take().is_some() {
                vpn::teardown();
            }
            // Best-effort: still run the child unprivileged in proxy mode.
            Ok(vpn::ensure_service_user().ok())
        }
    }
    #[cfg(target_os = "macos")]
    {
        // The engine runs as the _geph service user so the PF kill switch can
        // permit its egress by uid (loop prevention itself is IP_BOUND_IF pinning).
        let (uid, gid) =
            vpn::ensure_service_user().map_err(|e| format!("vpn service user: {e:#}"))?;
        if want_vpn {
            if inner.vpn.is_none() {
                // Discover the physical interface *before* installing the /1 routes.
                let phys =
                    vpn::physical_iface().map_err(|e| format!("physical interface: {e:#}"))?;
                inner.vpn = Some(
                    vpn::setup(phys, uid, inner.settings.allow_lan)
                        .map_err(|e| format!("vpn setup: {e:#}"))?,
                );
            }
        } else if inner.vpn.take().is_some() {
            // Dropping the handle restores DNS/routes and lifts the kill switch.
        }
        Ok(Some((uid, gid)))
    }
    #[cfg(windows)]
    {
        if want_vpn {
            if inner.vpn.is_none() {
                let phys =
                    vpn::physical_iface().map_err(|e| format!("physical interface: {e:#}"))?;
                inner.vpn = Some(
                    vpn::setup(phys, inner.settings.allow_lan)
                        .map_err(|e| format!("vpn setup: {e:#}"))?,
                );
            }
        } else if inner.vpn.take().is_some() {
            // Dropping the handle removes routes + kill switch (RAII teardown).
        }
        Ok(None)
    }
}

/// Whether the settings ask for the system proxy to be auto-configured: only
/// meaningful when the local proxy is on at all.
fn wants_auto_proxy(settings: &Settings) -> bool {
    settings.proxy.as_ref().is_some_and(|p| p.autoconf)
}

/// Configure (or clear) the given session's system proxy off the reactor thread
/// (it may spawn a privilege-dropping helper). Best-effort: failures are logged.
async fn apply_proxy(session: SessionContext, connected: bool) {
    let url = format!("http://{}/proxy.pac", supervisor::PAC_ADDR);
    let res = geph5_rt::spawn_blocking(move || proxy::apply_for_session(&session, connected, &url))
        .await;
    match res {
        Ok(()) => tracing::info!(connected, "configured system proxy"),
        Err(e) => tracing::warn!(err = format!("{e:#}"), connected, "system proxy config failed"),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn account_info_from(info: UserInfo) -> AccountInfo {
    let is_plus = info.plus_expires_unix.map(|e| e > now_unix()).unwrap_or(false);
    AccountInfo {
        user_id: info.user_id,
        level: if is_plus { "plus" } else { "free" }.to_string(),
        plus_expires_unix: info.plus_expires_unix,
        bw_used_mb: info.bw_consumption.map(|b| b.mb_used),
        bw_limit_mb: info.bw_consumption.map(|b| b.mb_limit),
    }
}

fn exit_info_from(
    hostname: String,
    exit: &geph5_broker_protocol::ExitDescriptor,
    meta: Option<&geph5_broker_protocol::ExitMetadata>,
) -> ExitInfo {
    ExitInfo {
        hostname,
        country: exit.country.alpha2().to_string(),
        city: exit.city.clone(),
        load: exit.load,
        allows_free: meta
            .map(|m| {
                m.allowed_levels
                    .contains(&geph5_broker_protocol::AccountLevel::Free)
            })
            .unwrap_or(false),
    }
}

#[async_trait]
impl GephCtlProtocol for ManagerImpl {
    async fn login(&self, secret: String) -> Result<AccountInfo, String> {
        let secret = secret.trim().to_string();
        // Validate before persisting anything.
        let account = self.account_for_secret(&secret).await?;
        let mut inner = self.inner.lock().await;
        inner.settings.secret = Some(secret);
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        // The query engine is secret-less, so login never disturbs it. Only the
        // tunnel engine carries the secret, so restart it if we're connected;
        // otherwise the next connect() picks up the new secret.
        if inner.settings.connected {
            Self::reconcile_tunnel(&mut inner).await?;
        }
        Ok(account)
    }

    async fn logout(&self, session: SessionContext) -> Result<(), String> {
        let auto_proxy = {
            let mut inner = self.inner.lock().await;
            inner.settings.secret = None;
            inner.settings.connected = false;
            inner.settings.save().map_err(|e| format!("{e:?}"))?;
            Self::reconcile_tunnel(&mut inner).await?;
            wants_auto_proxy(&inner.settings)
        };
        if auto_proxy {
            apply_proxy(session, false).await;
        }
        Ok(())
    }

    async fn account(&self) -> Result<AccountInfo, String> {
        let secret = {
            let inner = self.inner.lock().await;
            inner
                .settings
                .secret
                .clone()
                .ok_or_else(|| "not logged in".to_string())?
        };
        self.account_for_secret(&secret).await
    }

    async fn connect(&self, session: SessionContext) -> Result<(), String> {
        let apply = {
            let mut inner = self.inner.lock().await;
            if inner.settings.secret.is_none() {
                return Err("not logged in".to_string());
            }
            inner.settings.connected = true;
            inner.settings.save().map_err(|e| format!("{e:?}"))?;
            Self::reconcile_tunnel(&mut inner).await?;
            // In full-tunnel VPN mode the proxy is redundant (everything is
            // already routed), so only auto-configure it in proxy mode.
            wants_auto_proxy(&inner.settings) && !inner.settings.vpn
        };
        if apply {
            apply_proxy(session, true).await;
        }
        Ok(())
    }

    async fn reconnect(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        if !inner.settings.connected {
            return Err("not connected".to_string());
        }
        // restart_child keeps the VPN tun + kill switch up across the restart
        // (reconcile_vpn leaves the handle in place while connected), so there is
        // no leak window — only the engine child is swapped.
        Self::reconcile_tunnel(&mut inner).await
    }

    async fn disconnect(&self, session: SessionContext) -> Result<(), String> {
        let auto_proxy = {
            let mut inner = self.inner.lock().await;
            inner.settings.connected = false;
            inner.settings.save().map_err(|e| format!("{e:?}"))?;
            Self::reconcile_tunnel(&mut inner).await?;
            wants_auto_proxy(&inner.settings)
        };
        if auto_proxy {
            apply_proxy(session, false).await;
        }
        Ok(())
    }

    async fn status(&self) -> Result<Status, String> {
        // Connection status lives in the tunnel engine, which only exists while
        // connected.
        if !self.inner.lock().await.settings.connected {
            return Ok(Status {
                state: ConnState::Disconnected,
                exit: None,
                total_rx_bytes: 0.0,
                total_tx_bytes: 0.0,
            });
        }
        let client: ControlClient = supervisor::live_control();
        let conn = match client.conn_info().await {
            Ok(conn) => conn,
            // We intend to be connected but the tunnel engine is momentarily
            // unreachable (e.g. mid server-switch): report connecting, not error.
            Err(_) => {
                return Ok(Status {
                    state: ConnState::Connecting,
                    exit: None,
                    total_rx_bytes: 0.0,
                    total_tx_bytes: 0.0,
                });
            }
        };
        let (state, exit) = match conn {
            ConnInfo::Disconnected => (ConnState::Disconnected, None),
            ConnInfo::Connecting => (ConnState::Connecting, None),
            ConnInfo::Connected { sessions } => {
                let exit = sessions.first().map(|s| {
                    exit_info_from(
                        s.exit.country.alpha2().to_string(),
                        &s.exit,
                        None,
                    )
                });
                (ConnState::Connected, exit)
            }
        };
        let total_rx_bytes = client.stat_num("total_rx_bytes".into()).await.unwrap_or(0.0);
        let total_tx_bytes = client.stat_num("total_tx_bytes".into()).await.unwrap_or(0.0);
        Ok(Status {
            state,
            exit,
            total_rx_bytes,
            total_tx_bytes,
        })
    }

    async fn get_settings(&self) -> Result<SettingsView, String> {
        let inner = self.inner.lock().await;
        Ok(SettingsView {
            logged_in: inner.settings.secret.is_some(),
            exit_constraint: inner.settings.exit_constraint.clone(),
            connected: inner.settings.connected,
            proxy: inner.settings.proxy.clone(),
            vpn: inner.settings.vpn,
            allow_lan: inner.settings.allow_lan,
            allow_direct: inner.settings.allow_direct,
        })
    }

    async fn set_exit_constraint(
        &self,
        constraint: geph5_broker_protocol::ExitConstraint,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        tracing::debug!(?constraint, "setting changed: exit_constraint");
        inner.settings.exit_constraint = constraint;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        // Only the connected child needs restarting; a disconnected one rebuilds
        // from settings on the next connect().
        if inner.settings.connected {
            Self::reconcile_tunnel(&mut inner).await?;
        }
        Ok(())
    }

    async fn set_proxy_settings(
        &self,
        proxy: Option<ProxySettings>,
        session: SessionContext,
    ) -> Result<(), String> {
        let apply = {
            let mut inner = self.inner.lock().await;
            // The GUI pushes all settings before every connect; skip the tunnel
            // restart (and proxy churn) when nothing actually changed.
            if inner.settings.proxy == proxy {
                None
            } else {
                tracing::debug!(?proxy, "setting changed: proxy");
                inner.settings.proxy = proxy;
                inner.settings.save().map_err(|e| format!("{e:?}"))?;
                if inner.settings.connected {
                    // Rebinds (or unbinds) the proxy listeners with the new config.
                    Self::reconcile_tunnel(&mut inner).await?;
                    Some(wants_auto_proxy(&inner.settings) && !inner.settings.vpn)
                } else {
                    None
                }
            }
        };
        // Reflect the change in the live system proxy: enabling sets it for the
        // caller's session, disabling (or turning the proxy off) clears it.
        if let Some(on) = apply {
            apply_proxy(session, on).await;
        }
        Ok(())
    }

    async fn set_vpn_mode(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        tracing::debug!(enabled, "setting changed: vpn");
        inner.settings.vpn = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        // reconcile_tunnel reconciles the VPN tunnel/kill-switch with settings.vpn.
        if inner.settings.connected {
            Self::reconcile_tunnel(&mut inner).await?;
        }
        Ok(())
    }

    async fn set_allow_lan(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        tracing::debug!(enabled, "setting changed: allow_lan");
        inner.settings.allow_lan = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        if inner.settings.connected {
            Self::reconcile_tunnel(&mut inner).await?;
        }
        Ok(())
    }

    async fn set_allow_direct(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        tracing::debug!(enabled, "setting changed: allow_direct");
        inner.settings.allow_direct = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        if inner.settings.connected {
            Self::reconcile_tunnel(&mut inner).await?;
        }
        Ok(())
    }

    async fn list_exits(&self) -> Result<Vec<ExitInfo>, String> {
        // A broker query — always served by the permanent query engine, so it
        // works whether or not we're connected and never gaps on a tunnel restart.
        let net_status = supervisor::query_control()
            .net_status()
            .await
            .map_err(|e| format!("could not reach broker: {e:?}"))??;
        let mut out: Vec<ExitInfo> = net_status
            .exits
            .into_iter()
            .map(|(hostname, (_pk, exit, meta))| exit_info_from(hostname, &exit, Some(&meta)))
            .collect();
        out.sort_by(|a, b| {
            (a.country.as_str(), a.city.as_str()).cmp(&(b.country.as_str(), b.city.as_str()))
        });
        Ok(out)
    }

    async fn logs(&self, count: usize) -> Result<Vec<String>, String> {
        // Connection logs come from the tunnel engine when connected; otherwise
        // the query engine's (quieter) logs.
        let connected = self.inner.lock().await.settings.connected;
        let client = if connected {
            supervisor::live_control()
        } else {
            supervisor::query_control()
        };
        let mut logs = client
            .recent_logs()
            .await
            .map_err(|e| format!("could not reach engine: {e:?}"))?;
        if logs.len() > count {
            logs = logs.split_off(logs.len() - count);
        }
        Ok(logs)
    }

    async fn daemon_rpc(
        &self,
        method: String,
        params: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let req = nanorpc::JrpcRequest {
            jsonrpc: "2.0".into(),
            method,
            params,
            id: nanorpc::JrpcId::Number(1),
        };
        // Route to the tunnel engine when connected (real connection state),
        // falling back to the always-up query engine otherwise.
        let resp = self.forward_raw(req).await?;
        match resp.error {
            Some(err) => Err(err.message),
            None => Ok(resp.result.unwrap_or(serde_json::Value::Null)),
        }
    }
}

/// Wait for a termination signal (SIGINT/SIGTERM on unix, Ctrl-C on Windows).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // Await one delivery of `kind`, or pend forever if it can't be installed.
        async fn on(kind: SignalKind) {
            match signal(kind) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        }
        // Every way a foreground manager is normally told to stop. Missing any
        // strands the kill switch / routes and blackholes the machine, since Drop
        // doesn't run on an uncaught signal.
        tokio::select! {
            _ = on(SignalKind::hangup()) => {},     // terminal close
            _ = on(SignalKind::interrupt()) => {},  // Ctrl-C
            _ = on(SignalKind::terminate()) => {},  // kill / launchd stop
            _ = on(SignalKind::quit()) => {},       // Ctrl-\
        }
    }
    #[cfg(windows)]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Tear down everything the manager installed, so a Ctrl-C / `kill` cleanly restores
/// normal networking instead of leaving the fail-closed kill switch (and routes /
/// DNS) stranded with no engine to carry traffic. Mirrors the disconnected path of
/// `reconcile_vpn`.
async fn shutdown_teardown(inner: &Mutex<Inner>) {
    let mut inner = inner.lock().await;
    if let Some(child) = inner.tunnel.take() {
        supervisor::kill_child(child);
    }
    if let Some(child) = inner.query.take() {
        supervisor::kill_child(child);
    }
    #[cfg(windows)]
    {
        inner.vpn_pump = None;
    }
    // Dropping the handle tears down routes/DNS (and, on macOS/Windows, the kill
    // switch) via its Drop. Linux's nftables teardown is a separate free function.
    let had_vpn = inner.vpn.take().is_some();
    #[cfg(all(unix, not(target_os = "macos")))]
    if had_vpn {
        vpn::teardown();
    }
    let _ = had_vpn;
}

/// Run the manager forever: spawn the children and serve the CLI control protocol.
pub async fn run_manager() -> anyhow::Result<()> {
    let manager = ManagerImpl::start().await?;

    // Graceful shutdown: on a termination signal, tear the VPN down and exit. Rust
    // `Drop` does not run when the process is killed by a signal, so without this a
    // Ctrl-C would leave the kill switch / routes / DNS in place and strand the
    // machine offline until the manager is restarted.
    {
        let inner = manager.inner.clone();
        geph5_rt::spawn(async move {
            shutdown_signal().await;
            tracing::warn!("termination signal received; tearing down VPN and exiting");
            shutdown_teardown(&inner).await;
            std::process::exit(0);
        })
        .detach();
    }

    // macOS resilience watchdog: the physical interface + interface-scoped default
    // are captured once at connect, and sleep/wake or a network switch invalidates
    // them — leaving the engine unable to reach any exit. Driven by the kernel's
    // routing socket so it reacts to a wake within ~a second, with a slow poll as a
    // backstop for any missed event; on each wake it re-checks and repairs (or
    // fully rebuilds on a physical change).
    #[cfg(target_os = "macos")]
    {
        let inner = manager.inner.clone();
        let route_changed = std::sync::Arc::new(tokio::sync::Notify::new());
        // Block on PF_ROUTE in a dedicated thread; coalesce bursts via Notify.
        {
            let route_changed = route_changed.clone();
            std::thread::spawn(move || vpn::route_change_loop(|| route_changed.notify_one()));
        }
        geph5_rt::spawn(async move {
            loop {
                tokio::select! {
                    _ = route_changed.notified() => {
                        // Let the burst of route changes (DHCP, iface up) settle.
                        geph5_rt::sleep(Duration::from_secs(2)).await;
                    }
                    _ = geph5_rt::sleep(Duration::from_secs(60)) => {}
                }
                let mut inner = inner.lock().await;
                if !(inner.settings.connected && inner.settings.vpn) {
                    continue;
                }
                let action = match inner.vpn.as_ref() {
                    Some(handle) => vpn::network_check(handle),
                    None => continue,
                };
                match action {
                    vpn::VpnAction::Healthy => {}
                    vpn::VpnAction::Respawn => {
                        tracing::warn!("VPN routing went stale (sleep/wake?); repaired, respawning engine");
                        let _ = ManagerImpl::reconcile_tunnel(&mut inner).await;
                    }
                    vpn::VpnAction::Rebuild => {
                        tracing::warn!("physical network changed; rebuilding VPN");
                        inner.vpn = None; // drop → tear down stale routes/kill-switch
                        let _ = ManagerImpl::reconcile_tunnel(&mut inner).await;
                    }
                }
            }
        })
        .detach();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = supervisor::manager_control_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let listener = sillad::unix::UnixListener::bind(&path).await?; // unlinks any stale socket
        // The CLI/GUI run unprivileged; let them connect to the root manager's socket.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));
        tracing::info!(path = %path.display(), "geph manager listening for clients");
        nanorpc_sillad::rpc_serve(listener, GephCtlService(manager)).await?;
    }
    #[cfg(windows)]
    {
        // The CLI/GUI run unprivileged but the manager may run as a service; the
        // SDDL grants authenticated users access (the pipe analogue of chmod 0666).
        let name = supervisor::MANAGER_CONTROL_PIPE;
        let listener = sillad::windows_pipe::NamedPipeListener::bind(
            name,
            Some(sillad::windows_pipe::SDDL_ALLOW_AUTHENTICATED),
        )?;
        tracing::info!(name, "geph manager listening for clients");
        nanorpc_sillad::rpc_serve(listener, GephCtlService(manager)).await?;
    }
    Ok(())
}
