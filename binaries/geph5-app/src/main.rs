//! `geph` — a Mullvad-like command-line client for Geph5.
//!
//! `geph manager` runs the privileged supervisor that owns a child `geph5-client`
//! process; all other subcommands are thin clients that talk to it over loopback
//! TCP.

mod cli;
mod client;
mod manager;
mod platform;
mod supervisor;
mod vpn;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Manager => {
            init_manager_logging();
            platform::require_manager_privilege()?;
            geph5_rt::block_on(manager::run_manager())
        }
        // Background (un)registration runs directly, without contacting the
        // manager; it installs the platform autostart service, so it needs
        // root/Administrator like the manager.
        Command::RegisterManager => {
            platform::require_manager_privilege()?;
            platform::register_manager()
        }
        Command::UnregisterManager => {
            platform::require_manager_privilege()?;
            platform::unregister_manager()
        }
        // Internal helper: the manager re-invokes us dropped to the desktop user
        // to apply proxy settings. Runs directly, without contacting the manager.
        Command::ApplyProxy { mode, url } => {
            let connected = matches!(mode.as_str(), "on" | "true" | "1");
            platform::apply_proxy_in_process(connected, url.as_deref().unwrap_or_default())
        }
        other => client::run(other),
    }
}

fn init_manager_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("geph=debug,warn")),
        )
        .try_init();
}
