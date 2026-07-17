//! Child `geph5-client` process lifecycle: build its config, spawn it, kill it,
//! restart it, and hand out a control-protocol client pointed at it.
//!
//! Adapted from gephgui-wry's `src/manager.rs` (spawn `--config <file>`, poll the
//! control port until reachable), but here the supervisor manages the child as a
//! sibling `geph5-client` binary rather than re-executing itself.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    time::Duration,
};

use anyhow::Context as _;
use geph5_broker_protocol::{Credential, ExitConstraint};
use geph5_misc_rpc::{
    client_config::BrokerSource, client_control::ControlClient, manager_control::ProxySettings,
};
use serde::{Deserialize, Serialize};

use crate::platform::{self, EngineLaunch, EngineRole, PacketMode, SpawnedEngine};

/// PAC (proxy auto-config) endpoint the connected child serves. Always loopback:
/// it exists only for `apply_proxy` to hand the local desktop session.
pub const PAC_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12223);

const CONFIG_TEMPLATE: &str = include_str!("../default-config.yaml");

/// Persisted manager settings.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    /// The logged-in secret, if any.
    #[serde(default)]
    pub secret: Option<String>,
    /// Desired exit.
    #[serde(default = "default_exit_constraint")]
    pub exit_constraint: ExitConstraint,
    /// Whether the user wants the tunnel up.
    #[serde(default)]
    pub connected: bool,
    /// Local-proxy configuration; `None` means the engine binds no proxy ports
    /// at all.
    #[serde(default)]
    pub proxy: Option<ProxySettings>,
    /// Full-tunnel VPN mode: capture all traffic via a tun device (Linux) or a
    /// WinTUN device (Windows).
    #[serde(default)]
    pub vpn: bool,
    /// Let connections to private/LAN addresses bypass the tunnel.
    #[serde(default = "default_true")]
    pub allow_lan: bool,
    /// Allow direct (non-bridge) connections to exits. Faster but less
    /// censorship-resistant; off by default.
    #[serde(default)]
    pub allow_direct: bool,
}

fn default_exit_constraint() -> ExitConstraint {
    ExitConstraint::Auto
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        // Fresh installs: full-tunnel VPN by default, no local proxy listeners.
        Settings {
            secret: None,
            exit_constraint: ExitConstraint::Auto,
            connected: false,
            proxy: None,
            vpn: true,
            allow_lan: true,
            allow_direct: false,
        }
    }
}

impl Settings {
    /// Load settings from disk, returning defaults if the file is absent.
    pub fn load() -> anyhow::Result<Self> {
        let path = platform::settings_path();
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)
                .with_context(|| format!("could not parse {}", path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Persist settings to disk (creating the state dir if needed).
    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(platform::state_dir())?;
        let path = platform::settings_path();
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&path, bytes).with_context(|| format!("could not write {}", path.display()))
    }
}

/// Build the child's config YAML from current settings.
///
/// `Config` (from `geph5-misc-rpc`, the engine's lightweight config/RPC crate)
/// contains externally-tagged enums (`BrokerSource`, `ExitConstraint`).
/// `serde_yaml` renders those with YAML `!tags`, but the child parses its config
/// by going YAML -> `serde_json::Value` -> `Config` (see
/// `geph5-client/src/bin/geph5-client.rs`), where external tags are plain maps.
/// So we deserialize and serialize through `serde_json::Value` to match, exactly
/// like gephgui-wry does.
fn config_from_template() -> anyhow::Result<geph5_misc_rpc::client_config::Config> {
    let template_val: serde_json::Value =
        serde_yaml::from_str(CONFIG_TEMPLATE).context("bad embedded config template")?;
    serde_json::from_value(template_val).context("bad embedded config template")
}

