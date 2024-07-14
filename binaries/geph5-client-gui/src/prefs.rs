use std::path::PathBuf;

use anyhow::Context as _;
use app_dirs2::{get_app_root, AppDataType, AppInfo};
use moka::sync::Cache;
use once_cell::sync::Lazy;
use smol_str::SmolStr;

const APP_INFO: AppInfo = AppInfo {
    name: "Geph5",
    author: "Gephyra",
};

pub static PREF_DIR: Lazy<PathBuf> = Lazy::new(|| {
    let dir = get_app_root(AppDataType::UserConfig, &APP_INFO)
        .context("no config dir")
        .unwrap()
        .join("geph5-prefs");
    std::fs::create_dir_all(&dir).unwrap();
    dir
});

static PREF_CACHE: Lazy<Cache<SmolStr, SmolStr>> = Lazy::new(|| Cache::new(10000));

pub fn pref_write(key: &str, val: &str) -> anyhow::Result<()> {
    if let Ok(existing_val) = pref_read(key) {
        if val == existing_val {
            return Ok(());
        }
    }
    PREF_CACHE.remove(key);
    let key_path = PREF_DIR.join(key);
    std::fs::write(key_path, val.as_bytes())?;
    Ok(())
}

pub fn pref_read(key: &str) -> anyhow::Result<SmolStr> {
    PREF_CACHE
        .try_get_with(key.into(), || {
            let key_path = PREF_DIR.join(key);
            let contents = std::fs::read_to_string(key_path)?;
            anyhow::Ok(SmolStr::from(contents))
        })
        .map_err(|e| anyhow::anyhow!("{e}"))
}
