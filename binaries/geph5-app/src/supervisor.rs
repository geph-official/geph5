//! Child `geph5-client` process lifecycle: build its config, spawn it, kill it,
//! restart it, and hand out a control-protocol client pointed at it.
//!
//! Adapted from gephgui-wry's `src/manager.rs` (spawn `--config <file>`, poll the
//! control port until reachable), but here the supervisor manages the child as a
//! sibling `geph5-client` binary rather than re-executing itself.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    process::{Child, Command},
    time::Duration,
};

use anyhow::Context as _;
use geph5_broker_protocol::{Credential, ExitConstraint};
use geph5_misc_rpc::{client_config::BrokerSource, client_control::ControlClient};
use serde::{Deserialize, Serialize};

use crate::{paths, protocol::ProxySettings};

/// PAC (proxy auto-config) endpoint the connected child serves. Always loopback:
/// it exists only for `apply_proxy` to hand the local desktop session.
pub const PAC_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12223);

const CONFIG_TEMPLATE: &str = include_str!("../default-config.yaml");

// ---- control-plane endpoints ----
//
// The manager<->CLI/GUI and manager<->engine channels are namespaced, local-only,
// access-controlled streams: unix domain sockets on unix, Windows named pipes on
// Windows. Neither squats a TCP port, and both are permissioned (filesystem mode
// / pipe security descriptor) rather than reachable by anything that can open a
// loopback socket.

/// Socket the manager serves the CLI/GUI control protocol on. Lives directly in
/// the (root-owned) runtime dir; chmod'd 0666 so unprivileged clients can reach
/// the root manager.
#[cfg(unix)]
pub fn manager_control_path() -> std::path::PathBuf {
    paths::runtime_dir().join("control.sock")
}

/// Socket the engine child serves its control protocol on (manager-only). It
/// lives in a `engine/` subdir of the runtime dir, which the manager chowns to
/// the unprivileged service user so the child can bind here — without giving the
/// service user write access to the runtime root (which holds the manager's own,
/// root-owned, control socket).
#[cfg(unix)]
pub fn engine_control_path() -> std::path::PathBuf {
    paths::runtime_dir().join("engine").join("engine.sock")
}

/// Socket the permanent, secret-less *query* engine serves its control protocol
/// on (manager-only). Same service-user-owned-subdir scheme as the engine socket.
#[cfg(unix)]
pub fn query_control_path() -> std::path::PathBuf {
    paths::runtime_dir().join("query").join("query.sock")
}

/// Dialer for the manager's control endpoint (used by the CLI client).
#[cfg(unix)]
pub fn manager_control_dialer() -> sillad::unix::UnixDialer {
    sillad::unix::UnixDialer {
        path: manager_control_path(),
    }
}

/// Named pipe the manager serves the CLI/GUI control protocol on.
#[cfg(windows)]
pub const MANAGER_CONTROL_PIPE: &str = r"\\.\pipe\geph-manager-control";
/// Named pipe the engine child serves its control protocol on (manager-only).
#[cfg(windows)]
pub const ENGINE_CONTROL_PIPE: &str = r"\\.\pipe\geph-engine-control";
/// Named pipe the permanent, secret-less query engine serves on (manager-only).
#[cfg(windows)]
pub const QUERY_CONTROL_PIPE: &str = r"\\.\pipe\geph-query-control";

/// Dialer for the manager's control endpoint (used by the CLI client).
#[cfg(windows)]
pub fn manager_control_dialer() -> sillad::windows_pipe::NamedPipeDialer {
    sillad::windows_pipe::NamedPipeDialer {
        name: MANAGER_CONTROL_PIPE.to_string(),
    }
}

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
    cfg.cache = Some(paths::cache_dir().join(format!("db-{cache_tag}")));
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

/// Bare filename of the engine binary for the current platform.
const ENGINE_BIN: &str = if cfg!(windows) { "geph5-client.exe" } else { "geph5-client" };

/// Resolve the full path of the `geph5-client` engine binary.
///
/// On Windows this must be a concrete full path, not a bare name: the VPN kill
/// switch derives a WFP app-id from it to permit the engine's own egress, and an
/// app-id built from a relative name won't match the running image (the engine
/// would then be blocked by its own kill switch). The resolution order is:
///   1. `GEPH_CLIENT_BIN` (an uninstalled build, e.g. `target/debug/geph5-client`);
///   2. a companion binary next to the manager (cargo-install / the installer put
///      `geph5` and `geph5-client` in the same directory);
///   3. the bare name as a last resort (PATH lookup at spawn; no app-id match).
pub fn engine_bin_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("GEPH_CLIENT_BIN") {
        return path.into();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(sibling) = exe.parent().map(|d| d.join(ENGINE_BIN)) {
            if sibling.exists() {
                return sibling;
            }
        }
    }
    std::path::PathBuf::from(ENGINE_BIN)
}