/// Config for the **tunnel** engine: the real, credentialed engine that brings up
/// the proxy/tunnel. Only ever spawned while connected.
pub(crate) fn build_tunnel_config(
    settings: &Settings,
    pre_resolve_broker_fronts: bool,
) -> anyhow::Result<String> {
    let mut cfg = config_from_template()?;
    cfg.credentials = Credential::Secret(settings.secret.clone().unwrap_or_default());
    cfg.exit_constraint = settings.exit_constraint.clone();
    cfg.allow_lan = settings.allow_lan;
    cfg.allow_direct = settings.allow_direct;
    cfg.dry_run = !settings.connected;
    platform::configure_engine_control(&mut cfg, EngineRole::Tunnel);
    match &settings.proxy {
        Some(p) => {
            let ip: IpAddr = if p.listen_all {
                Ipv4Addr::UNSPECIFIED.into()
            } else {
                Ipv4Addr::LOCALHOST.into()
            };
            cfg.socks5_listen = Some(SocketAddr::new(ip, p.socks5_port));
            cfg.http_proxy_listen = Some(SocketAddr::new(ip, p.http_port));
            // PAC is always up alongside the proxies so autoconf can be toggled
            // live without a child restart.
            cfg.pac_listen = Some(PAC_ADDR);
        }
        None => {
            // Local proxy off: the engine binds no ports at all.
            cfg.socks5_listen = None;
            cfg.http_proxy_listen = None;
            cfg.pac_listen = None;
        }
    }
    // Key the cache by the secret so different accounts never share auth tokens
    // (the engine stores its auth_token under a fixed key, so a shared cache
    // would let account B reuse account A's token). Mirrors geph5-client's own
    // default of hashing the credential into the cache path.
    let secret = settings.secret.as_deref().unwrap_or_default();
    let cache_tag = blake3::hash(secret.as_bytes()).to_hex();
    cfg.cache = Some(platform::cache_dir().join(format!("db-{cache_tag}")));
    if pre_resolve_broker_fronts && let Some(broker) = cfg.broker.as_mut() {
        pre_resolve_fronted_broker_sources(broker);
    }
    let val = serde_json::to_value(&cfg).context("could not serialize child config")?;
    serde_yaml::to_string(&val).context("could not serialize child config")
}

fn pre_resolve_fronted_broker_sources(source: &mut BrokerSource) {
    match source {
        BrokerSource::Fronted {
            front,
            override_dns,
            ..
        } if override_dns.is_none() => match resolve_front_override_dns(front) {
            Ok(addrs) => {
                tracing::info!(
                    front = %front,
                    addr_count = addrs.len(),
                    "pre-resolved fronted broker source for VPN bootstrap"
                );
                *override_dns = Some(addrs);
            }
            Err(err) => {
                tracing::warn!(
                    front = %front,
                    err = %err,
                    "could not pre-resolve fronted broker source for VPN bootstrap"
                );
            }
        },
        BrokerSource::Race(sources) => {
            for source in sources {
                pre_resolve_fronted_broker_sources(source);
            }
        }
        BrokerSource::PriorityRace(sources) => {
            for source in sources.values_mut() {
                pre_resolve_fronted_broker_sources(source);
            }
        }
        _ => {}
    }
}

fn resolve_front_override_dns(front: &str) -> anyhow::Result<Vec<SocketAddr>> {
    let (host, port) = front_lookup_target(front)?;
    let mut addrs = Vec::new();
    for addr in (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("could not resolve {host}:{port}"))?
    {
        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }
    if addrs.is_empty() {
        anyhow::bail!("front resolved to no socket addresses");
    }
    Ok(addrs)
}

fn front_lookup_target(front: &str) -> anyhow::Result<(String, u16)> {
    let url = url::Url::parse(front).context("front is not a valid URL")?;
    let host = url
        .host_str()
        .context("front URL has no hostname")?
        .to_string();
    let port = url
        .port_or_known_default()
        .context("front URL has no explicit or scheme-default port")?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::front_lookup_target;

    #[test]
    fn front_lookup_target_uses_https_default_port() {
        assert_eq!(
            front_lookup_target("https://www.cdn77.com/").unwrap(),
            ("www.cdn77.com".to_string(), 443)
        );
    }

    #[test]
    fn front_lookup_target_preserves_explicit_port() {
        assert_eq!(
            front_lookup_target("http://example.com:8080/path").unwrap(),
            ("example.com".to_string(), 8080)
        );
    }

    #[test]
    fn front_lookup_target_rejects_urls_without_socket_target() {
        assert!(front_lookup_target("file:///tmp/front").is_err());
    }
}

