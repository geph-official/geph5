//! The control protocol spoken between clients (the `geph` CLI, desktop GUIs
//! such as gephgui-wry) and the `geph manager` supervisor (server), over a unix
//! domain socket (a named pipe on Windows).
//!
//! This is deliberately a *small, stable* surface of its own, distinct from
//! [`crate::client_control`]'s `ControlProtocol`: it adds
//! connect/disconnect/login/settings semantics that the supervisor implements by
//! spawning and restarting the child `geph5-client` process, while proxying
//! status/stats/exits/logs through to the child's own control protocol.

use async_trait::async_trait;
use geph5_broker_protocol::ExitConstraint;
use nanorpc::nanorpc_derive;
use serde::{Deserialize, Serialize};

/// Filesystem path of the manager's control socket.
#[cfg(target_os = "macos")]
pub const MANAGER_CONTROL_SOCK: &str = "/var/run/geph/control.sock";
/// Filesystem path of the manager's control socket.
#[cfg(all(unix, not(target_os = "macos")))]
pub const MANAGER_CONTROL_SOCK: &str = "/run/geph/control.sock";
/// Name of the manager's control named pipe.
#[cfg(windows)]
pub const MANAGER_CONTROL_PIPE: &str = r"\\.\pipe\geph-manager-control";

#[cfg(unix)]
pub type ManagerTransport = nanorpc_sillad::DialerTransport<sillad::unix::UnixDialer>;
#[cfg(windows)]
pub type ManagerTransport = nanorpc_sillad::DialerTransport<sillad::windows_pipe::NamedPipeDialer>;

/// A nanorpc transport that dials the manager's control endpoint. Each call
/// dials a fresh connection (no pooling). Requires the geph5-rt tokio runtime
/// to be driving the calling task.
pub fn manager_control_transport() -> ManagerTransport {
    #[cfg(unix)]
    {
        nanorpc_sillad::DialerTransport(sillad::unix::UnixDialer {
            path: MANAGER_CONTROL_SOCK.into(),
        })
    }
    #[cfg(windows)]
    {
        nanorpc_sillad::DialerTransport(sillad::windows_pipe::NamedPipeDialer {
            name: MANAGER_CONTROL_PIPE.to_string(),
        })
    }
}

/// A typed client pointed at the running manager's control endpoint.
pub fn manager_control_client() -> GephCtlClient {
    GephCtlClient::from(manager_control_transport())
}

/// High-level connection state, mirrored from the child's `ConnInfo`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    /// No tunnel desired (child is in dry-run mode).
    Disconnected,
    /// Tunnel desired but no session has come up yet.
    Connecting,
    /// At least one session is live.
    Connected,
}

/// A single exit, flattened for display.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExitInfo {
    /// Stable key/hostname of the exit in the net-status map.
    pub hostname: String,
    pub country: String,
    pub city: String,
    pub load: f32,
    /// Whether free accounts may use this exit.
    pub allows_free: bool,
}

/// Account information surfaced to the CLI.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AccountInfo {
    pub user_id: u64,
    /// "free" or "plus".
    pub level: String,
    pub plus_expires_unix: Option<u64>,
    /// Megabytes used / limit this period, if metered.
    pub bw_used_mb: Option<u32>,
    pub bw_limit_mb: Option<u32>,
}

/// Current manager status.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Status {
    pub state: ConnState,
    /// The exit we are connected through, if any.
    pub exit: Option<ExitInfo>,
    pub total_rx_bytes: f64,
    pub total_tx_bytes: f64,
}

/// The calling client's desktop session, so the (possibly root) manager knows
/// *whose* system proxy to configure. The proxy-setting code lives only in the
/// manager; clients merely forward their identity — for the CLI that's the uid it
/// runs as plus a few environment variables, no proxy logic of their own.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SessionContext {
    /// The user id whose session proxy should be configured.
    #[serde(default)]
    pub uid: u32,
    /// Primary gid; the manager derives it from the uid when absent.
    #[serde(default)]
    pub gid: Option<u32>,
    /// Home directory; derived from the uid when absent.
    #[serde(default)]
    pub home: Option<String>,
    /// D-Bus session bus address; defaults to `/run/user/<uid>/bus`.
    #[serde(default)]
    pub dbus_session_bus_address: Option<String>,
    /// XDG runtime dir; defaults to `/run/user/<uid>`.
    #[serde(default)]
    pub xdg_runtime_dir: Option<String>,
}

/// Local-proxy configuration. `None` at the settings level means no local proxy
/// listeners at all — the engine binds no ports.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ProxySettings {
    /// Whether to point the desktop's system proxy at the tunnel while connected.
    pub autoconf: bool,
    /// Bind the proxies on all interfaces (0.0.0.0) instead of loopback.
    pub listen_all: bool,
    /// SOCKS5 proxy port.
    pub socks5_port: u16,
    /// HTTP proxy port.
    pub http_port: u16,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            autoconf: true,
            listen_all: false,
            socks5_port: 9909,
            http_port: 9910,
        }
    }
}

