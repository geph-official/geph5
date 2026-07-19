//! Operating-system boundary for geph5-app.
//!
//! All target selection lives in this file. The rest of the application uses
//! the uniform functions and types exposed here and never branches on the host
//! operating system.
//!
//! Native system-proxy configuration uses no external helper binaries.
//!
//! The proxy-setting code lives **only here, in the manager**, so no client has
//! to duplicate it. Linux proxy settings are per-user, so the client passes its
//! [`SessionContext`] and the manager re-invokes itself dropped to that user.
//! Windows similarly launches the helper in the active console session. macOS
//! settings are machine-wide and are edited directly through SystemConfiguration;
//! the exact prior PAC values are snapshotted and restored on disconnect.
//!
//! Two desktops are supported on Linux:
//!   * GNOME (and derivatives) via GSettings `org.gnome.system.proxy`, reached
//!     through `libgio` loaded at runtime with `dlopen` (so a headless build
//!     without libgio still runs — GNOME proxy is simply skipped).
//!   * KDE via `~/.config/kioslaverc`.
//!
//! On Windows the same split applies: the manager runs as LocalSystem, but the
//! WinINET proxy settings live in the interactive user's hive
//! (`HKCU\…\Internet Settings`) and the change-notification must fire in that
//! user's session. So `apply_for_session` re-launches `geph __apply-proxy` in
//! the active console session via `CreateProcessAsUserW`, and `apply` (running
//! as that user) writes the `AutoConfigURL` value and refreshes WinINET.

use geph5_misc_rpc::manager_control::SessionContext;

/// Configure the system PAC setting. Linux needs the originating desktop
/// session; Windows targets the active console session and macOS edits the
/// machine-wide SystemConfiguration preferences directly.
pub(crate) fn set_system_proxy(
    session: Option<&SessionContext>,
    connected: bool,
    pac_url: &str,
) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match session {
            Some(session) => linux::apply_for_session(session, connected, pac_url),
            None => Ok(()),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = session;
        macos_proxy::apply(connected, pac_url)
    }
    #[cfg(target_os = "windows")]
    {
        if session.is_none() && !connected {
            return Ok(());
        }
        let default_session = SessionContext::default();
        windows::apply_for_session(session.unwrap_or(&default_session), connected, pac_url)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (session, connected, pac_url);
        compile_error!("geph5-app supports only Linux, macOS, and Windows");
    }
}