/// Config for the **query** engine: a permanent, secret-less, dry-run engine that
/// answers broker RPCs (exit list, account-by-cred, login validation) regardless
/// of connection state. Because it has no secret it runs no auth/bandwidth loops,
/// writes no auth token, and shares no cache/session with the tunnel engine — so
/// it never needs restarting (not on connect/disconnect, not on login/logout) and
/// broker queries it serves never gap.
fn build_query_config() -> anyhow::Result<String> {
    let mut cfg = config_from_template()?;
    // No credential: broker queries pass the credential as a per-call parameter,
    // so the query engine needs none of its own.
    cfg.credentials = Credential::Secret(String::new());
    cfg.dry_run = true;
    // It tunnels nothing, so it binds no proxy ports (those belong to the tunnel
    // engine and must not be double-bound).
    cfg.socks5_listen = None;
    cfg.http_proxy_listen = None;
    cfg.pac_listen = None;
    platform::configure_engine_control(&mut cfg, EngineRole::Query);
    // Its own cache; with no secret there is no auth token in it to contend over.
    cfg.cache = Some(platform::cache_dir().join("query-db"));
    let val = serde_json::to_value(&cfg).context("could not serialize query config")?;
    serde_yaml::to_string(&val).context("could not serialize query config")
}

/// Spawn the credentialed tunnel engine using the platform's native launch
/// behavior and the VPN controller's packet-transport request.
pub fn spawn_tunnel(
    config_yaml: String,
    service_user: Option<(u32, u32)>,
    packet_mode: PacketMode,
    bind_indices: Option<(u32, u32)>,
) -> anyhow::Result<SpawnedEngine> {
    let config_path = write_engine_config("child-config.yaml", config_yaml)?;
    platform::spawn_engine(EngineLaunch {
        role: EngineRole::Tunnel,
        config_path,
        service_user,
        packet_mode,
        bind_indices,
    })
}

/// Spawn the permanent **query** engine: secret-less, dry-run, never given a tun
/// fd. Runs unprivileged like the tunnel engine so its broker traffic is unmarked
/// (and thus allowed out by the VPN kill switch).
pub fn spawn_query(service_user: Option<(u32, u32)>) -> anyhow::Result<std::process::Child> {
    let config_yaml = build_query_config()?;
    let config_path = write_engine_config("query-config.yaml", config_yaml)?;
    let spawned = platform::spawn_engine(EngineLaunch {
        role: EngineRole::Query,
        config_path,
        service_user,
        packet_mode: PacketMode::None,
        bind_indices: None,
    })?;
    match spawned.transport {
        platform::ChildTransport::None => Ok(spawned.child),
        platform::ChildTransport::Stdio { .. } => {
            platform::kill_child(spawned.child);
            anyhow::bail!("query engine unexpectedly exposed a packet transport")
        }
    }
}

/// Write `config_yaml` to the role's config file, (re)creating the state + cache
/// dirs and migrating the old single-file cache layout. Returns the config path.
fn write_engine_config(
    config_name: &str,
    config_yaml: String,
) -> anyhow::Result<std::path::PathBuf> {
    std::fs::create_dir_all(platform::state_dir())?;
    // The cache is a directory the unprivileged child writes its SQLite into.
    // Migrate the old layout (where `cache` was the SQLite file itself).
    let cache_dir = platform::cache_dir();
    if cache_dir.is_file() {
        let _ = std::fs::remove_file(&cache_dir);
        for sfx in ["-shm", "-wal", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{sfx}", cache_dir.display()));
        }
    }
    std::fs::create_dir_all(&cache_dir)?;

    let config_path = platform::state_dir().join(config_name);
    std::fs::write(&config_path, config_yaml)
        .with_context(|| format!("could not write {}", config_path.display()))?;
    Ok(config_path)
}

/// A control-protocol client pointed at the **tunnel** engine (only reachable
/// while connected).
pub fn live_control() -> ControlClient {
    platform::engine_control(EngineRole::Tunnel)
}

/// A control-protocol client pointed at the permanent **query** engine.
pub fn query_control() -> ControlClient {
    platform::engine_control(EngineRole::Query)
}

/// Wait until the given engine's control protocol answers, up to `timeout`.
pub async fn wait_control_ready(client: &ControlClient, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Duration::from_millis(100);
    let start = std::time::Instant::now();
    loop {
        // start_time() is a cheap, always-available control method.
        match client.start_time().await {
            Ok(_) => return Ok(()),
            Err(_) if start.elapsed() < timeout => {
                tokio::time::sleep(deadline).await;
            }
            Err(e) => anyhow::bail!("engine never became reachable: {e:?}"),
        }
    }
}
