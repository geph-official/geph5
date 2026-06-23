//! Child `geph5-client` process lifecycle: build its config, spawn it, kill it,
//! restart it, and hand out a control-protocol client pointed at it.
//!
//! Adapted from gephgui-wry's `src/daemon.rs` (spawn `--config <file>`, poll the
//! control port until reachable), but here the supervisor manages the child as a
//! sibling `geph5-client` binary rather than re-executing itself.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    process::{Child, Command},
    time::Duration,
};

use anyhow::Context as _;
use geph5_broker_protocol::{Credential, ExitConstraint};
use geph5_misc_rpc::client_control::ControlClient;
use serde::{Deserialize, Serialize};

use crate::paths;

/// SOCKS5 proxy the connected child exposes.
pub const SOCKS5_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9909);
/// HTTP proxy the connected child exposes.
pub const HTTP_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9910);
/// PAC (proxy auto-config) endpoint the connected child serves.
pub const PAC_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12223);

const CONFIG_TEMPLATE: &str = include_str!("../default-config.yaml");

// ---- control-plane endpoints ----
//
// The daemon<->CLI/GUI and daemon<->engine channels are namespaced, local-only,
// access-controlled streams: unix domain sockets on unix, Windows named pipes on
// Windows. Neither squats a TCP port, and both are permissioned (filesystem mode
// / pipe security descriptor) rather than reachable by anything that can open a
// loopback socket.

/// Socket the daemon serves the CLI/GUI control protocol on. Lives directly in
/// the (root-owned) runtime dir; chmod'd 0666 so unprivileged clients can reach
/// the root daemon.
#[cfg(unix)]
pub fn daemon_control_path() -> std::path::PathBuf {
    paths::runtime_dir().join("control.sock")
}

/// Socket the engine child serves its control protocol on (daemon-only). It
/// lives in a `engine/` subdir of the runtime dir, which the daemon chowns to
/// the unprivileged service user so the child can bind here — without giving the
/// service user write access to the runtime root (which holds the daemon's own,
/// root-owned, control socket).
#[cfg(unix)]
pub fn engine_control_path() -> std::path::PathBuf {
    paths::runtime_dir().join("engine").join("engine.sock")
}

/// Socket the permanent, secret-less *query* engine serves its control protocol
/// on (daemon-only). Same service-user-owned-subdir scheme as the engine socket.
#[cfg(unix)]
pub fn query_control_path() -> std::path::PathBuf {
    paths::runtime_dir().join("query").join("query.sock")
}

/// Dialer for the daemon's control endpoint (used by the CLI client).
#[cfg(unix)]
pub fn daemon_control_dialer() -> sillad::unix::UnixDialer {
    sillad::unix::UnixDialer {
        path: daemon_control_path(),
    }
}

/// Named pipe the daemon serves the CLI/GUI control protocol on.
#[cfg(windows)]
pub const DAEMON_CONTROL_PIPE: &str = r"\\.\pipe\geph-daemon-control";
/// Named pipe the engine child serves its control protocol on (daemon-only).
#[cfg(windows)]
pub const ENGINE_CONTROL_PIPE: &str = r"\\.\pipe\geph-engine-control";
/// Named pipe the permanent, secret-less query engine serves on (daemon-only).
#[cfg(windows)]
pub const QUERY_CONTROL_PIPE: &str = r"\\.\pipe\geph-query-control";

/// Dialer for the daemon's control endpoint (used by the CLI client).
#[cfg(windows)]
pub fn daemon_control_dialer() -> sillad::windows_pipe::NamedPipeDialer {
    sillad::windows_pipe::NamedPipeDialer {
        name: DAEMON_CONTROL_PIPE.to_string(),
    }
}

/// Persisted daemon settings.
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
    /// Whether to point the desktop's system proxy at the tunnel while connected.
    #[serde(default = "default_true")]
    pub auto_proxy: bool,
    /// Full-tunnel VPN mode (Linux): capture all traffic via a tun device.
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
        Settings {
            secret: None,
            exit_constraint: ExitConstraint::Auto,
            connected: false,
            auto_proxy: true,
            vpn: false,
            allow_lan: true,
            allow_direct: false,
        }
    }
}

