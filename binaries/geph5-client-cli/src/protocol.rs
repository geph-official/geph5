//! The control protocol spoken between the `geph` CLI (client) and the
//! `geph daemon` supervisor (server), over loopback TCP.
//!
//! This is deliberately a *small, stable* surface of its own, distinct from
//! `geph5-client`'s `ControlProtocol`: it adds connect/disconnect/login/settings
//! semantics that the supervisor implements by spawning and restarting the child
//! `geph5-client` process, while proxying status/stats/exits/logs through to the
//! child's own control protocol.

use async_trait::async_trait;
use geph5_broker_protocol::ExitConstraint;
use nanorpc::nanorpc_derive;
use serde::{Deserialize, Serialize};

/// Default fixed loopback port for the daemon <-> CLI control channel.
pub const DAEMON_CONTROL_PORT: u16 = 28080;

/// Default fixed loopback port the child geph5-client listens on for its own
/// control protocol; only the supervisor talks to it.
pub const CHILD_CONTROL_PORT: u16 = 28081;

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

/// Current daemon status.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Status {
    pub state: ConnState,
    /// The exit we are connected through, if any.
    pub exit: Option<ExitInfo>,
    pub total_rx_bytes: f64,
    pub total_tx_bytes: f64,
}

/// The calling client's desktop session, so the (possibly root) daemon knows
/// *whose* system proxy to configure. The proxy-setting code lives only in the
/// daemon; clients merely forward their identity — for the CLI that's the uid it
/// runs as plus a few environment variables, no proxy logic of their own.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SessionContext {
    /// The user id whose session proxy should be configured.
    #[serde(default)]
    pub uid: u32,
    /// Primary gid; the daemon derives it from the uid when absent.
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

/// Persisted settings, as exposed to the CLI.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SettingsView {
    pub logged_in: bool,
    pub exit_constraint: ExitConstraint,
    /// Whether the user wants the tunnel up.
    pub connected: bool,
    /// Whether the system proxy is auto-configured while connected.
    pub auto_proxy: bool,
    /// Whether full-tunnel VPN mode is enabled.
    pub vpn: bool,
    /// Whether private/LAN addresses bypass the tunnel.
    pub allow_lan: bool,
    /// Whether direct (non-bridge) connections to exits are allowed.
    pub allow_direct: bool,
}

#[nanorpc_derive]
#[async_trait]
pub trait GephCtlProtocol {
    /// Validate a secret, persist it, and (re)start the child with it.
    async fn login(&self, secret: String) -> Result<AccountInfo, String>;
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
    async fn reconnect(&self) -> Result<(), String>;

    /// Current connection status.
    async fn status(&self) -> Result<Status, String>;

    /// Read persisted settings.
    async fn get_settings(&self) -> Result<SettingsView, String>;
    /// Change the exit constraint (restart child if currently connected).
    async fn set_exit_constraint(&self, constraint: ExitConstraint) -> Result<(), String>;

    /// Enable or disable the auto-proxy preference. If currently connected, the
    /// change is applied to `session`'s system proxy immediately.
    async fn set_auto_proxy(&self, enabled: bool, session: SessionContext) -> Result<(), String>;

    /// Enable or disable full-tunnel VPN mode (Linux). Restarts the tunnel if
    /// currently connected.
    async fn set_vpn_mode(&self, enabled: bool) -> Result<(), String>;

    /// Enable or disable LAN passthrough. Restarts the tunnel if connected.
    async fn set_allow_lan(&self, enabled: bool) -> Result<(), String>;

    /// Allow or forbid direct (non-bridge) connections to exits. Restarts the
    /// tunnel if connected.
    async fn set_allow_direct(&self, enabled: bool) -> Result<(), String>;

    /// List available exits from the broker.
    async fn list_exits(&self) -> Result<Vec<ExitInfo>, String>;

    /// Most recent `count` log lines from the child.
    async fn logs(&self, count: usize) -> Result<Vec<String>, String>;

    /// Escape hatch: forward a raw JSON-RPC call to the underlying geph5-client
    /// control protocol (`conn_info`, `stat_num`, `stat_history`, `net_status`,
    /// `recent_logs`, `broker_rpc`, `start_registration`, …). Richer clients
    /// such as the GUI use this to reach the full engine surface without every
    /// client reimplementing it.
    async fn daemon_rpc(
        &self,
        method: String,
        params: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, String>;
}