/// A `Command` that runs the engine binary. On macOS the engine runs unprivileged
/// as `_geph`, which cannot traverse the developer's 0750 home directory, so the
/// binary is first staged into a world-traversable, root-owned location (see
/// [`staged_engine_bin`]); elsewhere it runs the resolved path directly.
fn geph5_client_command() -> anyhow::Result<Command> {
    #[cfg(target_os = "macos")]
    let bin = staged_engine_bin()?;
    #[cfg(not(target_os = "macos"))]
    let bin = engine_bin_path();
    Ok(Command::new(bin))
}

/// Copy the resolved engine binary into `<state_dir>/bin` (root-owned, 0755) so the
/// unprivileged `_geph` engine can exec it even when the build output lives under
/// the developer's 0750 home. Re-stages only when the source is newer; once staged
/// it's a no-op. In a packaged install the source is already in an accessible
/// location, so this is just a cheap same-host copy.
#[cfg(target_os = "macos")]
fn staged_engine_bin() -> anyhow::Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let src = engine_bin_path();
    let dir = paths::state_dir().join("bin");
    std::fs::create_dir_all(&dir)?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
    let dst = dir.join(ENGINE_BIN);

    let up_to_date = match (std::fs::metadata(&src), std::fs::metadata(&dst)) {
        (Ok(s), Ok(d)) => match (s.modified(), d.modified()) {
            (Ok(sm), Ok(dm)) => dm >= sm,
            _ => false,
        },
        _ => false,
    };
    if !up_to_date {
        std::fs::copy(&src, &dst)
            .with_context(|| format!("staging engine binary {} -> {}", src.display(), dst.display()))?;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
        // Own it root:wheel so the unprivileged user can't rewrite the staged
        // binary (fs::copy may inherit the source's ownership).
        let _ = chown_path(&dst, 0, 0);
    }
    Ok(dst)
}

/// Spawn the **tunnel** engine: the real, credentialed engine. Only spawned while
/// connected. `service_user`, when set, drops it to that user (it always runs
/// unprivileged); `vpn_fd`, when set, is dup'd onto fd 3 and passed via
/// `--vpn-fd 3` for full-tunnel mode.
///
/// Unix only: on Windows the tunnel engine is spawned via [`spawn_tunnel_windows`]
/// (full-tunnel mode drives the engine through stdio rather than a tun fd).
#[cfg(not(windows))]
pub fn spawn_tunnel(
    config_yaml: String,
    service_user: Option<(u32, u32)>,
    vpn_fd: Option<i32>,
    bind_indices: Option<(u32, u32)>,
) -> anyhow::Result<Child> {
    #[cfg(unix)]
    let sock_dir = Some(
        engine_control_path()
            .parent()
            .expect("engine socket path has a parent")
            .to_path_buf(),
    );
    spawn_engine(
        "tunnel",
        "child-config.yaml",
        config_yaml,
        service_user,
        vpn_fd,
        bind_indices,
        {
            #[cfg(unix)]
            {
                sock_dir
            }
            #[cfg(not(unix))]
            {
                None
            }
        },
    )
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
    spawn_engine(
        "query",
        "query-config.yaml",
        config_yaml,
        service_user,
        None,
        None,
        {
            #[cfg(unix)]
            {
                sock_dir
            }
            #[cfg(not(unix))]
            {
                None
            }
        },
    )
}

/// Spawn the **tunnel** engine on Windows. In full-tunnel mode (`binds = Some`)
/// the manager owns the WinTUN device, so the child is driven through stdio
/// (`--stdio-vpn`, 16-bit length-prefixed packets) and pinned to the physical
/// interface via `GEPH_VPN_BIND_IF4/6`; the returned stdio handles are wired to the
/// manager's packet pump. In proxy mode (`binds = None`) it spawns an ordinary
/// child and returns no stdio.
#[cfg(windows)]
pub fn spawn_tunnel_windows(
    config_yaml: String,
    binds: Option<(u32, u32)>,
) -> anyhow::Result<(
    Child,
    Option<(std::process::ChildStdin, std::process::ChildStdout)>,
)> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    let config_path = write_engine_config("child-config.yaml", config_yaml)?;

    let mut cmd = geph5_client_command()?;
    cmd.arg("--config").arg(&config_path);
    let vpn = binds.is_some();
    if let Some((if4, if6)) = binds {
        cmd.arg("--stdio-vpn");
        cmd.env("GEPH_VPN_BIND_IF4", if4.to_string());
        cmd.env("GEPH_VPN_BIND_IF6", if6.to_string());
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
    }
    // CREATE_NO_WINDOW: don't pop a console window for the child.
    cmd.creation_flags(0x08000000);

    let mut child = cmd.spawn().context(
        "could not spawn geph5-client (is it installed on PATH? or set GEPH_CLIENT_BIN)",
    )?;
    assign_child_to_reaper_job(&child);
    tracing::info!(pid = child.id(), role = "tunnel", vpn, "spawned geph5-client engine");

    let stdio = if vpn {
        let cin = child.stdin.take().context("child stdin missing")?;
        let cout = child.stdout.take().context("child stdout missing")?;
        Some((cin, cout))
    } else {
        None
    };
    Ok((child, stdio))
}

