//! Native system-proxy configuration — no external tools.
//!
//! The proxy-setting code lives **only here, in the manager**, so no client has
//! to duplicate it. Because proxy settings are per-user but the manager may run
//! as root, the client passes its [`SessionContext`] (uid + a few env vars) and
//! the manager configures *that* user's session: when running as root it
//! re-invokes itself (`geph __apply-proxy …`) dropped to the target user, so the
//! work happens with the right HOME / session bus / file ownership.
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

use crate::protocol::SessionContext;

/// Manager-side entry point: configure `session`'s system proxy. When we're root
/// the work is done as the target user; when we already are that user it runs in
/// process. Best-effort across desktops; only genuine failures return `Err`.
pub fn apply_for_session(
    session: &SessionContext,
    connected: bool,
    pac_url: &str,
) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::apply_for_session(session, connected, pac_url)
    }
    #[cfg(target_os = "windows")]
    {
        windows::apply_for_session(session, connected, pac_url)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (session, connected, pac_url);
        tracing::debug!("system proxy configuration not implemented on this platform");
        Ok(())
    }
}

/// Body of the internal `__apply-proxy` subcommand — already running as the
/// target user, so it just does the work directly.
pub fn apply_in_process(connected: bool, pac_url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::apply(connected, pac_url)
    }
    #[cfg(target_os = "windows")]
    {
        windows::apply(connected, pac_url)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (connected, pac_url);
        Ok(())
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

    use crate::protocol::SessionContext;

    /// Configure the proxy for `session`. If we are already that user, apply in
    /// process; if we are root, re-invoke ourselves dropped to the target user.
    pub fn apply_for_session(
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
    pub fn apply(connected: bool, url: &str) -> anyhow::Result<()> {
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
        schema_source_lookup: unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> *mut c_void,
        schema_unref: unsafe extern "C" fn(*mut c_void),
        settings_new: unsafe extern "C" fn(*const c_char) -> *mut c_void,
        settings_set_string: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
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
                ($name:literal) => {{
                    let p = libc::dlsym(handle, $name.as_ptr());
                    if p.is_null() {
                        return None;
                    }
                    std::mem::transmute(p)
                }};
            }
            Some(Gio {
                schema_source_get_default: sym!(c"g_settings_schema_source_get_default"),
                schema_source_lookup: sym!(c"g_settings_schema_source_lookup"),
                schema_unref: sym!(c"g_settings_schema_unref"),
                settings_new: sym!(c"g_settings_new"),
                settings_set_string: sym!(c"g_settings_set_string"),
                settings_sync: sym!(c"g_settings_sync"),
                object_unref: sym!(c"g_object_unref"),
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
        let home = std::env::var_os("HOME").map(PathBuf::from).context("no HOME")?;
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
        InternetSetOptionW, INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED,
    };
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSGetActiveConsoleSessionId, WTSQueryUserToken,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetExitCodeProcess, WaitForSingleObject, CREATE_NO_WINDOW,
        PROCESS_INFORMATION, STARTUPINFOW,
    };

    use crate::protocol::SessionContext;

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
    pub fn apply_for_session(
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
    pub fn apply(connected: bool, url: &str) -> anyhow::Result<()> {
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
