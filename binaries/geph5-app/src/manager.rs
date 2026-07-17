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
use geph5_misc_rpc::{
    client_control::{ConnInfo, ControlClient},
    manager_control::{
        AccountInfo, ConnState, ExitInfo, GephCtlProtocol, ProxySettings, SessionContext,
        SettingsView, Status,
    },
};
use nanorpc::RpcTransport;
use tokio::sync::Mutex;

use crate::{
    platform,
    supervisor::{self, Settings},
    vpn,
};

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
    vpn: vpn::Vpn,
    /// Whether this manager successfully installed a system PAC setting, and
    /// the desktop session needed to remove it on platforms with per-user proxy
    /// configuration.
    proxy_active: bool,
    proxy_session: Option<SessionContext>,
    shutting_down: bool,
}

#[derive(Clone)]
pub struct ManagerImpl {
    // Arc so `run_manager` can hand a clone to the shutdown-signal task (which tears
    // the VPN down on SIGINT/SIGTERM, when normal cleanup cannot run by itself).
    inner: std::sync::Arc<Mutex<Inner>>,
}

impl ManagerImpl {
    /// Load settings, spawn the permanent query engine and (if persisted as
    /// connected) the tunnel engine, then return the manager.
    pub async fn start() -> anyhow::Result<Self> {
        // Purge any VPN state stranded by a prior crash / `kill -9`, so we never
        // start on a half-configured, blackholed machine. This runs regardless of
        // persisted state, including when the manager starts disconnected.
        vpn::cleanup_stale();
        let settings = Settings::load()?;
        let this = ManagerImpl {
            inner: std::sync::Arc::new(Mutex::new(Inner {
                settings,
                query: None,
                tunnel: None,
                vpn: vpn::Vpn::new(),
                proxy_active: false,
                proxy_session: None,
                shutting_down: false,
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
        let want_proxy =
            wants_auto_proxy(&inner.settings) && inner.settings.connected && !inner.settings.vpn;
        drop(inner);
        if apply_proxy(None, want_proxy).await {
            let mut inner = this.inner.lock().await;
            inner.proxy_active = want_proxy;
        }
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
        //
        // Fail closed: this engine is network-facing (it makes broker RPCs over
        // fronted TLS), so it must never fall back to running as root. If the
        // unprivileged service user cannot be established, don't start it.
        let service_user =
            platform::ensure_service_user().map_err(|e| format!("engine service user: {e:#}"))?;
        let child = supervisor::spawn_query(service_user).map_err(|e| format!("{e:?}"))?;
        inner.query = Some(child);
        supervisor::wait_control_ready(&supervisor::query_control(), CHILD_READY_TIMEOUT)
            .await
            .map_err(|e| format!("{e:?}"))
    }

    /// Bring the **tunnel** engine into line with `settings.connected`: kill any
    /// existing one, reconcile the VPN tunnel/kill-switch, then (if connected)
    /// spawn a fresh tunnel engine. The query engine is untouched.
    ///
    /// `force_vpn_rebuild` tears down and re-creates the live VPN handle even if
    /// one already exists — used when the physical network changed and the
    /// routes/pin are stale. The old handle is kept up through the slow steps
    /// (child kill, config build) and swapped only inside `reconcile_vpn`, so the
    /// kill switch is never lifted for longer than the back-to-back teardown+setup.
    async fn reconcile_tunnel(inner: &mut Inner) -> Result<(), String> {
        Self::reconcile_tunnel_inner(inner, false).await
    }

    async fn reconcile_tunnel_inner(
        inner: &mut Inner,
        force_vpn_rebuild: bool,
    ) -> Result<(), String> {
        // Kill the tunnel FIRST, before reconcile_vpn possibly tears the tun
        // device down. Otherwise the still-running engine's read on the tun fd
        // fails with EBADFD ("File descriptor in bad state") as the device
        // vanishes, logging a spurious error on every disconnect.
        if let Some(child) = inner.tunnel.take() {
            // Reaping waits on the child (a blocking syscall); do it off the
            // async runtime so it can't stall a reactor worker.
            geph5_rt::spawn_blocking(move || platform::kill_child(child)).await;
        }
        inner.vpn.stop_transport();
        // First VPN bring-up is the last point where host DNS is available before
        // the kill switch/routes go up, so pre-resolve fronted broker sources now.
        let pre_resolve_broker_fronts =
            inner.settings.connected && inner.settings.vpn && !inner.vpn.is_active();
        let tunnel_config = if inner.settings.connected {
            // build_tunnel_config resolves fronted-broker DNS with a blocking
            // getaddrinfo; run it off the async runtime so a slow resolver can't
            // starve a reactor worker.
            let settings = inner.settings.clone();
            let cfg = geph5_rt::spawn_blocking(move || {
                supervisor::build_tunnel_config(&settings, pre_resolve_broker_fronts)
            })
            .await
            .map_err(|e| format!("{e:#}"))?;
            Some(cfg)
        } else {
            None
        };
        let service_user = reconcile_vpn(inner, force_vpn_rebuild)?;
        if !inner.settings.connected {
            return Ok(());
        }
        let tunnel_config = tunnel_config.expect("connected tunnel has a prepared config");
        let spawned = supervisor::spawn_tunnel(
            tunnel_config,
            service_user,
            inner.vpn.packet_mode(),
            inner.vpn.bind_indices(),
        )
        .map_err(|e| format!("{e:?}"))?;
        if let Err(error) = inner.vpn.attach_transport(spawned.transport) {
            platform::kill_child(spawned.child);
            return Err(format!("attaching VPN packet transport: {error:#}"));
        }
        inner.tunnel = Some(spawned.child);
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

/// Bring the VPN and engine service identity into line with the desired state.
fn reconcile_vpn(inner: &mut Inner, force_vpn_rebuild: bool) -> Result<Option<(u32, u32)>, String> {
    let want_vpn = inner.settings.connected && inner.settings.vpn;
    // The proxy-mode engine is network-facing too, so platforms with a service
    // account still resolve it even when the VPN itself is disabled.
    let service_user =
        platform::ensure_service_user().map_err(|e| format!("engine service user: {e:#}"))?;
    inner
        .vpn
        .reconcile(
            want_vpn,
            force_vpn_rebuild,
            inner.settings.allow_lan,
            service_user,
        )
        .map_err(|e| format!("vpn reconcile: {e:#}"))?;
    Ok(service_user)
}

/// Whether the settings ask for the system proxy to be auto-configured: only
/// meaningful when the local proxy is on at all.
fn wants_auto_proxy(settings: &Settings) -> bool {
    settings.proxy.as_ref().is_some_and(|p| p.autoconf)
}

/// Configure (or clear) the given session's system proxy off the reactor thread
/// (it may spawn a privilege-dropping helper). Best-effort: failures are logged.
async fn apply_proxy(session: Option<SessionContext>, connected: bool) -> bool {
    let url = format!("http://{}/proxy.pac", supervisor::PAC_ADDR);
    let res = geph5_rt::spawn_blocking(move || {
        platform::set_system_proxy(session.as_ref(), connected, &url)
    })
    .await;
    match res {
        Ok(()) => {
            tracing::info!(connected, "configured system proxy");
            true
        }
        Err(e) => {
            tracing::warn!(
                err = format!("{e:#}"),
                connected,
                "system proxy config failed"
            );
            false
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn account_info_from(info: UserInfo) -> AccountInfo {
    let is_plus = info
        .plus_expires_unix
        .map(|e| e > now_unix())
        .unwrap_or(false);
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
            let cleared = apply_proxy(Some(session.clone()), false).await;
            let mut inner = self.inner.lock().await;
            inner.proxy_active = !cleared;
            inner.proxy_session = (!cleared).then_some(session);
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
            let configured = apply_proxy(Some(session.clone()), true).await;
            if configured {
                let mut inner = self.inner.lock().await;
                inner.proxy_active = true;
                inner.proxy_session = Some(session);
            }
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
            let cleared = apply_proxy(Some(session.clone()), false).await;
            let mut inner = self.inner.lock().await;
            inner.proxy_active = !cleared;
            inner.proxy_session = (!cleared).then_some(session);
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
                let exit = sessions
                    .first()
                    .map(|s| exit_info_from(s.exit.country.alpha2().to_string(), &s.exit, None));
                (ConnState::Connected, exit)
            }
        };
        let total_rx_bytes = client
            .stat_num("total_rx_bytes".into())
            .await
            .unwrap_or(0.0);
        let total_tx_bytes = client
            .stat_num("total_tx_bytes".into())
            .await
            .unwrap_or(0.0);
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

    // Setters below only persist the new value. A settings change never mutates
    // the live tunnel/VPN/proxy state; it takes effect on the next `connect` or
    // `reconnect`. This keeps a single, well-defined moment where the tunnel,
    // the kill-switch firewall, and the system proxy are all (re)built together
    // from a consistent snapshot — avoiding mid-session states where e.g. the
    // firewall's allow_lan rules diverge from settings, or turning VPN off tears
    // down routing while the session still reports connected.

    async fn set_exit_constraint(
        &self,
        constraint: geph5_broker_protocol::ExitConstraint,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        tracing::debug!(
            ?constraint,
            "setting changed: exit_constraint (applies on next connect)"
        );
        inner.settings.exit_constraint = constraint;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        Ok(())
    }

    async fn set_proxy_settings(
        &self,
        proxy: Option<ProxySettings>,
        _session: SessionContext,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        if inner.settings.proxy != proxy {
            tracing::debug!(?proxy, "setting changed: proxy (applies on next connect)");
            inner.settings.proxy = proxy;
            inner.settings.save().map_err(|e| format!("{e:?}"))?;
        }
        Ok(())
    }

    async fn set_vpn_mode(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        tracing::debug!(enabled, "setting changed: vpn (applies on next connect)");
        inner.settings.vpn = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        Ok(())
    }

    async fn set_allow_lan(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        tracing::debug!(
            enabled,
            "setting changed: allow_lan (applies on next connect)"
        );
        inner.settings.allow_lan = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        Ok(())
    }

    async fn set_allow_direct(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        tracing::debug!(
            enabled,
            "setting changed: allow_direct (applies on next connect)"
        );
        inner.settings.allow_direct = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
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

/// Tear down everything the manager installed, so a Ctrl-C / `kill` cleanly restores
/// normal networking instead of leaving the fail-closed kill switch (and routes /
/// DNS) stranded with no engine to carry traffic. Mirrors the disconnected path of
/// `reconcile_vpn`.
async fn shutdown_teardown(inner: &Mutex<Inner>, teardown_lock: &Mutex<()>) {
    // A signal and a control-server failure can arrive together. Serialize the
    // two cleanup paths so neither can exit the process while the other still
    // owns live engine/VPN state.
    let _teardown = teardown_lock.lock().await;
    let (tunnel, query, proxy_active, proxy_session) = {
        let mut inner = inner.lock().await;
        inner.shutting_down = true;
        let tunnel = inner.tunnel.take();
        let query = inner.query.take();
        let proxy_active = std::mem::take(&mut inner.proxy_active);
        let proxy_session = inner.proxy_session.take();
        (tunnel, query, proxy_active, proxy_session)
    };
    if let Some(child) = tunnel {
        platform::kill_child(child);
    }
    if let Some(child) = query {
        platform::kill_child(child);
    }
    // Killing the Windows engine closes its packet pipe, allowing the pump's
    // reader thread to finish. Explicit VPN cleanup then joins the pump before
    // removing routes, DNS, and the kill switch.
    inner.lock().await.vpn.cleanup();
    if proxy_active {
        let _ = apply_proxy(proxy_session, false).await;
    }
}

/// Run the manager forever: spawn the children and serve the CLI control protocol.
pub async fn run_manager() -> anyhow::Result<()> {
    let manager = ManagerImpl::start().await?;
    let teardown_lock = std::sync::Arc::new(Mutex::new(()));

    // Graceful shutdown: on a termination signal, tear the VPN down and exit. Without
    // this, Ctrl-C would leave the kill switch / routes / DNS in place and strand the
    // machine offline until the manager is restarted.
    {
        let inner = manager.inner.clone();
        let teardown_lock = teardown_lock.clone();
        geph5_rt::spawn(async move {
            platform::shutdown_signal().await;
            tracing::warn!("termination signal received; tearing down manager state and exiting");
            shutdown_teardown(&inner, &teardown_lock).await;
            std::process::exit(0);
        })
        .detach();
    }

    // One monitor loop for all platforms. Native event/poll cadence and probe
    // details stay in vpn.rs; potentially blocking route inspection runs while
    // the manager mutex is not held.
    {
        let inner = manager.inner.clone();
        geph5_rt::spawn(async move {
            loop {
                vpn::wait_network_change().await;
                let probe = {
                    let inner = inner.lock().await;
                    if inner.shutting_down || !(inner.settings.connected && inner.settings.vpn) {
                        continue;
                    }
                    match inner.vpn.network_probe() {
                        Some(probe) => probe,
                        None => continue,
                    }
                };
                let checked = geph5_rt::spawn_blocking(move || vpn::check_network(probe)).await;
                let mut inner = inner.lock().await;
                if checked.generation != inner.vpn.generation()
                    || inner.shutting_down
                    || !(inner.settings.connected && inner.settings.vpn)
                {
                    continue;
                }
                match checked.action {
                    vpn::NetworkAction::Healthy => {}
                    vpn::NetworkAction::Respawn => {
                        tracing::warn!("VPN route repaired; respawning tunnel engine");
                        let _ = ManagerImpl::reconcile_tunnel(&mut inner).await;
                    }
                    vpn::NetworkAction::Rebuild => {
                        tracing::warn!("physical network changed; rebuilding VPN");
                        let _ = ManagerImpl::reconcile_tunnel_inner(&mut inner, true).await;
                    }
                }
            }
        })
        .detach();
    }

    let inner = manager.inner.clone();
    let result = platform::serve_manager(manager).await;
    tracing::warn!("manager control server exited; tearing down installed state");
    shutdown_teardown(&inner, &teardown_lock).await;
    result
}
