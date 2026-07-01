//! Per-OS locations for manager state.

use std::path::PathBuf;

/// System state directory where the manager persists settings and cache.
///
/// - Linux: `/var/lib/geph`
/// - macOS: `/Library/Application Support/geph`
/// - Windows: `%ProgramData%\geph` (falls back to `C:\ProgramData\geph`)
pub fn state_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/var/lib/geph")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/geph")
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("geph")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        PathBuf::from("/var/lib/geph")
    }
}

/// Runtime directory for transient IPC sockets.
///
/// Per the FHS, runtime sockets belong on a tmpfs that is cleared at boot — not
/// in `/var/lib` (persistent state) and not in `/dev/shm` (POSIX shared memory).
/// No payload traverses the socket inode, so tmpfs-vs-disk is irrelevant to
/// throughput; the wins are FHS-correctness and no stale sockets surviving a
/// reboot/crash. Unused on Windows, which uses named pipes (their own namespace).
///
/// - Linux: `/run/geph`
/// - macOS: `/var/run/geph`
#[cfg(unix)]
pub fn runtime_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/var/run/geph")
    }
    #[cfg(not(target_os = "macos"))]
    {
        PathBuf::from("/run/geph")
    }
}

/// Path to the persisted settings file.
pub fn settings_path() -> PathBuf {
    state_dir().join("settings.json")
}

/// Cache directory handed to the child geph5-client.
pub fn cache_dir() -> PathBuf {
    state_dir().join("cache")
}
