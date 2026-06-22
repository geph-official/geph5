//! The `geph daemon` supervisor: owns persisted settings and a single child
//! `geph5-client` process, and implements the `GephCtlProtocol` the CLI talks to.

use std::{
    process::Child,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use geph5_broker_protocol::{Credential, UserInfo};
use geph5_misc_rpc::client_control::{ConnInfo, ControlClient};
use nanorpc::RpcTransport;
use smol::lock::Mutex;

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
    child: Option<Child>,
    /// Live full-tunnel VPN state (tun fd + routing/kill-switch), when connected
    /// in VPN mode. Held here so it survives child restarts.
    vpn: Option<vpn_linux::VpnHandle>,
}

pub struct DaemonImpl {
    inner: Mutex<Inner>,
}

impl DaemonImpl {
    /// Load settings, spawn the initial child, and return the daemon.
    pub async fn start() -> anyhow::Result<Self> {
        let settings = Settings::load()?;
        let this = DaemonImpl {
            inner: Mutex::new(Inner {
                settings,
                child: None,
                vpn: None,
            }),
        };
        // Reconcile the persisted state (sets up the tunnel/kill-switch if the
        // user was connected in VPN mode). If that fails, fall back to a clean
        // disconnected state rather than refusing to start.
        let mut inner = this.inner.lock().await;
        if let Err(e) = this.restart_child(&mut inner).await {
            tracing::warn!(err = %e, "could not restore persisted state; starting disconnected");
            inner.settings.connected = false;
            let _ = inner.settings.save();
            let _ = this.restart_child(&mut inner).await;
        }
        drop(inner);
        Ok(this)
    }

    /// Kill the old child, reconcile the VPN tunnel/kill-switch with desired
    /// state, then spawn a fresh child (as the service user, with the tun fd in
    /// VPN mode).
    async fn restart_child(&self, inner: &mut Inner) -> Result<(), String> {
        // Kill the child FIRST, before reconcile_vpn possibly tears the tun
        // device down. Otherwise the still-running child's read on the tun fd
        // fails with EBADFD ("File descriptor in bad state") as the device
        // vanishes, logging a spurious error on every disconnect.
        if let Some(child) = inner.child.take() {
            supervisor::kill_child(child);
        }
        let service_user = reconcile_vpn(inner)?;
        let vpn_fd = inner.vpn.as_ref().map(|h| h.tun_fd());
        let child = supervisor::spawn_child(&inner.settings, service_user, vpn_fd)
            .map_err(|e| format!("{e:?}"))?;
        inner.child = Some(child);
        supervisor::wait_child_ready(CHILD_READY_TIMEOUT)
            .await
            .map_err(|e| format!("{e:?}"))
    }

    /// Ask the broker (via the child) for the account behind a secret.
    async fn account_for_secret(&self, secret: &str) -> Result<AccountInfo, String> {
        let cred = Credential::Secret(secret.to_string());
        let params = vec![serde_json::to_value(&cred).map_err(|e| e.to_string())?];
        let raw = supervisor::child_control()
            .broker_rpc("get_user_info_by_cred".into(), params)
            .await
            .map_err(|e| format!("could not reach broker: {e:?}"))??;
        let info: Option<UserInfo> = serde_json::from_value(raw).map_err(|e| e.to_string())?;
        let info = info.ok_or_else(|| "incorrect secret".to_string())?;
        Ok(account_info_from(info))
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
    let res = smol::unblock(move || proxy::apply_for_session(&session, connected, &url)).await;
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
        // Only restart if connected; a disconnected (dry-run) child doesn't use
        // the stored secret, and the next connect() picks it up.
        if inner.settings.connected {
            self.restart_child(&mut inner).await?;
        }
        Ok(account)
    }

    async fn logout(&self, session: SessionContext) -> Result<(), String> {
        let auto_proxy = {
            let mut inner = self.inner.lock().await;
            inner.settings.secret = None;
            inner.settings.connected = false;
            inner.settings.save().map_err(|e| format!("{e:?}"))?;
            self.restart_child(&mut inner).await?;
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
            self.restart_child(&mut inner).await?;
            // In full-tunnel VPN mode the proxy is redundant (everything is
            // already routed), so only auto-configure it in proxy mode.
            inner.settings.auto_proxy && !inner.settings.vpn
        };
        if apply {
            apply_proxy(session, true).await;
        }
        Ok(())
    }

    async fn disconnect(&self, session: SessionContext) -> Result<(), String> {
        let auto_proxy = {
            let mut inner = self.inner.lock().await;
            inner.settings.connected = false;
            inner.settings.save().map_err(|e| format!("{e:?}"))?;
            self.restart_child(&mut inner).await?;
            inner.settings.auto_proxy
        };
        if auto_proxy {
            apply_proxy(session, false).await;
        }
        Ok(())
    }

    async fn status(&self) -> Result<Status, String> {
        let client: ControlClient = supervisor::child_control();
        let conn = client
            .conn_info()
            .await
            .map_err(|e| format!("could not reach child: {e:?}"))?;
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
            self.restart_child(&mut inner).await?;
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
        // restart_child reconciles the VPN tunnel/kill-switch with settings.vpn.
        if inner.settings.connected {
            self.restart_child(&mut inner).await?;
        }
        Ok(())
    }

    async fn set_allow_lan(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.settings.allow_lan = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        if inner.settings.connected {
            self.restart_child(&mut inner).await?;
        }
        Ok(())
    }

    async fn set_allow_direct(&self, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.settings.allow_direct = enabled;
        inner.settings.save().map_err(|e| format!("{e:?}"))?;
        if inner.settings.connected {
            self.restart_child(&mut inner).await?;
        }
        Ok(())
    }

    async fn list_exits(&self) -> Result<Vec<ExitInfo>, String> {
        let net_status = supervisor::child_control()
            .net_status()
            .await
            .map_err(|e| format!("could not reach child: {e:?}"))??;
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
        let mut logs = supervisor::child_control()
            .recent_logs()
            .await
            .map_err(|e| format!("could not reach child: {e:?}"))?;
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
        let resp = supervisor::child_control()
            .0
            .call_raw(req)
            .await
            .map_err(|e| format!("could not reach child: {e:?}"))?;
        match resp.error {
            Some(err) => Err(err.message),
            None => Ok(resp.result.unwrap_or(serde_json::Value::Null)),
        }
    }
}

/// Run the daemon forever: spawn the child and serve the CLI control protocol.
pub async fn run_daemon() -> anyhow::Result<()> {
    let daemon = DaemonImpl::start().await?;
    let addr = supervisor::daemon_control_addr();
    tracing::info!(%addr, "geph daemon listening for CLI connections");
    let listener = sillad::tcp::TcpListener::bind(addr).await?;
    nanorpc_sillad::rpc_serve(listener, GephCtlService(daemon)).await?;
    Ok(())
}
