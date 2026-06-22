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
// On unix the daemon<->CLI/GUI and daemon<->engine channels are unix domain
// sockets: no port to squat, and access is filesystem-permissioned. On other
// platforms (Windows) they currently fall back to loopback TCP; named pipes are
// the planned replacement there.

/// Socket the daemon serves the CLI/GUI control protocol on.
#[cfg(unix)]
pub fn daemon_control_path() -> std::path::PathBuf {
    paths::state_dir().join("control.sock")
}

/// Socket the engine child serves its control protocol on (daemon-only). It
/// lives in the cache dir, which is owned by the unprivileged service user so
/// the child can create it.
#[cfg(unix)]
pub fn engine_control_path() -> std::path::PathBuf {
    paths::cache_dir().join("engine.sock")
}

/// Dialer for the daemon's control endpoint (used by the CLI client).
#[cfg(unix)]
pub fn daemon_control_dialer() -> sillad::unix::UnixDialer {
    sillad::unix::UnixDialer {
        path: daemon_control_path(),
    }
}

#[cfg(not(unix))]
pub const DAEMON_CONTROL_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 28080);
#[cfg(not(unix))]
pub const CHILD_CONTROL_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 28081);

#[cfg(not(unix))]
pub fn daemon_control_dialer() -> sillad::tcp::TcpDialer {
    sillad::tcp::TcpDialer {
        dest_addr: DAEMON_CONTROL_ADDR,
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
/// `geph5_client::Config` contains externally-tagged enums (`BrokerSource`,
/// `ExitConstraint`). `serde_yaml` renders those with YAML `!tags`, but the
/// child parses its config by going YAML -> `serde_json::Value` -> `Config`
/// (see `geph5-client/src/bin/geph5-client.rs`), where external tags are plain
/// maps. So we deserialize and serialize through `serde_json::Value` to match,
/// exactly like gephgui-wry does.
fn build_child_config(settings: &Settings) -> anyhow::Result<String> {
    let template_val: serde_json::Value =
        serde_yaml::from_str(CONFIG_TEMPLATE).context("bad embedded config template")?;
    let mut cfg: geph5_client::Config =
        serde_json::from_value(template_val).context("bad embedded config template")?;
    cfg.credentials = Credential::Secret(settings.secret.clone().unwrap_or_default());
    cfg.exit_constraint = settings.exit_constraint.clone();
    cfg.allow_lan = settings.allow_lan;
    cfg.allow_direct = settings.allow_direct;
    // Disconnected == dry-run child: it still serves the control protocol and
    // broker RPCs (login/account/exit-list), but brings up no tunnel or proxies.
    cfg.dry_run = !settings.connected;
    #[cfg(unix)]
    {
        cfg.control_listen = None;
        cfg.control_listen_unix = Some(engine_control_path());
    }
    #[cfg(not(unix))]
    {
        cfg.control_listen = Some(CHILD_CONTROL_ADDR);
        cfg.control_listen_unix = None;
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

/// Spawn a fresh child process.
///
/// `service_user` (uid, gid), when set, drops the child to that user — the child
/// always runs unprivileged in both proxy and VPN modes. `vpn_fd`, when set, is
/// dup'd onto fd 3 in the child and passed via `--vpn-fd 3` for full-tunnel mode.
pub fn spawn_child(
    settings: &Settings,
    service_user: Option<(u32, u32)>,
    vpn_fd: Option<i32>,
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

    let config_yaml = build_child_config(settings)?;
    let config_path = paths::state_dir().join("child-config.yaml");
    std::fs::write(&config_path, config_yaml)
        .with_context(|| format!("could not write {}", config_path.display()))?;

    // Make the config + cache dir readable/writable by the unprivileged child.
    #[cfg(unix)]
    if let Some((uid, gid)) = service_user {
        let _ = chown_path(&config_path, uid, gid);
        let _ = chown_recursive(&cache_dir, uid, gid);
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
        dry_run = !settings.connected,
        vpn = vpn_fd.is_some(),
        "spawned child geph5-client"
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

/// A control-protocol client pointed at the engine child.
pub fn child_control() -> ControlClient {
    #[cfg(unix)]
    let dialer = nanorpc_sillad::DialerTransport(sillad::unix::UnixDialer {
        path: engine_control_path(),
    });
    #[cfg(not(unix))]
    let dialer = nanorpc_sillad::DialerTransport(sillad::tcp::TcpDialer {
        dest_addr: CHILD_CONTROL_ADDR,
    });
    ControlClient::from(dialer)
}

/// Wait until the child's control protocol answers, up to `timeout`.
pub async fn wait_child_ready(timeout: Duration) -> anyhow::Result<()> {
    let client = child_control();
    let deadline = Duration::from_millis(100);
    let start = std::time::Instant::now();
    loop {
        // start_time() is a cheap, always-available control method.
        match client.start_time().await {
            Ok(_) => return Ok(()),
            Err(_) if start.elapsed() < timeout => {
                smol::Timer::after(deadline).await;
            }
            Err(e) => anyhow::bail!("child never became reachable: {e:?}"),
        }
    }
}
