use std::net::SocketAddr;

pub fn set_http_proxy(proxy: SocketAddr) -> anyhow::Result<()> {
    let shell_src = include_str!("macos_set_proxy.sh");
    std::env::set_var("proxy_server", proxy.ip().to_string());
    std::env::set_var("port", proxy.port().to_string());
    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg(shell_src)
        .status()?;
    if !output.success() {
        return Err(anyhow::anyhow!("Failed to set proxy"));
    }
    Ok(())
}

pub fn unset_http_proxy() -> anyhow::Result<()> {
    let shell_src = include_str!("macos_unset_proxy.sh");
    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg(shell_src)
        .status()?;
    if !output.success() {
        return Err(anyhow::anyhow!("Failed to unset proxy"));
    }
    Ok(())
}
