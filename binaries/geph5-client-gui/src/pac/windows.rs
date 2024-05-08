
use anyhow::Context;
use std::net::SocketAddr;
use winapi::um::wininet::{
    InternetSetOptionA, INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED,
};
use winreg::enums::*;
use winreg::types::ToRegValue;
use winreg::RegKey;

const HTTP_PROXY_REG_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

fn set_registry_value(key: &RegKey, name: &str, value: &impl ToRegValue) -> anyhow::Result<()> {
    key.set_value(name, value)
        .context(format!("Failed to set registry value '{}'", name))
}

fn refresh_internet_options() {
    unsafe {
        InternetSetOptionA(
            std::ptr::null_mut(),
            INTERNET_OPTION_SETTINGS_CHANGED,
            std::ptr::null_mut(),
            0,
        );
        InternetSetOptionA(
            std::ptr::null_mut(),
            INTERNET_OPTION_REFRESH,
            std::ptr::null_mut(),
            0,
        );
    }
}

pub fn set_http_proxy(proxy: SocketAddr) -> anyhow::Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(HTTP_PROXY_REG_PATH, KEY_WRITE)
        .context("Failed to open registry key with write access")?;

    set_registry_value(
        &key,
        "ProxyServer",
        &format!("{}:{}", proxy.ip(), proxy.port()),
    )?;
    set_registry_value(&key, "ProxyEnable", &1u32)?;

    refresh_internet_options();

    Ok(())
}

pub fn unset_http_proxy() -> anyhow::Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(HTTP_PROXY_REG_PATH, KEY_WRITE)
        .context("Failed to open registry key with write access")?;

    set_registry_value(&key, "ProxyServer", &"")?;
    set_registry_value(&key, "ProxyEnable", &0u32)?;

    refresh_internet_options();

    Ok(())
}