impl Settings {
    /// Load settings from disk, returning defaults if the file is absent.
    pub fn load() -> anyhow::Result<Self> {
        let path = paths::settings_path();
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)
                .with_context(|| format!("could not parse {}", path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Persist settings to disk (creating the state dir if needed).
    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(paths::state_dir())?;
        let path = paths::settings_path();
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
fn build_tunnel_config(settings: &Settings) -> anyhow::Result<String> {
    let mut cfg = config_from_template()?;
    cfg.credentials = Credential::Secret(settings.secret.clone().unwrap_or_default());
    cfg.exit_constraint = settings.exit_constraint.clone();
    cfg.allow_lan = settings.allow_lan;
    cfg.allow_direct = settings.allow_direct;
    cfg.dry_run = !settings.connected;
    #[cfg(unix)]
    {
        cfg.control_listen = None;
        cfg.control_listen_unix = Some(engine_control_path());
    }
    #[cfg(windows)]
    {
        cfg.control_listen = None;
        cfg.control_listen_unix = None;
        cfg.control_listen_pipe = Some(ENGINE_CONTROL_PIPE.to_string());
    }
    cfg.socks5_listen = Some(SOCKS5_ADDR);
    cfg.http_proxy_listen = Some(HTTP_ADDR);
    cfg.pac_listen = Some(PAC_ADDR);
    // Key the cache by the secret so different accounts never share auth tokens
    // (the engine stores its auth_token under a fixed key, so a shared cache
    // would let account B reuse account A's token). Mirrors geph5-client's own
    // default of hashing the credential into the cache path.
    let secret = settings.secret.as_deref().unwrap_or_default();
    let cache_tag = blake3::hash(secret.as_bytes()).to_hex();
    cfg.cache = Some(paths::cache_dir().join(format!("db-{cache_tag}")));
    let val = serde_json::to_value(&cfg).context("could not serialize child config")?;
    serde_yaml::to_string(&val).context("could not serialize child config")
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
    #[cfg(unix)]
    {
        cfg.control_listen = None;
        cfg.control_listen_unix = Some(query_control_path());
    }
    #[cfg(windows)]
    {
        cfg.control_listen = None;
        cfg.control_listen_unix = None;
        cfg.control_listen_pipe = Some(QUERY_CONTROL_PIPE.to_string());
    }
    // Its own cache; with no secret there is no auth token in it to contend over.
    cfg.cache = Some(paths::cache_dir().join("query-db"));
    let val = serde_json::to_value(&cfg).context("could not serialize query config")?;
    serde_yaml::to_string(&val).context("could not serialize query config")
}

/// Locate the `geph5-client` binary: prefer one next to the current executable,
/// otherwise rely on `PATH`.
fn geph5_client_command() -> Command {
    // Normally `geph5-client` is an installed companion binary found on PATH
    // (Windows appends `.exe` automatically). `GEPH_CLIENT_BIN` overrides this to
    // point at an uninstalled build, e.g. `target/debug/geph5-client`.
    match std::env::var_os("GEPH_CLIENT_BIN") {
        Some(path) => Command::new(path),
        None => Command::new("geph5-client"),
    }
}

/// Spawn the **tunnel** engine: the real, credentialed engine. Only spawned while
/// connected. `service_user`, when set, drops it to that user (it always runs
/// unprivileged); `vpn_fd`, when set, is dup'd onto fd 3 and passed via
/// `--vpn-fd 3` for full-tunnel mode.
pub fn spawn_tunnel(
    settings: &Settings,
    service_user: Option<(u32, u32)>,
    vpn_fd: Option<i32>,
) -> anyhow::Result<Child> {
    let config_yaml = build_tunnel_config(settings)?;
    #[cfg(unix)]
    let sock_dir = Some(
        engine_control_path()
            .parent()
            .expect("engine socket path has a parent")
            .to_path_buf(),
    );
    spawn_engine("tunnel", "child-config.yaml", config_yaml, service_user, vpn_fd, {
        #[cfg(unix)]
        {
            sock_dir
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
}

/// Spawn the permanent **query** engine: secret-less, dry-run, never given a tun
/// fd. Runs unprivileged like the tunnel engine so its broker traffic is unmarked
/// (and thus allowed out by the VPN kill switch).
pub fn spawn_query(service_user: Option<(u32, u32)>) -> anyhow::Result<Child> {
    let config_yaml = build_query_config()?;
    #[cfg(unix)]
    let sock_dir = Some(
        query_control_path()
            .parent()
            .expect("query socket path has a parent")
            .to_path_buf(),
    );
    spawn_engine("query", "query-config.yaml", config_yaml, service_user, None, {
        #[cfg(unix)]
        {
            sock_dir
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
}

/// Lower-level: write `config_yaml`, make state/cache/socket dirs owned by the
/// service user, and spawn a `geph5-client` against the config.
fn spawn_engine(
    role: &str,
    config_name: &str,
    config_yaml: String,
    service_user: Option<(u32, u32)>,
    vpn_fd: Option<i32>,
    _sock_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<Child> {
    std::fs::create_dir_all(paths::state_dir())?;
    // The cache is a directory the unprivileged child writes its SQLite into.
    // Migrate the old layout (where `cache` was the SQLite file itself).
    let cache_dir = paths::cache_dir();
    if cache_dir.is_file() {
        let _ = std::fs::remove_file(&cache_dir);
        for sfx in ["-shm", "-wal", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{sfx}", cache_dir.display()));
        }
    }
    std::fs::create_dir_all(&cache_dir)?;

    // The engine binds its control socket in a service-user-owned runtime subdir.
    #[cfg(unix)]
    if let Some(dir) = _sock_dir.as_ref() {
        std::fs::create_dir_all(dir)?;
    }

    let config_path = paths::state_dir().join(config_name);
    std::fs::write(&config_path, config_yaml)
        .with_context(|| format!("could not write {}", config_path.display()))?;

    // Make the config + cache dir + socket dir owned by the unprivileged child so
    // it can read its config and bind its control socket.
    #[cfg(unix)]
    if let Some((uid, gid)) = service_user {
        let _ = chown_path(&config_path, uid, gid);
        let _ = chown_recursive(&cache_dir, uid, gid);
        if let Some(dir) = _sock_dir.as_ref() {
            let _ = chown_path(dir, uid, gid);
        }
    }

    let mut cmd = geph5_client_command();
    cmd.arg("--config").arg(&config_path);
    if vpn_fd.is_some() {
        cmd.arg("--vpn-fd").arg("3");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: don't pop a console window for the child.
        cmd.creation_flags(0x08000000);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: child_pre_exec uses only async-signal-safe syscalls and no
        // allocation, as required for a post-fork/pre-exec hook.
        unsafe {
            cmd.pre_exec(move || child_pre_exec(service_user, vpn_fd));
        }
    }
    let child = cmd.spawn().context(
        "could not spawn geph5-client (is it installed on PATH? or set GEPH_CLIENT_BIN)",
    )?;
    tracing::info!(
        pid = child.id(),
        role,
        vpn = vpn_fd.is_some(),
        "spawned geph5-client engine"
    );
    Ok(child)
}

#[cfg(unix)]
fn chown_path(path: &std::path::Path, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("path has NUL"))?;
    if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn chown_recursive(path: &std::path::Path, uid: u32, gid: u32) -> std::io::Result<()> {
    chown_path(path, uid, gid)?;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            chown_recursive(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

/// `pre_exec` hook for the child. Drops to the unprivileged service user, hands
/// the tun fd over at a known number, and arms parent-death. The ordering is
/// load-bearing:
///   1. `setgroups` before `setuid` (it needs root) to drop root's supplementary
///      groups — and `CommandExt::groups` is still unstable, so we do it by hand;
///   2. `setgid`/`setuid` to the service user;
///   3. `dup2` the tun fd onto fd 3 (the dup clears CLOEXEC so it survives exec);
///   4. `PR_SET_PDEATHSIG` LAST — `setuid` clears any pending parent-death signal.
///
/// SAFETY: runs post-fork/pre-exec, so it must use only async-signal-safe
/// syscalls and perform no allocation.
#[cfg(target_os = "linux")]
fn child_pre_exec(service_user: Option<(u32, u32)>, vpn_fd: Option<i32>) -> std::io::Result<()> {
    let err = std::io::Error::last_os_error;
    if let Some((uid, gid)) = service_user {
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            return Err(err());
        }
        if unsafe { libc::setgid(gid) } != 0 {
            return Err(err());
        }
        if unsafe { libc::setuid(uid) } != 0 {
            return Err(err());
        }
    }
    if let Some(fd) = vpn_fd
        && unsafe { libc::dup2(fd, 3) } < 0
    {
        return Err(err());
    }
    // PR_SET_PDEATHSIG == 1.
    if unsafe { libc::prctl(1, libc::SIGTERM, 0, 0, 0) } != 0 {
        return Err(err());
    }
    Ok(())
}

/// Kill a child process and reap it.
pub fn kill_child(mut child: Child) {
    let pid = child.id();
    let _ = child.kill();
    let _ = child.wait();
    tracing::info!(pid, "killed child geph5-client");
}

/// A control-protocol client pointed at the **tunnel** engine (only reachable
/// while connected).
pub fn live_control() -> ControlClient {
    #[cfg(unix)]
    let dialer = nanorpc_sillad::DialerTransport(sillad::unix::UnixDialer {
        path: engine_control_path(),
    });
    #[cfg(windows)]
    let dialer = nanorpc_sillad::DialerTransport(sillad::windows_pipe::NamedPipeDialer {
        name: ENGINE_CONTROL_PIPE.to_string(),
    });
    ControlClient::from(dialer)
}

/// A control-protocol client pointed at the permanent **query** engine.
pub fn query_control() -> ControlClient {
    #[cfg(unix)]
    let dialer = nanorpc_sillad::DialerTransport(sillad::unix::UnixDialer {
        path: query_control_path(),
    });
    #[cfg(windows)]
    let dialer = nanorpc_sillad::DialerTransport(sillad::windows_pipe::NamedPipeDialer {
        name: QUERY_CONTROL_PIPE.to_string(),
    });
    ControlClient::from(dialer)
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
                smol::Timer::after(deadline).await;
            }
            Err(e) => anyhow::bail!("engine never became reachable: {e:?}"),
        }
    }
}