/// Body of the internal `__apply-proxy` subcommand — already running as the
/// target user, so it just does the work directly.
pub(crate) fn apply_proxy_in_process(connected: bool, pac_url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::apply(connected, pac_url)
    }
    #[cfg(target_os = "windows")]
    {
        windows::apply(connected, pac_url)
    }
    #[cfg(target_os = "macos")]
    {
        macos_proxy::apply(connected, pac_url)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (connected, pac_url);
        compile_error!("geph5-app supports only Linux, macOS, and Windows");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        ffi::{CStr, CString},
        os::raw::{c_char, c_int, c_void},
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    use anyhow::Context as _;
    use std::os::unix::process::CommandExt as _;

    use geph5_misc_rpc::manager_control::SessionContext;

    /// Configure the proxy for `session`. If we are already that user, apply in
    /// process; if we are root, re-invoke ourselves dropped to the target user.
    pub(super) fn apply_for_session(
        session: &SessionContext,
        connected: bool,
        url: &str,
    ) -> anyhow::Result<()> {
        let euid = unsafe { libc::geteuid() };
        if euid == session.uid {
            return apply(connected, url);
        }
        if euid != 0 {
            anyhow::bail!(
                "cannot configure proxy for uid {} while running as uid {}",
                session.uid,
                euid
            );
        }
        let gid = session
            .gid
            .or_else(|| passwd_info(session.uid).map(|(g, _)| g))
            .context("could not resolve gid for target user")?;
        let home = session
            .home
            .clone()
            .or_else(|| passwd_info(session.uid).map(|(_, h)| h.to_string_lossy().into_owned()))
            .context("could not resolve home for target user")?;
        let xdg = session
            .xdg_runtime_dir
            .clone()
            .unwrap_or_else(|| format!("/run/user/{}", session.uid));
        let dbus = session
            .dbus_session_bus_address
            .clone()
            .unwrap_or_else(|| format!("unix:path=/run/user/{}/bus", session.uid));

        let exe = std::env::current_exe().context("current_exe")?;
        let mut cmd = std::process::Command::new(exe);
        cmd.uid(session.uid)
            .gid(gid)
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", xdg)
            .env("DBUS_SESSION_BUS_ADDRESS", dbus)
            .arg("__apply-proxy")
            .arg(if connected { "on" } else { "off" });
        if connected {
            cmd.arg(url);
        }
        let status = cmd
            .status()
            .context("spawning __apply-proxy as target user")?;
        if !status.success() {
            anyhow::bail!("__apply-proxy exited with {status}");
        }
        Ok(())
    }

    fn passwd_info(uid: u32) -> Option<(u32, PathBuf)> {
        unsafe {
            let pw = libc::getpwuid(uid);
            if pw.is_null() {
                return None;
            }
            let gid = (*pw).pw_gid;
            let home = CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned();
            Some((gid, PathBuf::from(home)))
        }
    }

    /// Apply to whichever desktops are present. Absent desktops are no-ops.
    pub(super) fn apply(connected: bool, url: &str) -> anyhow::Result<()> {
        let mut errors = Vec::new();
        if let Err(e) = apply_gnome(connected, url) {
            errors.push(format!("gnome: {e:#}"));
        }
        if let Err(e) = apply_kde(connected, url) {
            errors.push(format!("kde: {e:#}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("{}", errors.join("; "))
        }
    }

    // ---- GNOME via GSettings (libgio, dlopen'd) ----

    struct Gio {
        schema_source_get_default: unsafe extern "C" fn() -> *mut c_void,
        schema_source_lookup:
            unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> *mut c_void,
        schema_unref: unsafe extern "C" fn(*mut c_void),
        settings_new: unsafe extern "C" fn(*const c_char) -> *mut c_void,
        settings_set_string:
            unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
        settings_sync: unsafe extern "C" fn(),
        object_unref: unsafe extern "C" fn(*mut c_void),
    }

    fn gio() -> Option<&'static Gio> {
        static GIO: OnceLock<Option<Gio>> = OnceLock::new();
        GIO.get_or_init(|| unsafe {
            // RTLD_GLOBAL so dependent symbols (g_object_unref in libgobject) resolve.
            let handle = libc::dlopen(
                c"libgio-2.0.so.0".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_GLOBAL,
            );
            if handle.is_null() {
                return None;
            }
            macro_rules! sym {
                ($name:literal, $ty:ty) => {{
                    let p = libc::dlsym(handle, $name.as_ptr());
                    if p.is_null() {
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, $ty>(p)
                }};
            }
            Some(Gio {
                schema_source_get_default: sym!(
                    c"g_settings_schema_source_get_default",
                    unsafe extern "C" fn() -> *mut c_void
                ),
                schema_source_lookup: sym!(
                    c"g_settings_schema_source_lookup",
                    unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> *mut c_void
                ),
                schema_unref: sym!(
                    c"g_settings_schema_unref",
                    unsafe extern "C" fn(*mut c_void)
                ),
                settings_new: sym!(
                    c"g_settings_new",
                    unsafe extern "C" fn(*const c_char) -> *mut c_void
                ),
                settings_set_string: sym!(
                    c"g_settings_set_string",
                    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int
                ),
                settings_sync: sym!(c"g_settings_sync", unsafe extern "C" fn()),
                object_unref: sym!(c"g_object_unref", unsafe extern "C" fn(*mut c_void)),
            })
        })
        .as_ref()
    }

    fn apply_gnome(connected: bool, url: &str) -> anyhow::Result<()> {
        let gio = match gio() {
            Some(g) => g,
            None => {
                tracing::debug!("libgio unavailable; skipping GNOME proxy");
                return Ok(());
            }
        };
        const SCHEMA: &CStr = c"org.gnome.system.proxy";
        unsafe {
            let source = (gio.schema_source_get_default)();
            if source.is_null() {
                return Ok(());
            }
            // Guard: creating a GSettings for a missing schema would abort the process.
            let schema = (gio.schema_source_lookup)(source, SCHEMA.as_ptr(), 1);
            if schema.is_null() {
                tracing::debug!("org.gnome.system.proxy schema absent; skipping GNOME proxy");
                return Ok(());
            }
            (gio.schema_unref)(schema);

            let settings = (gio.settings_new)(SCHEMA.as_ptr());
            if settings.is_null() {
                anyhow::bail!("g_settings_new returned null");
            }
            let set = |key: &CStr, val: &CStr| {
                (gio.settings_set_string)(settings, key.as_ptr(), val.as_ptr())
            };
            if connected {
                set(c"mode", c"auto");
                let url_c = CString::new(url).context("PAC url has a NUL")?;
                set(c"autoconfig-url", &url_c);
            } else {
                set(c"mode", c"none");
            }
            (gio.settings_sync)();
            (gio.object_unref)(settings);
        }
        Ok(())
    }

    // ---- KDE via kioslaverc ----

    fn apply_kde(connected: bool, url: &str) -> anyhow::Result<()> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("no HOME")?;
        let config = home.join(".config");
        let rc = config.join("kioslaverc");
        // Only touch KDE config if KDE is actually present, to avoid littering.
        if !rc.exists() && !config.join("kdeglobals").exists() {
            tracing::debug!("no KDE markers; skipping KDE proxy");
            return Ok(());
        }
        // ProxyType: 0 = none, 1 = manual, 2 = PAC script.
        let updates: Vec<(&str, &str)> = if connected {
            vec![("ProxyType", "2"), ("Proxy Config Script", url)]
        } else {
            vec![("ProxyType", "0")]
        };
        update_ini_section(&rc, "Proxy Settings", &updates)
    }

    /// Minimal INI editor: update/insert `key=value` pairs inside one section,
    /// preserving every other line and section verbatim.
    fn update_ini_section(path: &Path, section: &str, kvs: &[(&str, &str)]) -> anyhow::Result<()> {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
        let header = format!("[{section}]");
        let mut remaining: Vec<(&str, &str)> = kvs.to_vec();

        if let Some(start) = lines.iter().position(|l| l.trim() == header) {
            let end = lines[start + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map(|p| start + 1 + p)
                .unwrap_or(lines.len());
            for line in lines.iter_mut().take(end).skip(start + 1) {
                if let Some(eq) = line.find('=') {
                    let key = line[..eq].trim().to_string();
                    if let Some(pos) = remaining.iter().position(|(k, _)| *k == key) {
                        let (_, v) = remaining.remove(pos);
                        *line = format!("{key}={v}");
                    }
                }
            }
            for (k, v) in remaining.iter().rev() {
                lines.insert(end, format!("{k}={v}"));
            }
        } else {
            if lines.last().is_some_and(|l| !l.is_empty()) {
                lines.push(String::new());
            }
            lines.push(header);
            for (k, v) in &remaining {
                lines.push(format!("{k}={v}"));
            }
        }

        let mut out = lines.join("\n");
        out.push('\n');
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use anyhow::bail;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE,
    };
    use windows_sys::Win32::Networking::WinInet::{
        INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED, InternetSetOptionW,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
    };
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSGetActiveConsoleSessionId, WTSQueryUserToken,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessAsUserW, GetExitCodeProcess, PROCESS_INFORMATION,
        STARTUPINFOW, WaitForSingleObject,
    };

    use geph5_misc_rpc::manager_control::SessionContext;

    /// A NUL-terminated UTF-16 string, as the Win32 wide APIs expect.
    fn wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Manager-side (LocalSystem): run the proxy edit inside the interactive
    /// desktop user's session, so it writes *that* user's `HKCU` and the WinINET
    /// refresh reaches *that* session's apps. The Windows analogue of Linux's
    /// privilege-dropping re-invoke; `session` is unused because the manager
    /// targets the active console session itself. Best-effort: a machine with no
    /// interactive user logged in is a no-op, not an error.
    pub(super) fn apply_for_session(
        _session: &SessionContext,
        connected: bool,
        url: &str,
    ) -> anyhow::Result<()> {
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        // 0xFFFFFFFF: no session is currently attached to the physical console.
        if session_id == u32::MAX {
            tracing::debug!("no active console session; skipping system proxy config");
            return Ok(());
        }

        // Token for the user logged into the console session. Needs
        // SeTcbPrivilege, which LocalSystem holds; failure means no interactive
        // user (e.g. the login screen), so we skip rather than fail.
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { WTSQueryUserToken(session_id, &mut token) } == 0 {
            tracing::debug!(
                err = %std::io::Error::last_os_error(),
                "no interactive user token; skipping system proxy config"
            );
            return Ok(());
        }

        let result = spawn_apply_proxy(token, connected, url);
        unsafe { CloseHandle(token) };
        result
    }

    /// `CreateProcessAsUserW(token, …)` to run `geph __apply-proxy on|off [url]`
    /// in the target user's session, then wait for it and check its exit code.
    fn spawn_apply_proxy(token: HANDLE, connected: bool, url: &str) -> anyhow::Result<()> {
        let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("current_exe: {e}"))?;
        let exe_str = exe.to_string_lossy();
        // CreateProcess treats the first token of the command line as argv[0]
        // even when an application name is given, so include the exe there too.
        let cmdline = if connected {
            format!("\"{exe_str}\" __apply-proxy on \"{url}\"")
        } else {
            format!("\"{exe_str}\" __apply-proxy off")
        };

        let app_w = wide(&exe_str);
        let mut cmdline_w = wide(&cmdline);
        // Give the child a valid window station/desktop in the user's session.
        let mut desktop_w = wide("winsta0\\default");

        unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = desktop_w.as_mut_ptr();
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();

            let ok = CreateProcessAsUserW(
                token,
                app_w.as_ptr(),
                cmdline_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0, // bInheritHandles
                CREATE_NO_WINDOW,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            );
            if ok == 0 {
                bail!(
                    "CreateProcessAsUserW failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            CloseHandle(pi.hThread);
            WaitForSingleObject(pi.hProcess, u32::MAX);
            let mut code: u32 = 0;
            GetExitCodeProcess(pi.hProcess, &mut code);
            CloseHandle(pi.hProcess);
            if code != 0 {
                bail!("__apply-proxy child exited with code {code}");
            }
        }
        Ok(())
    }

    /// Runs as the desktop user (invoked via the hidden `__apply-proxy`
    /// subcommand): write or clear the WinINET PAC URL, then refresh WinINET so
    /// running apps pick up the change without a restart.
    pub(super) fn apply(connected: bool, url: &str) -> anyhow::Result<()> {
        const SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
        let subkey_w = wide(SUBKEY);
        let value_w = wide("AutoConfigURL");

        unsafe {
            let mut hkey: HKEY = std::ptr::null_mut();
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey_w.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            );
            if rc != ERROR_SUCCESS {
                bail!(
                    "RegCreateKeyExW failed: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }

            // Touch only AutoConfigURL (the PAC); leave the manual-proxy keys
            // (ProxyEnable/ProxyServer) alone — PAC works independently of them.
            let rc = if connected {
                let data = wide(url);
                RegSetValueExW(
                    hkey,
                    value_w.as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr() as *const u8,
                    (data.len() * 2) as u32, // bytes, including the NUL terminator
                )
            } else {
                let rc = RegDeleteValueW(hkey, value_w.as_ptr());
                // Clearing an already-absent value is success (idempotent).
                if rc == ERROR_FILE_NOT_FOUND {
                    ERROR_SUCCESS
                } else {
                    rc
                }
            };
            RegCloseKey(hkey);
            if rc != ERROR_SUCCESS {
                bail!(
                    "updating AutoConfigURL failed: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }

            // Signal WinINET that settings changed and to re-read them now.
            InternetSetOptionW(
                std::ptr::null_mut(),
                INTERNET_OPTION_SETTINGS_CHANGED,
                std::ptr::null(),
                0,
            );
            InternetSetOptionW(
                std::ptr::null_mut(),
                INTERNET_OPTION_REFRESH,
                std::ptr::null(),
                0,
            );
        }
        Ok(())
    }
}

// ---- platform-neutral application boundary ----

use std::{
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command},
};

use anyhow::Context as _;
use geph5_misc_rpc::{
    client_config::Config, client_control::ControlClient, manager_control::GephCtlService,
};

use crate::manager::ManagerImpl;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EngineRole {
    Tunnel,
    Query,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum PacketMode {
    None,
    Fd(i32),
    Stdio,
}

pub(crate) struct EngineLaunch {
    pub role: EngineRole,
    pub config_path: PathBuf,
    #[allow(dead_code)]
    pub service_user: Option<(u32, u32)>,
    pub packet_mode: PacketMode,
    pub bind_indices: Option<(u32, u32)>,
}

#[allow(dead_code)]
pub(crate) enum ChildTransport {
    None,
    Stdio {
        stdin: ChildStdin,
        stdout: ChildStdout,
    },
}

pub(crate) struct SpawnedEngine {
    pub child: Child,
    pub transport: ChildTransport,
}

pub(crate) fn state_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/var/lib/geph")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/geph")
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("geph")
    }
}

#[allow(dead_code)]
fn runtime_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/run/geph")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/var/run/geph")
    }
    #[cfg(target_os = "windows")]
    {
        // Windows IPC uses the named-pipe namespace; this value is never used.
        PathBuf::new()
    }
}

