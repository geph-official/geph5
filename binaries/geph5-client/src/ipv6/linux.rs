use std::process::Command;

use anyhow::Context;

// Try to disable IPv6 globally on Linux machines.
// This would need root privilages to work and will fail otherwise.
pub fn disable_ipv6() -> anyhow::Result<()> {
    Command::new("sh")
        .args([
            "-c",
            "echo 1 | tee /proc/sys/net/ipv6/conf/all/disable_ipv6",
        ])
        .output()
        .context("Failed to disable IPv6 globally")?;
    Ok(())
}

// Try to disable IPv6 globally on Linux machines.
// This would need root privilages to work and will fail otherwise.
pub fn enable_ipv6() -> anyhow::Result<()> {
    Command::new("sh")
        .args([
            "-c",
            "echo 0 | tee /proc/sys/net/ipv6/conf/all/disable_ipv6",
        ])
        .output()
        .context("Failed to enable IPv6 globally")?;
    Ok(())
}
