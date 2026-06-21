//! CLI-side: connect to the `geph daemon`, issue one `GephCtlProtocol` call, and
//! render the result for a human.

use anyhow::Context as _;
use geph5_broker_protocol::ExitConstraint;
use isocountry::CountryCode;

use crate::{
    cli::{Command, ExitConstraintCmd, ExitConstraintSet},
    protocol::{AccountInfo, ConnState, GephCtlClient, GephCtlError, SessionContext, Status},
    supervisor::daemon_control_addr,
};

/// Build a client pointed at the running daemon.
fn daemon_client() -> GephCtlClient {
    GephCtlClient::from(nanorpc_sillad::DialerTransport(sillad::tcp::TcpDialer {
        dest_addr: daemon_control_addr(),
    }))
}

/// Flatten the double-Result that RPC methods return, turning a transport-level
/// failure into a friendly "is the daemon running?" message.
fn flatten<T>(r: Result<Result<T, String>, GephCtlError<anyhow::Error>>) -> anyhow::Result<T> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(msg)) => Err(anyhow::anyhow!(msg)),
        Err(GephCtlError::Transport(_)) => Err(anyhow::anyhow!(
            "could not reach the geph daemon — is it running? start it with `sudo geph daemon`"
        )),
        Err(e) => Err(anyhow::anyhow!("rpc error: {e:?}")),
    }
}

/// Entry point for all non-daemon subcommands.
pub fn run(command: Command) -> anyhow::Result<()> {
    smolscale::block_on(run_inner(command))
}