pub(crate) fn settings_path() -> PathBuf {
    state_dir().join("settings.json")
}

pub(crate) fn cache_dir() -> PathBuf {
    state_dir().join("cache")
}

#[allow(dead_code)]
fn engine_control_path(role: EngineRole) -> PathBuf {
    let name = match role {
        EngineRole::Tunnel => "engine/engine.sock",
        EngineRole::Query => "query/query.sock",
    };
    runtime_dir().join(name)
}

pub(crate) fn current_session() -> SessionContext {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        SessionContext {
            uid: unsafe { libc::geteuid() },
            gid: Some(unsafe { libc::getegid() }),
            home: std::env::var("HOME").ok(),
            dbus_session_bus_address: std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(),
            xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(),
        }
    }
    #[cfg(target_os = "windows")]
    {
        SessionContext::default()
    }
}

pub(crate) fn require_manager_privilege() -> anyhow::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if unsafe { libc::geteuid() } != 0 {
            anyhow::bail!("this command must be run as root (try: sudo ...)");
        }
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

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
            anyhow::bail!("this command must be run as Administrator");
        }
        Ok(())
    }
}

pub(crate) async fn shutdown_signal() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use tokio::signal::unix::{SignalKind, signal};
        async fn on(kind: SignalKind) {
            match signal(kind) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        }
        tokio::select! {
            _ = on(SignalKind::hangup()) => {},
            _ = on(SignalKind::interrupt()) => {},
            _ = on(SignalKind::terminate()) => {},
            _ = on(SignalKind::quit()) => {},
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub(crate) fn configure_engine_control(config: &mut Config, role: EngineRole) {
    config.control_listen = None;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        config.control_listen_unix = Some(engine_control_path(role));
    }
    #[cfg(target_os = "windows")]
    {
        config.control_listen_unix = None;
        config.control_listen_pipe = Some(engine_pipe_name(role).to_string());
    }
}