/// Write `config_yaml` to the role's config file, (re)creating the state + cache
/// dirs and migrating the old single-file cache layout. Returns the config path.
fn write_engine_config(config_name: &str, config_yaml: String) -> anyhow::Result<std::path::PathBuf> {
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

    let config_path = paths::state_dir().join(config_name);
    std::fs::write(&config_path, config_yaml)
        .with_context(|| format!("could not write {}", config_path.display()))?;
    Ok(config_path)
}

/// Lower-level: write `config_yaml`, make state/cache/socket dirs owned by the
/// service user, and spawn a `geph5-client` against the config.
fn spawn_engine(
    role: &str,
    config_name: &str,
    config_yaml: String,
    service_user: Option<(u32, u32)>,
    vpn_fd: Option<i32>,
    bind_indices: Option<(u32, u32)>,
    _sock_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<Child> {
    // `service_user` is only consulted on Unix (uid drop); harmless elsewhere.
    let _ = service_user;
    let config_path = write_engine_config(config_name, config_yaml)?;
    #[cfg(unix)]
    let cache_dir = paths::cache_dir();

    // The engine binds its control socket in a service-user-owned runtime subdir.
    #[cfg(unix)]
    if let Some(dir) = _sock_dir.as_ref() {
        std::fs::create_dir_all(dir)?;
    }

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

    let mut cmd = geph5_client_command()?;
    cmd.arg("--config").arg(&config_path);
    if vpn_fd.is_some() {
        cmd.arg("--vpn-fd").arg("3");
    }
    // Physical-interface pinning for the engine's own sockets (macOS full-tunnel:
    // the bound dialer reads these and applies IP_BOUND_IF). Linux/query pass None.
    if let Some((if4, if6)) = bind_indices {
        cmd.env("GEPH_VPN_BIND_IF4", if4.to_string());
        cmd.env("GEPH_VPN_BIND_IF6", if6.to_string());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: don't pop a console window for the child.
        cmd.creation_flags(0x08000000);
    }
    #[cfg(unix)]
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
    #[cfg(windows)]
    assign_child_to_reaper_job(&child);
    tracing::info!(
        pid = child.id(),
        role,
        vpn = vpn_fd.is_some(),
        "spawned geph5-client engine"
    );
    Ok(child)
}

/// Windows: assign an engine child to a process-lifetime job object configured with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the child cannot outlive the manager.
///
/// The manager only kills its children via the Ctrl-C teardown in `manager.rs`, but a
/// hard `TerminateProcess` — which is exactly what Task Scheduler's `/End` does when
/// an installer upgrade runs `unregister-manager` — bypasses that, orphaning
/// `geph5-client.exe` and leaving it holding its own file (breaking the overwrite).
/// With the child in this job, the manager's death (by *any* means) closes the job's
/// last handle and the OS reaps the child. The Windows analogue of the Linux
/// `PR_SET_PDEATHSIG` armed on the tun-fd children in `child_pre_exec`.
#[cfg(windows)]
fn assign_child_to_reaper_job(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // Created once and never closed — it must stay open for the manager's whole life,
    // since closing the last handle is precisely what triggers the kill. Stored as
    // usize because the raw HANDLE pointer is not Send/Sync.
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            tracing::warn!(
                err = %std::io::Error::last_os_error(),
                "CreateJobObject failed; engine children may orphan if the manager is killed"
            );
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            tracing::warn!(
                err = %std::io::Error::last_os_error(),
                "SetInformationJobObject(KILL_ON_JOB_CLOSE) failed; engine children may orphan"
            );
        }
        job as usize
    });
    if job == 0 {
        return;
    }
    if unsafe { AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE) } == 0 {
        tracing::warn!(
            pid = child.id(),
            err = %std::io::Error::last_os_error(),
            "AssignProcessToJobObject failed; this engine child may orphan if the manager is killed"
        );
    }
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

/// `pre_exec` hook for the child. Drops to the unprivileged service user (when one
/// is given), hands the tun fd over at a known number, and — on Linux — arms
/// parent-death. The ordering is load-bearing:
///   1. `setgroups` before `setuid` (it needs root) to drop root's supplementary
///      groups — and `CommandExt::groups` is still unstable, so we do it by hand;
///   2. `setgid`/`setuid` to the service user;
///   3. `dup2` the tun fd onto fd 3 (the dup clears CLOEXEC so it survives exec);
///   4. `PR_SET_PDEATHSIG` LAST — `setuid` clears any pending parent-death signal.
///
/// On macOS there is no service user (so steps 1–2 are skipped) and no
/// `PR_SET_PDEATHSIG` equivalent (step 4 is skipped); only the fd hand-off runs.
///
/// SAFETY: runs post-fork/pre-exec, so it must use only async-signal-safe
/// syscalls and perform no allocation.
#[cfg(unix)]
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
    // PR_SET_PDEATHSIG == 1. Linux-only; macOS has no direct equivalent.
    #[cfg(target_os = "linux")]
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
                geph5_rt::sleep(deadline).await;
            }
            Err(e) => anyhow::bail!("engine never became reachable: {e:?}"),
        }
    }
}
