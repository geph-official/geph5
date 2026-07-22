//! Child `geph5-client` process lifecycle: build its config, spawn it, kill it,
//! restart it, and hand out a control-protocol client pointed at it.
//!
//! Adapted from gephgui-wry's `src/manager.rs` (spawn `--config <file>`, poll the
//! control port until reachable), but here the supervisor manages the child as a
//! sibling `geph5-client` binary rather than re-executing itself.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::Context as _;
use geph5_broker_protocol::{Credential, ExitConstraint};
use geph5_misc_rpc::{
    client_control::ControlClient,
    manager_control::{ProxySettings, TunnelSettings},
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
    /// Full-tunnel VPN mode: capture all traffic via the platform VPN device.
    #[serde(default)]
    pub vpn: bool,
    /// Let connections to private/LAN addresses bypass the tunnel.
    #[serde(default = "default_true")]
    pub allow_lan: bool,
    /// Allow direct (non-bridge) connections to exits. Faster but less
    /// censorship-resistant; off by default.
    #[serde(default)]
    pub allow_direct: bool,
    /// Let destinations in mainland China bypass the tunnel.
    #[serde(default)]
    pub passthrough_china: bool,
    /// Metadata attached to newly-created exit sessions.
    #[serde(default)]
    pub session_metadata: serde_json::Value,
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
            passthrough_china: false,
            session_metadata: serde_json::Value::Null,
        }
    }
}

impl Settings {
    pub fn apply_tunnel_settings(&mut self, settings: TunnelSettings) {
        self.exit_constraint = settings.exit_constraint;
        self.proxy = settings.proxy;
        self.vpn = settings.vpn;
        self.allow_lan = settings.allow_lan;
        self.allow_direct = settings.allow_direct;
        self.passthrough_china = settings.passthrough_china;
        self.session_metadata = settings.session_metadata;
    }

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
pub(crate) fn build_tunnel_config(settings: &Settings) -> anyhow::Result<String> {
    let mut cfg = config_from_template()?;
    cfg.credentials = Credential::Secret(settings.secret.clone().unwrap_or_default());
    cfg.exit_constraint = settings.exit_constraint.clone();
    cfg.allow_lan = settings.allow_lan;
    cfg.allow_direct = settings.allow_direct;
    cfg.passthrough_china = settings.passthrough_china;
    cfg.spoof_dns = settings.passthrough_china;
    cfg.sess_metadata = settings.session_metadata.clone();
    cfg.dry_run = !settings.connected;
    platform::configure_engine_control(&mut cfg, EngineRole::Tunnel);
    match &settings.proxy {
        Some(p) => {
            let (socks5, http, pac) = proxy_listen_addrs(p);
            cfg.socks5_listen = Some(socks5);
            cfg.http_proxy_listen = Some(http);
            // PAC is always up alongside the proxies so autoconf can be toggled
            // live without a child restart.
            cfg.pac_listen = Some(pac);
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
    let val = serde_json::to_value(&cfg).context("could not serialize child config")?;
    serde_yaml::to_string(&val).context("could not serialize child config")
}

/// The listen addresses the tunnel engine binds for these proxy settings.
fn proxy_listen_addrs(p: &ProxySettings) -> (SocketAddr, SocketAddr, SocketAddr) {
    let ip: IpAddr = if p.listen_all {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv4Addr::LOCALHOST.into()
    };
    (
        SocketAddr::new(ip, p.socks5_port),
        SocketAddr::new(ip, p.http_port),
        PAC_ADDR,
    )
}

/// How long the proxy-port pre-flight keeps retrying before declaring a
/// conflict: long enough for the just-killed previous engine's listeners to
/// disappear, short enough not to stall the connect flow noticeably.
const PORT_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(2);

/// Verify that the proxy ports the tunnel engine is about to bind are free.
///
/// The engine races all its subsystems, so a single failed proxy-port bind
/// kills the whole child and surfaces only as an opaque "engine never became
/// reachable" timeout. Checking here instead lets the error name the port and,
/// where possible, the process holding it. Test-binds use the same
/// `sillad::tcp::TcpListener` the engine uses, so bind semantics (SO_REUSEADDR
/// etc.) match exactly.
pub async fn ensure_proxy_ports_free(proxy: &ProxySettings) -> anyhow::Result<()> {
    let (socks5, http, pac) = proxy_listen_addrs(proxy);
    ensure_ports_free(&[socks5, http, pac], PORT_PREFLIGHT_TIMEOUT).await
}

async fn ensure_ports_free(addrs: &[SocketAddr], timeout: Duration) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        let mut conflict = None;
        for addr in addrs {
            if let Err(err) = sillad::tcp::TcpListener::bind(*addr).await {
                conflict = Some((*addr, err));
                break;
            }
        }
        let Some((addr, err)) = conflict else {
            return Ok(());
        };
        if start.elapsed() < timeout {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        let port = addr.port();
        let holder = geph5_rt::spawn_blocking(move || port_holder(port))
            .await
            .map(|h| format!(" by {h}"))
            .unwrap_or_default();
        anyhow::bail!(
            "local proxy port {addr} is already taken{holder} ({err}); close the program using it (an older Geph still running?) or change the proxy ports in settings"
        );
    }
}

/// Best-effort lookup of the process listening on `port`, via lsof.
#[cfg(unix)]
fn port_holder(port: u16) -> Option<String> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fcp"])
        .output()
        .ok()?;
    // -F output is one field per line: `p<pid>` then `c<command>`.
    let text = String::from_utf8_lossy(&output.stdout);
    let mut pid = None;
    let mut cmd = None;
    for line in text.lines() {
        match line.as_bytes().first() {
            Some(b'p') if pid.is_none() => pid = Some(line[1..].to_string()),
            Some(b'c') if cmd.is_none() => cmd = Some(line[1..].to_string()),
            _ => {}
        }
    }
    Some(format!("{} (pid {})", cmd?, pid?))
}

#[cfg(not(unix))]
fn port_holder(_port: u16) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::{Settings, build_tunnel_config, ensure_ports_free};
    use geph5_broker_protocol::ExitConstraint;
    use geph5_misc_rpc::manager_control::{ProxySettings, TunnelSettings};
    use std::time::Duration;