pub(crate) fn engine_control(role: EngineRole) -> ControlClient {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let dialer = nanorpc_sillad::DialerTransport(sillad::unix::UnixDialer {
        path: engine_control_path(role),
    });
    #[cfg(target_os = "windows")]
    let dialer = nanorpc_sillad::DialerTransport(sillad::windows_pipe::NamedPipeDialer {
        name: engine_pipe_name(role).to_string(),
    });
    ControlClient::from(dialer)
}

#[cfg(target_os = "windows")]
fn engine_pipe_name(role: EngineRole) -> &'static str {
    match role {
        EngineRole::Tunnel => r"\\.\pipe\geph-engine-control",
        EngineRole::Query => r"\\.\pipe\geph-query-control",
    }
}

pub(crate) async fn serve_manager(manager: ManagerImpl) -> anyhow::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = PathBuf::from(geph5_misc_rpc::manager_control::MANAGER_CONTROL_SOCK);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let listener = sillad::unix::UnixListener::bind(&path).await?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        tracing::info!(path = %path.display(), "geph manager listening for clients");
        nanorpc_sillad::rpc_serve(listener, GephCtlService(manager)).await?;
    }
    #[cfg(target_os = "windows")]
    {
        let name = geph5_misc_rpc::manager_control::MANAGER_CONTROL_PIPE;
        let listener = sillad::windows_pipe::NamedPipeListener::bind(
            name,
            Some(sillad::windows_pipe::SDDL_ALLOW_AUTHENTICATED),
        )?;
        tracing::info!(name, "geph manager listening for clients");
        nanorpc_sillad::rpc_serve(listener, GephCtlService(manager)).await?;
    }
    Ok(())
}

pub(crate) fn ensure_service_user() -> anyhow::Result<Option<(u32, u32)>> {
    #[cfg(target_os = "linux")]
    {
        const USER: &str = "geph5-daemon";
        if let Some(ids) = lookup_user(USER) {
            return Ok(Some(ids));
        }
        run_status(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                USER,
            ],
        )
        .context("creating geph5-daemon service user")?;
        Ok(Some(
            lookup_user(USER).context("geph5-daemon missing after useradd")?,
        ))
    }
    #[cfg(target_os = "macos")]
    {
        const USER: &str = "_geph";
        if let Some(ids) = lookup_user(USER) {
            return Ok(Some(ids));
        }
        let uid = (300..700u32)
            .find(|uid| unsafe { libc::getpwuid(*uid).is_null() })
            .context("no free uid for the _geph service user")?;
        let gid = 1u32;
        let path = format!("/Users/{USER}");
        run_status("dscl", &[".", "-create", &path])?;
        run_status(
            "dscl",
            &[".", "-create", &path, "UserShell", "/usr/bin/false"],
        )?;
        run_status(
            "dscl",
            &[".", "-create", &path, "RealName", "Geph VPN service"],
        )?;
        run_status(
            "dscl",
            &[".", "-create", &path, "UniqueID", &uid.to_string()],
        )?;
        run_status(
            "dscl",
            &[".", "-create", &path, "PrimaryGroupID", &gid.to_string()],
        )?;
        run_status(
            "dscl",
            &[".", "-create", &path, "NFSHomeDirectory", "/var/empty"],
        )?;
        run_status("dscl", &[".", "-create", &path, "Password", "*"])?;
        let _ = run_status("dscl", &[".", "-create", &path, "IsHidden", "1"]);
        let _ = run_status("dscacheutil", &["-flushcache"]);
        Ok(Some(
            lookup_user(USER).context("_geph missing after dscl create")?,
        ))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(None)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn lookup_user(name: &str) -> Option<(u32, u32)> {
    let name = std::ffi::CString::new(name).ok()?;
    unsafe {
        let passwd = libc::getpwnam(name.as_ptr());
        (!passwd.is_null()).then(|| ((*passwd).pw_uid, (*passwd).pw_gid))
    }
}

fn run_status(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{} {} failed: {}",
            program,
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn engine_binary_name() -> &'static str {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        "geph5-client"
    }
    #[cfg(target_os = "windows")]
    {
        "geph5-client.exe"
    }
}