async fn run_inner(command: Command) -> anyhow::Result<()> {
    let client = daemon_client();
    match command {
        Command::Daemon | Command::ApplyProxy { .. } => unreachable!("handled in main"),

        Command::Login { secret } => {
            let secret = match secret {
                Some(s) => s,
                None => prompt_secret()?,
            };
            let account = flatten(client.login(secret.trim().to_string()).await)?;
            println!("Logged in.");
            print_account(&account);
        }
        Command::Logout => {
            flatten(client.logout(current_session()).await)?;
            println!("Logged out.");
        }
        Command::Account => {
            let account = flatten(client.account().await)?;
            print_account(&account);
        }

        Command::Connect => {
            // Pass our session so the daemon can configure this user's system
            // proxy; the proxy-setting code lives entirely in the daemon.
            flatten(client.connect(current_session()).await)?;
            println!("Connecting…");
        }
        Command::Disconnect => {
            flatten(client.disconnect(current_session()).await)?;
            println!("Disconnected.");
        }
        Command::Status => {
            let status = flatten(client.status().await)?;
            print_status(&status);
            let settings = flatten(client.get_settings().await)?;
            println!(
                "Mode:  {} | lan-access {} | auto-proxy {}",
                if settings.vpn { "vpn" } else { "proxy" },
                on_off(settings.allow_lan),
                on_off(settings.auto_proxy),
            );
        }

        Command::ExitConstraint(ExitConstraintCmd::Get) => {
            let settings = flatten(client.get_settings().await)?;
            println!("{}", render_constraint(&settings.exit_constraint));
        }
        Command::ExitConstraint(ExitConstraintCmd::Set(set)) => {
            let constraint = constraint_from_args(&set)?;
            flatten(client.set_exit_constraint(constraint.clone()).await)?;
            println!("Exit constraint set to {}.", render_constraint(&constraint));
        }

        Command::Exits => {
            let exits = flatten(client.list_exits().await)?;
            if exits.is_empty() {
                println!("(no exits available)");
            }
            for e in exits {
                println!(
                    "{:<28} {:>2}  {:<18} load {:>5.1}%  {}",
                    e.hostname,
                    e.country.to_uppercase(),
                    e.city,
                    e.load * 100.0,
                    if e.allows_free { "free" } else { "plus" }
                );
            }
        }

        Command::AutoProxy { state } => match state {
            None => {
                let settings = flatten(client.get_settings().await)?;
                println!("auto-proxy is {}", on_off(settings.auto_proxy));
            }
            Some(s) => {
                let enabled = parse_on_off(&s)?;
                flatten(client.set_auto_proxy(enabled, current_session()).await)?;
                println!("auto-proxy set to {}", on_off(enabled));
            }
        },

        Command::Vpn { state } => match state {
            None => {
                let settings = flatten(client.get_settings().await)?;
                println!("vpn is {}", on_off(settings.vpn));
            }
            Some(s) => {
                let enabled = parse_on_off(&s)?;
                flatten(client.set_vpn_mode(enabled).await)?;
                println!("vpn set to {}", on_off(enabled));
            }
        },

        Command::LanAccess { state } => match state {
            None => {
                let settings = flatten(client.get_settings().await)?;
                println!("lan-access is {}", on_off(settings.allow_lan));
            }
            Some(s) => {
                let enabled = parse_on_off(&s)?;
                flatten(client.set_allow_lan(enabled).await)?;
                println!("lan-access set to {}", on_off(enabled));
            }
        },

        Command::Logs { count } => {
            let logs = flatten(client.logs(count).await)?;
            for line in logs {
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// Describe the caller's desktop session so the daemon can configure its proxy.
/// This is just identity (uid + a few env vars) — no proxy logic lives here.
fn current_session() -> SessionContext {
    #[cfg(unix)]
    {
        SessionContext {
            uid: unsafe { libc::geteuid() },
            gid: Some(unsafe { libc::getegid() }),
            home: std::env::var("HOME").ok(),
            dbus_session_bus_address: std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(),
            xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(),
        }
    }
    #[cfg(not(unix))]
    {
        SessionContext::default()
    }
}

fn parse_on_off(s: &str) -> anyhow::Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" | "enable" | "enabled" => Ok(true),
        "off" | "false" | "no" | "0" | "disable" | "disabled" => Ok(false),
        other => anyhow::bail!("expected `on` or `off`, got `{other}`"),
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn prompt_secret() -> anyhow::Result<String> {
    use std::io::BufRead as _;
    eprint!("Enter your account secret: ");
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("could not read secret from stdin")?;
    Ok(line.trim().to_string())
}

fn constraint_from_args(set: &ExitConstraintSet) -> anyhow::Result<ExitConstraint> {
    match (&set.country, &set.city) {
        (None, _) => Ok(ExitConstraint::Auto),
        (Some(country), None) => Ok(ExitConstraint::Country(parse_country(country)?)),
        (Some(country), Some(city)) => {
            Ok(ExitConstraint::CountryCity(parse_country(country)?, city.clone()))
        }
    }
}

fn parse_country(s: &str) -> anyhow::Result<CountryCode> {
    CountryCode::for_alpha2_caseless(s)
        .map_err(|_| anyhow::anyhow!("'{s}' is not a valid two-letter country code"))
}

fn render_constraint(c: &ExitConstraint) -> String {
    match c {
        ExitConstraint::Auto => "auto".to_string(),
        ExitConstraint::Direct(s) => format!("direct {s}"),
        ExitConstraint::Hostname(s) => format!("hostname {s}"),
        ExitConstraint::Country(c) => c.alpha2().to_uppercase(),
        ExitConstraint::CountryCity(c, city) => format!("{}/{}", c.alpha2().to_uppercase(), city),
    }
}

fn print_account(a: &AccountInfo) {
    println!("Account #{}: {}", a.user_id, a.level);
    if let Some(exp) = a.plus_expires_unix {
        println!("  Plus expires: {} (unix)", exp);
    }
    if let (Some(used), Some(limit)) = (a.bw_used_mb, a.bw_limit_mb) {
        println!("  Bandwidth: {used} / {limit} MB this period");
    }
}

fn print_status(s: &Status) {
    let state = match s.state {
        ConnState::Disconnected => "disconnected",
        ConnState::Connecting => "connecting",
        ConnState::Connected => "connected",
    };
    println!("State: {state}");
    if let Some(exit) = &s.exit {
        println!(
            "Exit:  {} {} ({})",
            exit.country.to_uppercase(),
            exit.city,
            exit.hostname
        );
    }
    println!(
        "Traffic: {:.2} MiB down / {:.2} MiB up",
        s.total_rx_bytes / 1_048_576.0,
        s.total_tx_bytes / 1_048_576.0
    );
}
