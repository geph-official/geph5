//! `clap` command-line surface for the `geph` binary.

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "geph5", version, about = "Geph5 command-line client")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the privileged supervising daemon (must be run as root).
    Daemon,

    /// Register the daemon to run in the background across logins and reboots
    /// (systemd on Linux, a boot-time scheduled task on Windows; must be run as
    /// root/Administrator).
    RegisterDaemon,
    /// Remove the daemon's background registration (must be run as
    /// root/Administrator).
    UnregisterDaemon,

    /// Log in with an account secret. Reads from stdin if omitted.
    Login {
        /// The account secret.
        secret: Option<String>,
    },
    /// Log out and forget the stored secret.
    Logout,
    /// Show account information.
    Account,

    /// Bring the tunnel up.
    Connect,
    /// Tear the tunnel down.
    Disconnect,
    /// Re-establish the tunnel with current settings, without a leak window.
    Reconnect,
    /// Show connection status.
    Status,

    /// View or change the exit constraint.
    #[command(subcommand)]
    ExitConstraint(ExitConstraintCmd),

    /// List available exits.
    Exits,

    /// Show or set whether the local SOCKS5/HTTP proxies are enabled at all.
    Proxy {
        /// `on` or `off`. Omit to show the current setting.
        state: Option<String>,
    },

    /// Show or set whether the system proxy is auto-configured while connected.
    AutoProxy {
        /// `on` or `off`. Omit to show the current setting.
        state: Option<String>,
    },

    /// Show or set full-tunnel VPN mode.
    Vpn {
        /// `on` or `off`. Omit to show the current setting.
        state: Option<String>,
    },

    /// Show or set whether private/LAN addresses bypass the tunnel.
    LanAccess {
        /// `on` or `off`. Omit to show the current setting.
        state: Option<String>,
    },

    /// Show or set whether direct (non-bridge) connections to exits are allowed.
    AllowDirect {
        /// `on` or `off`. Omit to show the current setting.
        state: Option<String>,
    },

    /// Show recent daemon logs.
    Logs {
        /// Number of log lines to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        count: usize,
    },

    /// Internal: apply system proxy settings for the current user. The daemon
    /// re-invokes this dropped to the desktop user; not for direct use.
    #[command(name = "__apply-proxy", hide = true)]
    ApplyProxy {
        /// `on` or `off`.
        mode: String,
        /// PAC URL (required for `on`).
        url: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ExitConstraintCmd {
    /// Show the current exit constraint.
    Get,
    /// Set the exit constraint. With no flags, picks automatically.
    Set(ExitConstraintSet),
}

#[derive(Args)]
pub struct ExitConstraintSet {
    /// Two-letter country code, e.g. "us". Omit for automatic selection.
    #[arg(long)]
    pub country: Option<String>,
    /// City name (requires --country).
    #[arg(long, requires = "country")]
    pub city: Option<String>,
}