pub(crate) fn engine_bin_path() -> PathBuf {
    if let Some(path) = std::env::var_os("GEPH_CLIENT_BIN") {
        return path.into();
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(sibling) = exe.parent().map(|dir| dir.join(engine_binary_name()))
        && sibling.is_file()
    {
        return sibling;
    }
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join(engine_binary_name()))
            .find(|path| path.is_file())
    {
        return found;
    }
    PathBuf::from(engine_binary_name())
}

fn engine_command() -> anyhow::Result<Command> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let path = engine_bin_path();
    #[cfg(target_os = "macos")]
    let path = staged_engine_bin()?;
    Ok(Command::new(path))
}

#[cfg(target_os = "macos")]
fn staged_engine_bin() -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let src = engine_bin_path();
    let dir = state_dir().join("bin");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    let dst = dir.join(engine_binary_name());
    let current = match (std::fs::metadata(&src), std::fs::metadata(&dst)) {
        (Ok(src), Ok(dst)) => match (src.modified(), dst.modified()) {
            (Ok(src), Ok(dst)) => dst >= src,
            _ => false,
        },
        _ => false,
    };
    if !current {
        // Stage via a temp file + rename rather than copying over `dst` in
        // place: an in-place copy leaves a window where a concurrent exec of
        // `dst` (e.g. the respawn loop of a manager still running across a pkg
        // upgrade) sees a half-written Mach-O. The kernel SIGKILLs that exec for
        // an invalid code signature and macOS caches the verdict against the
        // inode, so every later exec dies even after the copy completes. A
        // rename never exposes a partial file, and the fresh inode carries no
        // cached verdict.
        let tmp = dir.join(format!(
            ".{}.staging.{}",
            engine_binary_name(),
            std::process::id()
        ));
        std::fs::copy(&src, &tmp).with_context(|| {
            format!(
                "staging engine binary {} -> {}",
                src.display(),
                tmp.display()
            )
        })?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        let _ = chown_path(&tmp, 0, 0);
        std::fs::rename(&tmp, &dst).with_context(|| {
            format!(
                "activating staged engine binary {} -> {}",
                tmp.display(),
                dst.display()
            )
        })?;
    }
    Ok(dst)
}

