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
mod service;
mod supervisor;
#[cfg(not(windows))]
mod vpn_linux;
#[cfg(windows)]
mod vpn_windows;

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
        // Background (un)registration runs directly, without contacting the
        // daemon; it installs the autostart (systemd unit on Linux, a boot-time
        // scheduled task on Windows), so it needs root/Administrator like the daemon.
        Command::RegisterDaemon => {
            require_root();
            service::register()
        }
        Command::UnregisterDaemon => {
            require_root();
            service::unregister()
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
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("geph=debug,warn")),
        )
        .try_init();
}

#[cfg(unix)]
fn require_root() {
    // SAFETY: geteuid is always safe to call.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        eprintln!("this command must be run as root (try: sudo ...)");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn require_root() {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // VPN mode needs Administrator for WinTUN, routing, and WFP. A Windows
    // service running as LocalSystem is the eventual model; for now we require
    // the daemon to be launched elevated and bail clearly otherwise.
    let elevated = unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            false
        } else {
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut ret_len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            );
            CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    };
    if !elevated {
        eprintln!("this command must be run as Administrator");
        std::process::exit(1);
    }
}

#[cfg(not(any(unix, windows)))]
fn require_root() {}
