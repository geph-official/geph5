use std::{net::SocketAddr, process::Command};

use anyhow::Context;

pub fn set_http_proxy(proxy: SocketAddr) -> anyhow::Result<()> {
    let gsettings_result = Command::new("gsettings")
        .args(["set", "org.gnome.system.proxy.http", "enabled", "true"])
        .output();

    if gsettings_result.is_err() {
        return Err(anyhow::Error::new(gsettings_result.err().unwrap())
            .context("Failed to enable HTTP proxy setting"));
    } else {
        gsettings_result.context("Failed to enable HTTP proxy setting")?;
        Command::new("gsettings")
            .args([
                "set",
                "org.gnome.system.proxy.http",
                "host",
                proxy.ip().to_string().as_str(),
            ])
            .output()
            .context("Failed to set HTTP proxy host")?;
        Command::new("gsettings")
            .args([
                "set",
                "org.gnome.system.proxy.http",
                "port",
                &proxy.port().to_string(),
            ])
            .output()
            .context("Failed to set HTTP proxy port")?;
        Command::new("gsettings")
            .args(["set", "org.gnome.system.proxy.https", "enabled", "true"])
            .output()
            .context("Failed to enable HTTPS proxy setting")?;
        Command::new("gsettings")
            .args([
                "set",
                "org.gnome.system.proxy.https",
                "host",
                proxy.ip().to_string().as_str(),
            ])
            .output()
            .context("Failed to set HTTPS proxy host")?;
        Command::new("gsettings")
            .args([
                "set",
                "org.gnome.system.proxy.https",
                "port",
                &proxy.port().to_string(),
            ])
            .output()
            .context("Failed to set HTTPS proxy port")?;
        Command::new("gsettings")
            .args(["set", "org.gnome.system.proxy", "mode", "manual"])
            .output()
            .context("Failed to set proxy mode to manual")?;
    }

    Ok(())
}

pub fn unset_http_proxy() -> anyhow::Result<()> {
    let gsettings_result = Command::new("gsettings")
        .args(["set", "org.gnome.system.proxy", "mode", "none"])
        .output();

    gsettings_result.context("Failed to disable proxy")?;

    Ok(())
}