pub(crate) fn spawn_engine(launch: EngineLaunch) -> anyhow::Result<SpawnedEngine> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        spawn_engine_posix(launch)
    }
    #[cfg(target_os = "windows")]
    {
        spawn_engine_windows(launch)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_engine_posix(launch: EngineLaunch) -> anyhow::Result<SpawnedEngine> {
    use std::os::unix::process::CommandExt;

    let socket_dir = engine_control_path(launch.role)
        .parent()
        .expect("engine control path has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&socket_dir)?;
    if let Some((uid, gid)) = launch.service_user {
        let _ = chown_path(&launch.config_path, uid, gid);
        let _ = chown_recursive(&cache_dir(), uid, gid);
        let _ = chown_path(&socket_dir, uid, gid);
    }

    let vpn_fd = match launch.packet_mode {
        PacketMode::None => None,
        PacketMode::Fd(fd) => Some(fd),
        PacketMode::Stdio => anyhow::bail!("stdio VPN transport is Windows-only"),
    };
    let mut command = engine_command()?;
    command.arg("--config").arg(&launch.config_path);
    if vpn_fd.is_some() {
        command.arg("--vpn-fd").arg("3");
    }
    if let Some((if4, if6)) = launch.bind_indices {
        command.env("GEPH_VPN_BIND_IF4", if4.to_string());
        command.env("GEPH_VPN_BIND_IF6", if6.to_string());
    }
    let service_user = launch.service_user;
    unsafe {
        command.pre_exec(move || child_pre_exec(service_user, vpn_fd));
    }
    let child = command.spawn().context(
        "could not spawn geph5-client (is it installed on PATH? or set GEPH_CLIENT_BIN)",
    )?;
    tracing::info!(
        pid = child.id(),
        role = ?launch.role,
        vpn = vpn_fd.is_some(),
        "spawned geph5-client engine"
    );
    Ok(SpawnedEngine {
        child,
        transport: ChildTransport::None,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn child_pre_exec(service_user: Option<(u32, u32)>, vpn_fd: Option<i32>) -> std::io::Result<()> {
    let error = std::io::Error::last_os_error;
    if let Some((uid, gid)) = service_user {
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            return Err(error());
        }
        if unsafe { libc::setgid(gid) } != 0 || unsafe { libc::setuid(uid) } != 0 {
            return Err(error());
        }
    }
    if let Some(fd) = vpn_fd
        && unsafe { libc::dup2(fd, 3) } < 0
    {
        return Err(error());
    }
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(1, libc::SIGTERM, 0, 0, 0) } != 0 {
        return Err(error());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn chown_path(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn chown_recursive(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    chown_path(path, uid, gid)?;
    if metadata.file_type().is_dir() {
        for entry in std::fs::read_dir(path)? {
            chown_recursive(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn spawn_engine_windows(launch: EngineLaunch) -> anyhow::Result<SpawnedEngine> {
    use std::os::windows::process::CommandExt;

    let mut command = engine_command()?;
    command.arg("--config").arg(&launch.config_path);
    let transport = match launch.packet_mode {
        PacketMode::None => false,
        PacketMode::Stdio => {
            command.arg("--stdio-vpn");
            command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            true
        }
        PacketMode::Fd(_) => anyhow::bail!("fd VPN transport is not supported on Windows"),
    };
    if let Some((if4, if6)) = launch.bind_indices {
        command.env("GEPH_VPN_BIND_IF4", if4.to_string());
        command.env("GEPH_VPN_BIND_IF6", if6.to_string());
    }
    command.creation_flags(0x08000000);
    let child = command.spawn().context(
        "could not spawn geph5-client (is it installed on PATH? or set GEPH_CLIENT_BIN)",
    )?;
    assign_child_to_reaper_job(&child);
    let mut child = scopeguard::guard(child, |mut child| {
        let _ = child.kill();
        let _ = child.wait();
    });
    let child_transport = if transport {
        ChildTransport::Stdio {
            stdin: child.stdin.take().context("child stdin missing")?,
            stdout: child.stdout.take().context("child stdout missing")?,
        }
    } else {
        ChildTransport::None
    };
    tracing::info!(
        pid = child.id(),
        role = ?launch.role,
        vpn = transport,
        "spawned geph5-client engine"
    );
    Ok(SpawnedEngine {
        child: scopeguard::ScopeGuard::into_inner(child),
        transport: child_transport,
    })
}

#[cfg(target_os = "windows")]
fn assign_child_to_reaper_job(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of_val(&info) as u32,
        ) == 0
        {
            tracing::warn!(
                err = %std::io::Error::last_os_error(),
                "could not configure the engine reaper job"
            );
        }
        job as usize
    });
    if job != 0
        && unsafe { AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE) } == 0
    {
        tracing::warn!(
            pid = child.id(),
            err = %std::io::Error::last_os_error(),
            "could not assign engine child to reaper job"
        );
    }
}

pub(crate) fn kill_child(mut child: Child) {
    let pid = child.id();
    let _ = child.kill();
    let _ = child.wait();
    tracing::info!(pid, "killed child geph5-client");
}

pub(crate) fn register_manager() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        const NAME: &str = "geph-manager.service";
        const PATH: &str = "/etc/systemd/system/geph-manager.service";
        if !Path::new("/run/systemd/system").is_dir() {
            anyhow::bail!("systemd does not appear to be running");
        }
        let exe = current_exe_utf8()?;
        let unit = format!(
            "[Unit]\nDescription=Geph5 privileged manager\nAfter=network-online.target\n\
             Wants=network-online.target\n\n[Service]\nExecStart={exe} manager\nRestart=always\n\
             RestartSec=2\n\n[Install]\nWantedBy=multi-user.target\n"
        );
        std::fs::write(PATH, unit).with_context(|| format!("writing {PATH}"))?;
        run_status("systemctl", &["daemon-reload"])?;
        run_status("systemctl", &["enable", NAME])?;
        run_status("systemctl", &["restart", NAME])?;
        println!("Registered and started {NAME}.");
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        const LABEL: &str = "io.geph.manager";
        const LEGACY: &str = "io.geph.daemon";
        const PLIST: &str = "/Library/LaunchDaemons/io.geph.manager.plist";
        let exe = current_exe_utf8()?;
        let plist = launchd_plist(&exe);
        let _ = launchctl(&["bootout", &format!("system/{LEGACY}")]);
        let _ = launchctl(&["bootout", &format!("system/{LABEL}")]);
        std::fs::write(PLIST, plist).with_context(|| format!("writing {PLIST}"))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(PLIST, std::fs::Permissions::from_mode(0o644))?;
        let path = std::ffi::CString::new(PLIST)?;
        if unsafe { libc::chown(path.as_ptr(), 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error()).context("chown launchd plist");
        }
        launchctl(&["bootstrap", "system", PLIST])?;
        launchctl(&["enable", &format!("system/{LABEL}")])?;
        launchctl(&["kickstart", "-k", &format!("system/{LABEL}")])?;
        println!("Registered and started {LABEL}.");
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        const NAME: &str = "Geph Manager";
        let exe = current_exe_utf8()?;
        let xml_path = std::env::temp_dir().join("geph-manager-task.xml");
        write_utf16le(&xml_path, &task_xml(&exe))?;
        let create = run_status(
            "schtasks",
            &[
                "/Create",
                "/TN",
                NAME,
                "/XML",
                xml_path.to_str().context("temporary path is not UTF-8")?,
                "/F",
            ],
        );
        let _ = std::fs::remove_file(&xml_path);
        create?;
        run_status("schtasks", &["/Run", "/TN", NAME])?;
        println!("Registered and started the \"{NAME}\" scheduled task.");
        Ok(())
    }
}

pub(crate) fn unregister_manager() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        const NAME: &str = "geph-manager.service";
        const PATH: &str = "/etc/systemd/system/geph-manager.service";
        let _ = run_status("systemctl", &["disable", "--now", NAME]);
        if Path::new(PATH).exists() {
            std::fs::remove_file(PATH).with_context(|| format!("removing {PATH}"))?;
        }
        run_status("systemctl", &["daemon-reload"])?;
        println!("Unregistered {NAME}.");
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        const LABEL: &str = "io.geph.manager";
        const LEGACY: &str = "io.geph.daemon";
        const PLIST: &str = "/Library/LaunchDaemons/io.geph.manager.plist";
        let _ = launchctl(&["bootout", &format!("system/{LABEL}")]);
        let _ = launchctl(&["bootout", &format!("system/{LEGACY}")]);
        match std::fs::remove_file(PLIST) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("removing {PLIST}")),
        }
        println!("Unregistered {LABEL}.");
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        const NAME: &str = "Geph Manager";
        let _ = run_status("schtasks", &["/End", "/TN", NAME]);
        let _ = run_status("schtasks", &["/Delete", "/TN", NAME, "/F"]);
        println!("Unregistered the \"{NAME}\" scheduled task.");
        Ok(())
    }
}

