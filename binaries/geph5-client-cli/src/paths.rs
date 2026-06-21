//! Per-OS locations for daemon state.

use std::path::PathBuf;

/// System state directory where the daemon persists settings and cache.
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

/// Path to the persisted settings file.
pub fn settings_path() -> PathBuf {
    state_dir().join("settings.json")
}

/// Cache directory handed to the child geph5-client.
pub fn cache_dir() -> PathBuf {
    state_dir().join("cache")
}
