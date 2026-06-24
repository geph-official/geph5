//! `geph` — a Mullvad-like command-line client for Geph5.
//!
//! `geph daemon` runs the privileged supervisor that owns a child `geph5-client`
//! process; all other subcommands are thin clients that talk to it over loopback
//! TCP.

mod cli;
mod client;
mod daemon;
mod paths;
mod protocol;
mod proxy;
mod supervisor;
mod vpn_linux;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => {
            init_daemon_logging();
            require_root();
            geph5_rt::block_on(daemon::run_daemon())
        }
        // Internal helper: the daemon re-invokes us dropped to the desktop user
        // to apply proxy settings. Runs directly, without contacting the daemon.
        Command::ApplyProxy { mode, url } => {
            let connected = matches!(mode.as_str(), "on" | "true" | "1");
            proxy::apply_in_process(connected, url.as_deref().unwrap_or_default())
        }
        other => client::run(other),
    }
}

fn init_daemon_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("geph=info,warn")),
        )
        .try_init();
}

#[cfg(unix)]
fn require_root() {
    // SAFETY: geteuid is always safe to call.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        eprintln!("geph5 daemon must be run as root (try: sudo geph5 daemon)");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn require_root() {
    // On Windows, elevation is handled by the service manager / installer.
}