fn current_exe_utf8() -> anyhow::Result<String> {
    let exe = std::env::current_exe()
        .context("could not determine the geph5 executable path")?
        .canonicalize()
        .context("could not resolve the geph5 executable path")?;
    Ok(exe
        .to_str()
        .context("the geph5 executable path is not UTF-8")?
        .to_string())
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> anyhow::Result<()> {
    run_status("launchctl", args)
}

#[cfg(target_os = "macos")]
fn launchd_plist(exe: &str) -> String {
    let exe = xml_escape(exe);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.geph.manager</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>manager</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/var/log/geph-manager.log</string>
  <key>StandardErrorPath</key>
  <string>/var/log/geph-manager.log</string>
</dict>
</plist>
"#
    )
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "windows")]
fn task_xml(exe: &str) -> String {
    let exe = xml_escape(exe);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Geph5 privileged manager</Description></RegistrationInfo>
  <Triggers><BootTrigger><Enabled>true</Enabled></BootTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled><Hidden>false</Hidden>
    <RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>manager</Arguments></Exec></Actions>
</Task>
"#
    )
}

#[cfg(target_os = "windows")]
fn write_utf16le(path: &Path, value: &str) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(2 + value.len() * 2);
    bytes.extend_from_slice(&[0xff, 0xfe]);
    for unit in value.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

#[cfg(target_os = "macos")]
mod macos_proxy {
    use std::{collections::BTreeMap, path::PathBuf};

    use anyhow::Context as _;
    use core_foundation::{
        base::{CFType, TCFType},
        dictionary::{CFDictionary, CFMutableDictionary},
        number::CFNumber,
        string::CFString,
    };
    use serde::{Deserialize, Serialize};
    use system_configuration_sys::{
        core_foundation_sys::{
            array::{CFArrayGetCount, CFArrayGetValueAtIndex},
            base::{CFRelease, CFTypeRef},
        },
        network_configuration::{
            SCNetworkProtocolGetConfiguration, SCNetworkProtocolSetConfiguration,
            SCNetworkServiceCopyProtocol, SCNetworkServiceGetServiceID, SCNetworkServiceRef,
            SCNetworkSetCopyCurrent, SCNetworkSetCopyServices,
        },
        preferences::{
            SCPreferencesApplyChanges, SCPreferencesCommitChanges, SCPreferencesCreate,
            SCPreferencesLock, SCPreferencesRef, SCPreferencesUnlock,
        },
    };

    const SNAPSHOT_VERSION: u32 = 1;
    const SNAPSHOT_FILE: &str = "system-proxy-state.json";
    const PROXY_PROTOCOL: &str = "Proxies";
    const PAC_ENABLED: &str = "ProxyAutoConfigEnable";
    const PAC_URL: &str = "ProxyAutoConfigURLString";

    type ProxyDictionary = CFMutableDictionary<CFString, CFType>;

    #[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
    struct PacState {
        enabled: Option<i32>,
        url: Option<String>,
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    struct ProxySnapshot {
        version: u32,
        installed_url: String,
        services: BTreeMap<String, PacState>,
    }

    impl ProxySnapshot {
        fn new(installed_url: &str) -> Self {
            Self {
                version: SNAPSHOT_VERSION,
                installed_url: installed_url.to_string(),
                services: BTreeMap::new(),
            }
        }
    }

    struct Preferences {
        raw: SCPreferencesRef,
    }

    impl Preferences {
        fn open() -> anyhow::Result<Self> {
            let name = CFString::new("io.geph.manager");
            let raw = unsafe {
                SCPreferencesCreate(
                    std::ptr::null(),
                    name.as_concrete_TypeRef(),
                    std::ptr::null(),
                )
            };
            if raw.is_null() {
                anyhow::bail!("SCPreferencesCreate returned null");
            }
            if unsafe { SCPreferencesLock(raw, 1) } == 0 {
                unsafe { CFRelease(raw as CFTypeRef) };
                anyhow::bail!("could not lock SystemConfiguration preferences");
            }
            Ok(Self { raw })
        }

        fn commit(&self) -> anyhow::Result<()> {
            if unsafe { SCPreferencesCommitChanges(self.raw) } == 0 {
                anyhow::bail!("SCPreferencesCommitChanges failed");
            }
            if unsafe { SCPreferencesApplyChanges(self.raw) } == 0 {
                anyhow::bail!("SCPreferencesApplyChanges failed");
            }
            Ok(())
        }

        fn edit_services(
            &self,
            mut edit: impl FnMut(&str, &mut ProxyDictionary) -> anyhow::Result<bool>,
        ) -> anyhow::Result<usize> {
            let set = unsafe { SCNetworkSetCopyCurrent(self.raw) };
            if set.is_null() {
                anyhow::bail!("SCNetworkSetCopyCurrent returned null");
            }
            let set = scopeguard::guard(set, |set| unsafe {
                CFRelease(set as CFTypeRef);
            });
            let services = unsafe { SCNetworkSetCopyServices(*set) };
            if services.is_null() {
                anyhow::bail!("SCNetworkSetCopyServices returned null");
            }
            let services = scopeguard::guard(services, |services| unsafe {
                CFRelease(services as CFTypeRef);
            });
            let protocol_type = CFString::new(PROXY_PROTOCOL);
            let mut changed = 0usize;
            let count = unsafe { CFArrayGetCount(*services) };
            for index in 0..count {
                let service =
                    unsafe { CFArrayGetValueAtIndex(*services, index) } as SCNetworkServiceRef;
                if service.is_null() {
                    continue;
                }
                let service_id = unsafe { SCNetworkServiceGetServiceID(service) };
                if service_id.is_null() {
                    continue;
                }
                let service_id = unsafe { CFString::wrap_under_get_rule(service_id) }.to_string();
                let protocol = unsafe {
                    SCNetworkServiceCopyProtocol(service, protocol_type.as_concrete_TypeRef())
                };
                if protocol.is_null() {
                    continue;
                }
                let protocol = scopeguard::guard(protocol, |protocol| unsafe {
                    CFRelease(protocol as CFTypeRef);
                });
                let current = unsafe { SCNetworkProtocolGetConfiguration(*protocol) };
                let mut dictionary = if current.is_null() {
                    ProxyDictionary::new()
                } else {
                    let current =
                        unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(current) };
                    CFMutableDictionary::from(&current)
                };
                if edit(&service_id, &mut dictionary)? {
                    if unsafe {
                        SCNetworkProtocolSetConfiguration(
                            *protocol,
                            dictionary.as_concrete_TypeRef() as _,
                        )
                    } == 0
                    {
                        anyhow::bail!("could not stage PAC settings for service {service_id}");
                    }
                    changed += 1;
                }
            }
            Ok(changed)
        }
    }