/// Complete desired configuration for the credentialed tunnel engine. Clients
/// submit this as one coherent snapshot; the manager persists it and, when
/// connected, reconciles the live engine and host networking exactly once.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TunnelSettings {
    pub exit_constraint: ExitConstraint,
    /// Local-proxy configuration; `None` means no local proxy listeners.
    pub proxy: Option<ProxySettings>,
    /// Whether full-tunnel VPN mode is enabled.
    pub vpn: bool,
    /// Whether private/LAN addresses bypass the tunnel.
    pub allow_lan: bool,
    /// Whether direct (non-bridge) connections to exits are allowed.
    pub allow_direct: bool,
    /// Whether destinations in mainland China bypass the tunnel.
    #[serde(default)]
    pub passthrough_china: bool,
    /// Metadata attached to newly-created exit sessions.
    #[serde(default)]
    pub session_metadata: serde_json::Value,
}

/// Persisted settings, as exposed to clients.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SettingsView {
    pub logged_in: bool,
    pub exit_constraint: ExitConstraint,
    /// Whether the user wants the tunnel up.
    pub connected: bool,
    /// Local-proxy configuration; `None` means no local proxy listeners.
    pub proxy: Option<ProxySettings>,
    /// Whether full-tunnel VPN mode is enabled.
    pub vpn: bool,
    /// Whether private/LAN addresses bypass the tunnel.
    pub allow_lan: bool,
    /// Whether direct (non-bridge) connections to exits are allowed.
    pub allow_direct: bool,
    /// Whether destinations in mainland China bypass the tunnel.
    pub passthrough_china: bool,
    /// Metadata attached to newly-created exit sessions.
    pub session_metadata: serde_json::Value,
}

impl SettingsView {
    /// Return the mutable tunnel-settings portion of this view, suitable for a
    /// CLI read-modify-apply operation.
    pub fn tunnel_settings(&self) -> TunnelSettings {
        TunnelSettings {
            exit_constraint: self.exit_constraint.clone(),
            proxy: self.proxy.clone(),
            vpn: self.vpn,
            allow_lan: self.allow_lan,
            allow_direct: self.allow_direct,
            passthrough_china: self.passthrough_china,
            session_metadata: self.session_metadata.clone(),
        }
    }
}

#[nanorpc_derive]
#[async_trait]
pub trait GephCtlProtocol {
    /// Validate a secret, persist it, and (re)start the child with it.
    async fn login(&self, secret: String) -> Result<AccountInfo, String>;
    /// Persist a secret WITHOUT validating it against the broker, restarting the
    /// child with it only if already connected. Unlike `login`, this makes no
    /// network call, so it never blocks on a slow or dead network. Callers that
    /// have already validated the secret elsewhere (the GUI checks it at its
    /// login screen) use this on the connect path instead of re-validating every
    /// time; the tunnel engine authenticates the secret itself as it connects,
    /// so a bad secret still surfaces as an ordinary connection failure.
    async fn set_secret(&self, secret: String) -> Result<(), String>;
    /// Forget the stored secret, drop back to a logged-out child, and (if
    /// auto-proxy is on) clear the caller's system proxy.
    async fn logout(&self, session: SessionContext) -> Result<(), String>;
    /// Account info for the currently stored secret.
    async fn account(&self) -> Result<AccountInfo, String>;

    /// Bring the tunnel up and, if auto-proxy is on, point `session`'s system
    /// proxy at the tunnel.
    async fn connect(&self, session: SessionContext) -> Result<(), String>;
    /// Tear the tunnel down and, if auto-proxy is on, clear `session`'s proxy.
    async fn disconnect(&self, session: SessionContext) -> Result<(), String>;

    /// Re-establish the tunnel with the current settings WITHOUT a leak window:
    /// in VPN mode the tun device and kill switch stay up the whole time while
    /// only the engine child is restarted. Used for "reconnect" and for applying
    /// a new exit while connected. Errors if not currently connected.
    async fn reconnect(&self, session: SessionContext) -> Result<(), String>;

    /// Current connection status.
    async fn status(&self) -> Result<Status, String>;

    /// Read persisted settings.
    async fn get_settings(&self) -> Result<SettingsView, String>;
    /// Persist a complete settings snapshot and, when connected, immediately
    /// run one full idempotent reconciliation using it.
    async fn apply_settings(
        &self,
        settings: TunnelSettings,
        session: SessionContext,
    ) -> Result<(), String>;

    /// List available exits from the broker.
    async fn list_exits(&self) -> Result<Vec<ExitInfo>, String>;

    /// Most recent `count` log lines from the child.
    async fn logs(&self, count: usize) -> Result<Vec<String>, String>;

    /// Escape hatch: forward a raw JSON-RPC call to the control protocol of the
    /// underlying geph5-client — the "daemon" this method is named for
    /// (`conn_info`, `stat_num`, `stat_history`, `net_status`, `recent_logs`,
    /// `broker_rpc`, `start_registration`, …). Richer clients such as the GUI
    /// use this to reach the full engine surface without every client
    /// reimplementing it.
    async fn daemon_rpc(
        &self,
        method: String,
        params: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, String>;
}
