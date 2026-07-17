//! CLI-side: connect to the `geph manager`, issue one `GephCtlProtocol` call, and
//! render the result for a human.

use std::sync::LazyLock;

use anyhow::Context as _;
use geph5_broker_protocol::ExitConstraint;
use isocountry::CountryCode;

use geph5_misc_rpc::manager_control::{
    self, AccountInfo, ConnState, GephCtlClient, GephCtlError, Status,
};

use crate::{
    cli::{Command, ExitConstraintCmd, ExitConstraintSet},
    platform,
};

/// A shared client pointed at the running manager. Each call still dials a fresh
/// connection (DialerTransport has no pooling); this just avoids rebuilding the
/// wrapper, matching the workspace's `LazyLock` idiom.
fn manager_client() -> &'static GephCtlClient {
    static CLIENT: LazyLock<GephCtlClient> = LazyLock::new(manager_control::manager_control_client);
    &CLIENT
}

/// Flatten the double-Result that RPC methods return, turning a transport-level
/// failure into a friendly "is the manager running?" message.
fn flatten<T>(r: Result<Result<T, String>, GephCtlError<anyhow::Error>>) -> anyhow::Result<T> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(msg)) => Err(anyhow::anyhow!(msg)),
        Err(GephCtlError::Transport(_)) => Err(anyhow::anyhow!(
            "could not reach the geph manager — is it running? start it with `sudo geph5 manager`"
        )),
        Err(e) => Err(anyhow::anyhow!("rpc error: {e:?}")),
    }
}

/// Entry point for all non-manager subcommands.
pub fn run(command: Command) -> anyhow::Result<()> {
    geph5_rt::block_on(run_inner(command))
}

async fn run_inner(command: Command) -> anyhow::Result<()> {
    let client = manager_client();
    match command {
        Command::Manager
        | Command::ApplyProxy { .. }
        | Command::RegisterManager
        | Command::UnregisterManager => unreachable!("handled in main"),

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
            flatten(client.logout(platform::current_session()).await)?;
            println!("Logged out.");
        }
        Command::Account => {
            let account = flatten(client.account().await)?;
            print_account(&account);
        }

        Command::Connect => {
            // Pass our session so the manager can configure this user's system
            // proxy; the proxy-setting code lives entirely in the manager.
            flatten(client.connect(platform::current_session()).await)?;
            println!("Connecting…");
        }
        Command::Disconnect => {
            flatten(client.disconnect(platform::current_session()).await)?;
            println!("Disconnected.");
        }
        Command::Reconnect => {
            flatten(client.reconnect(platform::current_session()).await)?;
            println!("Reconnecting…");
        }
        Command::Status => {
            let status = flatten(client.status().await)?;
            print_status(&status);
            let settings = flatten(client.get_settings().await)?;
            let mode = match (settings.vpn, settings.proxy.is_some()) {
                (true, true) => "vpn+proxy",
                (true, false) => "vpn",
                (false, true) => "proxy",
                (false, false) => "none",
            };
            println!(
                "Mode:  {} | lan-access {} | allow-direct {} | auto-proxy {}",
                mode,
                on_off(settings.allow_lan),
                on_off(settings.allow_direct),
                on_off(settings.proxy.as_ref().is_some_and(|p| p.autoconf)),
            );
        }

        Command::ExitConstraint(ExitConstraintCmd::Get) => {
            let settings = flatten(client.get_settings().await)?;
            println!("{}", render_constraint(&settings.exit_constraint));
        }
        Command::ExitConstraint(ExitConstraintCmd::Set(set)) => {
            let constraint = constraint_from_args(&set)?;
            let view = flatten(client.get_settings().await)?;
            let mut settings = view.tunnel_settings();
            settings.exit_constraint = constraint.clone();
            flatten(
                client
                    .apply_settings(settings, platform::current_session())
                    .await,
            )?;
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

        Command::Proxy { state } => match state {
            None => {
                let settings = flatten(client.get_settings().await)?;
                match settings.proxy {
                    Some(p) => {
                        let host = if p.listen_all { "0.0.0.0" } else { "127.0.0.1" };
                        println!(
                            "proxy is on (socks5 {host}:{}, http {host}:{})",
                            p.socks5_port, p.http_port
                        );
                    }
                    None => println!("proxy is off"),
                }
            }
            Some(s) => {
                let enabled = parse_on_off(&s)?;
                let view = flatten(client.get_settings().await)?;
                let mut settings = view.tunnel_settings();
                let proxy = if enabled {
                    // Keep an existing config; otherwise start from the defaults.
                    Some(settings.proxy.clone().unwrap_or_default())
                } else {
                    None
                };
                settings.proxy = proxy;
                flatten(
                    client
                        .apply_settings(settings, platform::current_session())
                        .await,
                )?;
                println!("proxy set to {}", on_off(enabled));
            }
        },

        Command::AutoProxy { state } => match state {
            None => {
                let settings = flatten(client.get_settings().await)?;
                match settings.proxy {
                    Some(p) => println!("auto-proxy is {}", on_off(p.autoconf)),
                    None => println!("auto-proxy is off (proxy is off)"),
                }
            }
            Some(s) => {
                let enabled = parse_on_off(&s)?;
                let view = flatten(client.get_settings().await)?;
                let mut settings = view.tunnel_settings();
                let Some(mut proxy) = settings.proxy.take() else {
                    anyhow::bail!("the proxy is off entirely; enable it first with `proxy on`");
                };
                proxy.autoconf = enabled;
                settings.proxy = Some(proxy);
                flatten(
                    client
                        .apply_settings(settings, platform::current_session())
                        .await,
                )?;
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
                let view = flatten(client.get_settings().await)?;
                let mut settings = view.tunnel_settings();
                settings.vpn = enabled;
                flatten(
                    client
                        .apply_settings(settings, platform::current_session())
                        .await,
                )?;
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
                let view = flatten(client.get_settings().await)?;
                let mut settings = view.tunnel_settings();
                settings.allow_lan = enabled;
                flatten(
                    client
                        .apply_settings(settings, platform::current_session())
                        .await,
                )?;
                println!("lan-access set to {}", on_off(enabled));
            }
        },

        Command::AllowDirect { state } => match state {
            None => {
                let settings = flatten(client.get_settings().await)?;
                println!("allow-direct is {}", on_off(settings.allow_direct));
            }
            Some(s) => {
                let enabled = parse_on_off(&s)?;
                let view = flatten(client.get_settings().await)?;
                let mut settings = view.tunnel_settings();
                settings.allow_direct = enabled;
                flatten(
                    client
                        .apply_settings(settings, platform::current_session())
                        .await,
                )?;
                println!("allow-direct set to {}", on_off(enabled));
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
        (Some(country), Some(city)) => Ok(ExitConstraint::CountryCity(
            parse_country(country)?,
            city.clone(),
        )),
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