    impl Drop for Preferences {
        fn drop(&mut self) {
            unsafe {
                SCPreferencesUnlock(self.raw);
                CFRelease(self.raw as CFTypeRef);
            }
        }
    }

    pub(super) fn apply(connected: bool, pac_url: &str) -> anyhow::Result<()> {
        if connected {
            enable(pac_url)
        } else {
            restore(pac_url)
        }
    }

    fn enable(pac_url: &str) -> anyhow::Result<()> {
        let prior_snapshot = load_snapshot()?;
        let recovering = prior_snapshot.is_some();
        let mut snapshot = prior_snapshot.unwrap_or_else(|| ProxySnapshot::new(pac_url));
        let preferences = Preferences::open()?;
        let changed = preferences.edit_services(|service_id, dictionary| {
            let current = read_state(dictionary);
            if !snapshot.services.contains_key(service_id) {
                let original = if !recovering && current.url.as_deref() == Some(pac_url) {
                    // A pre-snapshot Geph installation: do not preserve its stale
                    // loopback URL as the user's original setting.
                    PacState::default()
                } else {
                    current.clone()
                };
                snapshot.services.insert(service_id.to_string(), original);
            }
            let desired = PacState {
                enabled: Some(1),
                url: Some(pac_url.to_string()),
            };
            if current == desired {
                return Ok(false);
            }
            // Connected state is authoritative. Preserve the original snapshot
            // for disconnect, but repair any external PAC drift now.
            set_state(dictionary, &desired);
            Ok(true)
        })?;
        snapshot.installed_url = pac_url.to_string();
        save_snapshot(&snapshot)?;
        if changed > 0 {
            preferences.commit()?;
        }
        Ok(())
    }

    fn restore(fallback_url: &str) -> anyhow::Result<()> {
        let snapshot = match load_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(err = %error, "macOS proxy snapshot is unreadable; clearing only Geph's PAC URL");
                None
            }
        };
        let owned_url = snapshot
            .as_ref()
            .map(|snapshot| snapshot.installed_url.as_str())
            .filter(|url| !url.is_empty())
            .unwrap_or(fallback_url);
        if owned_url.is_empty() {
            return Ok(());
        }
        let preferences = Preferences::open()?;
        let changed = preferences.edit_services(|service_id, dictionary| {
            let current = read_state(dictionary);
            if current.url.as_deref() != Some(owned_url) {
                return Ok(false);
            }
            let original = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.services.get(service_id))
                .cloned()
                .unwrap_or_default();
            set_state(dictionary, &original);
            Ok(true)
        })?;
        if changed > 0 {
            preferences.commit()?;
        }
        remove_snapshot()?;
        Ok(())
    }

    fn read_state(dictionary: &ProxyDictionary) -> PacState {
        let enabled = dictionary
            .find(&CFString::new(PAC_ENABLED))
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|number| number.to_i32());
        let url = dictionary
            .find(&CFString::new(PAC_URL))
            .and_then(|value| value.downcast::<CFString>())
            .map(|url| url.to_string());
        PacState { enabled, url }
    }

    fn set_state(dictionary: &mut ProxyDictionary, state: &PacState) {
        let enabled_key = CFString::new(PAC_ENABLED);
        match state.enabled {
            Some(enabled) => dictionary.set(enabled_key, CFNumber::from(enabled).into_CFType()),
            None => dictionary.remove(enabled_key),
        }
        let url_key = CFString::new(PAC_URL);
        match &state.url {
            Some(url) => dictionary.set(url_key, CFString::new(url).into_CFType()),
            None => dictionary.remove(url_key),
        }
    }

    fn snapshot_path() -> PathBuf {
        super::state_dir().join(SNAPSHOT_FILE)
    }

    fn load_snapshot() -> anyhow::Result<Option<ProxySnapshot>> {
        let path = snapshot_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let snapshot: ProxySnapshot = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if snapshot.version != SNAPSHOT_VERSION {
            anyhow::bail!(
                "unsupported macOS proxy snapshot version {}",
                snapshot.version
            );
        }
        Ok(Some(snapshot))
    }

    fn save_snapshot(snapshot: &ProxySnapshot) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let path = snapshot_path();
        let parent = path.parent().expect("snapshot path has a parent");
        std::fs::create_dir_all(parent)?;
        let temp = parent.join(format!(".{SNAPSHOT_FILE}.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec_pretty(snapshot)?;
        std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&temp, &path).with_context(|| format!("installing {}", path.display()))?;
        Ok(())
    }

    fn remove_snapshot() -> anyhow::Result<()> {
        match std::fs::remove_file(snapshot_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("removing macOS proxy snapshot"),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn snapshot_preserves_absent_and_present_values() {
            let snapshot = ProxySnapshot {
                version: SNAPSHOT_VERSION,
                installed_url: "http://127.0.0.1:12223/proxy.pac".into(),
                services: BTreeMap::from([
                    ("wifi".into(), PacState::default()),
                    (
                        "ethernet".into(),
                        PacState {
                            enabled: Some(1),
                            url: Some("https://corp.example/proxy.pac".into()),
                        },
                    ),
                ]),
            };
            let encoded = serde_json::to_vec(&snapshot).unwrap();
            let decoded: ProxySnapshot = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded.services["wifi"], PacState::default());
            assert_eq!(decoded.services["ethernet"].enabled, Some(1));
            assert_eq!(
                decoded.services["ethernet"].url.as_deref(),
                Some("https://corp.example/proxy.pac")
            );
        }
    }
}