    #[test]
    fn ports_free_passes_on_unused_ports() {
        // Grab ephemeral ports, then release them so the pre-flight sees them free.
        let addrs: Vec<_> = (0..2)
            .map(|_| {
                std::net::TcpListener::bind("127.0.0.1:0")
                    .unwrap()
                    .local_addr()
                    .unwrap()
            })
            .collect();
        geph5_rt::block_on(ensure_ports_free(&addrs, Duration::from_millis(100))).unwrap();
    }

    #[test]
    fn ports_free_names_the_conflicting_port() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = holder.local_addr().unwrap();
        let err = geph5_rt::block_on(ensure_ports_free(&[taken], Duration::from_millis(100)))
            .unwrap_err();
        assert!(err.to_string().contains(&taken.to_string()), "{err}");
    }

    #[test]
    fn applying_tunnel_snapshot_preserves_lifecycle_and_credentials() {
        let mut settings = Settings {
            secret: Some("keep-me".into()),
            connected: true,
            ..Settings::default()
        };
        settings.apply_tunnel_settings(TunnelSettings {
            exit_constraint: ExitConstraint::Auto,
            proxy: Some(ProxySettings::default()),
            vpn: false,
            allow_lan: false,
            allow_direct: true,
            passthrough_china: true,
            session_metadata: serde_json::json!({"filter": {"ads": true}}),
        });

        assert_eq!(settings.secret.as_deref(), Some("keep-me"));
        assert!(settings.connected);
        assert!(!settings.vpn);
        assert!(!settings.allow_lan);
        assert!(settings.allow_direct);
        assert!(settings.passthrough_china);
        assert_eq!(settings.session_metadata["filter"]["ads"], true);
    }

    #[test]
    fn tunnel_config_contains_the_complete_snapshot() {
        let settings = Settings {
            secret: Some("secret".into()),
            connected: true,
            allow_lan: false,
            allow_direct: true,
            passthrough_china: true,
            session_metadata: serde_json::json!({"filter": {"nsfw": true}}),
            ..Settings::default()
        };

        let yaml = build_tunnel_config(&settings).unwrap();
        let value: serde_json::Value = serde_yaml::from_str(&yaml).unwrap();
        let config: geph5_misc_rpc::client_config::Config = serde_json::from_value(value).unwrap();
        assert!(!config.allow_lan);
        assert!(config.allow_direct);
        assert!(config.passthrough_china);
        assert_eq!(config.sess_metadata["filter"]["nsfw"], true);
    }
}

/// Config for the **query** engine: a permanent, secret-less, dry-run engine that
/// answers broker RPCs (exit list, account-by-cred, login validation) regardless
/// of connection state. Because it has no secret it runs no auth/bandwidth loops,
/// writes no auth token, and shares no cache/session with the tunnel engine. It
/// is not restarted for settings or lifecycle changes; the manager only replaces
/// it if the process dies.
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
    phys_dns: Vec<std::net::IpAddr>,
) -> anyhow::Result<SpawnedEngine> {
    let config_path = write_engine_config("child-config.yaml", config_yaml)?;
    platform::spawn_engine(EngineLaunch {
        role: EngineRole::Tunnel,
        config_path,
        service_user,
        packet_mode,
        bind_indices,
        phys_dns,
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
        // The query engine is not physical-NIC-bound (it tunnels its broker calls
        // through the active tunnel), so it needs no physical resolvers.
        phys_dns: Vec::new(),
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
/// Fails fast, with the exit status, if the engine process dies first.
pub async fn wait_control_ready(
    child: &mut std::process::Child,
    client: &ControlClient,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Duration::from_millis(100);
    let start = std::time::Instant::now();
    loop {
        // start_time() is a cheap, always-available control method.
        match client.start_time().await {
            Ok(_) => return Ok(()),
            Err(e) => {
                if let Ok(Some(status)) = child.try_wait() {
                    anyhow::bail!(
                        "engine exited during startup ({status}); see the manager log for the engine's own error"
                    );
                }
                if start.elapsed() >= timeout {
                    anyhow::bail!("engine never became reachable: {e:?}");
                }
                tokio::time::sleep(deadline).await;
            }
        }
    }
}
