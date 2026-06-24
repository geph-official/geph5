//! The `geph daemon` supervisor: owns persisted settings and two child
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
        AccountInfo, ConnState, ExitInfo, GephCtlProtocol, GephCtlService, SessionContext,
        SettingsView, Status,
    },
    proxy,
    supervisor::{self, Settings},
    vpn_linux,
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
    /// Live full-tunnel VPN state (tun fd + routing/kill-switch), when connected
    /// in VPN mode. Held here so it survives tunnel-engine restarts.
    vpn: Option<vpn_linux::VpnHandle>,
}

pub struct DaemonImpl {
    inner: Mutex<Inner>,
}

impl DaemonImpl {
    /// Load settings, spawn the permanent query engine and (if persisted as
    /// connected) the tunnel engine, then return the daemon.
    pub async fn start() -> anyhow::Result<Self> {
        let settings = Settings::load()?;
        let this = DaemonImpl {
            inner: Mutex::new(Inner {
                settings,
                query: None,
                tunnel: None,
                vpn: None,
            }),
        };
        let mut inner = this.inner.lock().await;
        // The query engine must be up for broker queries; bring it up first.
        // Non-fatal if it fails — the daemon still serves settings/connect, and
        // query calls will surface a clear error until it recovers.
        if let Err(e) = this.ensure_query_engine(&mut inner).await {
            tracing::warn!(err = %e, "query engine failed to start; broker queries unavailable");
        }
        // Restore connection state (spawns the tunnel + VPN kill-switch if the
        // user was connected). If that fails, fall back to a clean disconnected
        // state rather than refusing to start.
        if let Err(e) = this.reconcile_tunnel(&mut inner).await {
            tracing::warn!(err = %e, "could not restore persisted state; starting disconnected");
            inner.settings.connected = false;
            let _ = inner.settings.save();
            let _ = this.reconcile_tunnel(&mut inner).await;
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
        // traffic is unmarked and the VPN kill switch lets it out.
        let service_user = vpn_linux::ensure_service_user().ok();
        let child = supervisor::spawn_query(service_user).map_err(|e| format!("{e:?}"))?;
        inner.query = Some(child);
        supervisor::wait_control_ready(&supervisor::query_control(), CHILD_READY_TIMEOUT)
            .await
            .map_err(|e| format!("{e:?}"))
    }

    /// Bring the **tunnel** engine into line with `settings.connected`: kill any
    /// existing one, reconcile the VPN tunnel/kill-switch, then (if connected)
    /// spawn a fresh tunnel engine. The query engine is untouched.
    async fn reconcile_tunnel(&self, inner: &mut Inner) -> Result<(), String> {
        // Kill the tunnel FIRST, before reconcile_vpn possibly tears the tun
        // device down. Otherwise the still-running engine's read on the tun fd
        // fails with EBADFD ("File descriptor in bad state") as the device
        // vanishes, logging a spurious error on every disconnect.
        if let Some(child) = inner.tunnel.take() {
            supervisor::kill_child(child);
        }
        let service_user = reconcile_vpn(inner)?;
        if !inner.settings.connected {
            return Ok(());
        }
        let vpn_fd = inner.vpn.as_ref().map(|h| h.tun_fd());
        let child = supervisor::spawn_tunnel(&inner.settings, service_user, vpn_fd)
            .map_err(|e| format!("{e:?}"))?;
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
/// (`connected && vpn`) and resolve the service user the child runs as.
fn reconcile_vpn(inner: &mut Inner) -> Result<Option<(u32, u32)>, String> {
    let want_vpn = inner.settings.connected && inner.settings.vpn;
    if want_vpn {
        let (uid, gid) =
            vpn_linux::ensure_service_user().map_err(|e| format!("vpn service user: {e:#}"))?;
        if inner.vpn.is_none() {
            inner.vpn = Some(vpn_linux::setup(uid).map_err(|e| format!("vpn setup: {e:#}"))?);
        }
        Ok(Some((uid, gid)))
    } else {
        // Tear down the tunnel/kill-switch when not in active VPN mode so normal
        // (disconnected or proxy-mode) connectivity is restored.
        if inner.vpn.take().is_some() {
            vpn_linux::teardown();
        }
        // Best-effort: still run the child unprivileged in proxy mode (else root).
        Ok(vpn_linux::ensure_service_user().ok())
    }
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
impl GephCtlProtocol for DaemonImpl {
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
            self.reconcile_tunnel(&mut inner).await?;
        }
        Ok(account)
    }

    async fn logout(&self, session: SessionContext) -> Result<(), String> {
        let auto_proxy = {
            let mut inner = self.inner.lock().await;
            inner.settings.secret = None;
            inner.settings.connected = false;
            inner.settings.save().map_err(|e| format!("{e:?}"))?;
            self.reconcile_tunnel(&mut inner).await?;
            inner.settings.auto_proxy
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
            self.reconcile_tunnel(&mut inner).await?;
            // In full-tunnel VPN mode the proxy is redundant (everything is
            // already routed), so only auto-configure it in proxy mode.
            inner.settings.auto_proxy && !inner.settings.vpn
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
        self.reconcile_tunnel(&mut inner).await
    }

    async fn disconnect(&self, session: SessionContext) -> Result<(), String> {
        let auto_proxy = {
            let mut inner = self.inner.lock().await;
            inner.settings.connected = false;
            inner.settings.save().map_err(|e| format!("{e:?}"))?;
            self.reconcile_tunnel(&mut inner).await?;
            inner.settings.auto_proxy
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
            auto_proxy: inner.settings.auto_proxy,
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
        inner.settings.exit_constraint = constraint;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        // Only the connected child needs restarting; a disconnected one rebuilds
        // from settings on the next connect().
        if inner.settings.connected {
            self.reconcile_tunnel(&mut inner).await?;
        }
        Ok(())
    }

    async fn set_auto_proxy(&self, enabled: bool, session: SessionContext) -> Result<(), String> {
        let connected = {
            let mut inner = self.inner.lock().await;
            inner.settings.auto_proxy = enabled;
            inner.settings.save().map_err(|e| format!("{e:?}"))?;
            inner.settings.connected
        };
        // Reflect the change in the live proxy if we're connected: enabling sets
        // it for the caller's session, disabling clears it.
        if connected {
            apply_proxy(session, enabled).await;
        }
        Ok(())
    }

    async fn set_vpn_mode(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.settings.vpn = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        // reconcile_tunnel reconciles the VPN tunnel/kill-switch with settings.vpn.
        if inner.settings.connected {
            self.reconcile_tunnel(&mut inner).await?;
        }
        Ok(())
    }

    async fn set_allow_lan(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.settings.allow_lan = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        if inner.settings.connected {
            self.reconcile_tunnel(&mut inner).await?;
        }
        Ok(())
    }

    async fn set_allow_direct(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.settings.allow_direct = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        if inner.settings.connected {
            self.reconcile_tunnel(&mut inner).await?;
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

/// Run the daemon forever: spawn the child and serve the CLI control protocol.
pub async fn run_daemon() -> anyhow::Result<()> {
    let daemon = DaemonImpl::start().await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = supervisor::daemon_control_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let listener = sillad::unix::UnixListener::bind(&path).await?; // unlinks any stale socket
        // The CLI/GUI run unprivileged; let them connect to the root daemon's socket.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));
        tracing::info!(path = %path.display(), "geph daemon listening for clients");
        nanorpc_sillad::rpc_serve(listener, GephCtlService(daemon)).await?;
    }
    #[cfg(windows)]
    {
        // The CLI/GUI run unprivileged but the daemon may run as a service; the
        // SDDL grants authenticated users access (the pipe analogue of chmod 0666).
        let name = supervisor::DAEMON_CONTROL_PIPE;
        let listener = sillad::windows_pipe::NamedPipeListener::bind(
            name,
            Some(sillad::windows_pipe::SDDL_ALLOW_AUTHENTICATED),
        )?;
        tracing::info!(name, "geph daemon listening for clients");
        nanorpc_sillad::rpc_serve(listener, GephCtlService(daemon)).await?;
    }
    Ok(())
}
