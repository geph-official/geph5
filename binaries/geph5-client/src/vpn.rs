//! This module provides functionality for setting up a system-level VPN.

use anyctx::AnyCtx;
use anyhow::Context;
use bytes::Bytes;
use dashmap::DashMap;
use futures_util::{AsyncReadExt, AsyncWriteExt, TryFutureExt};
use ipstack_geph::{IpStack, IpStackConfig};
use itertools::Itertools;
use once_cell::sync::Lazy;
use smol::{
    channel::{Receiver, Sender},
    future::FutureExt as _,
};
use std::{
    net::{IpAddr, Ipv4Addr},
    process::Command,
};

use crate::{client_inner::open_conn, Config};

const FAKE_LOCAL_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 64, 89, 64));

pub struct VpnCapture {
    ipstack: IpStack,
}

impl VpnCapture {
    pub fn new(ctx: AnyCtx<Config>) -> Self {
        #[cfg(target_os = "linux")]
        {
            let (send_captured, recv_captured) = smol::channel::unbounded();
            let (send_injected, recv_injected) = smol::channel::unbounded();
            smolscale::spawn(
                packet_shuffle(ctx.clone(), send_captured, recv_injected)
                    .inspect_err(|e| tracing::error!(err = debug(e), "packet shuffle stopped")),
            )
            .detach();
            // TEST
            std::env::set_var("GEPH_DNS", "1.1.1.1");
            let ipstack = IpStack::new(IpStackConfig::default(), recv_captured, send_injected);

            Self { ipstack }
        }
        #[cfg(not(target_os = "linux"))]
        {
            todo!()
        }
    }

    pub fn ipstack(&self) -> &IpStack {
        &self.ipstack
    }
}

pub fn vpn_whitelist(addr: IpAddr) {
    tracing::warn!(addr = display(addr), "*** WHITELIST ***");
    WHITELIST.insert(addr, SingleWhitelister::new(addr));
}

#[cfg(target_os = "linux")]
pub fn setup_routing() -> anyhow::Result<()> {
    let cmd = include_str!("vpn/linux_routing_setup.sh");
    let mut child = Command::new("sh").arg("-c").arg(cmd).spawn().unwrap();
    child.wait().context("iptables was not set up properly")?;

    unsafe {
        libc::atexit(teardown_routing);
    }

    // scopeguard::defer!(teardown_routing());
    anyhow::Ok(())
}

#[cfg(target_os = "linux")]
extern "C" fn teardown_routing() {
    tracing::debug!("teardown_routing starting!");
    WHITELIST.clear();
    let cmd = include_str!("vpn/linux_routing_setup.sh")
        .lines()
        .filter(|l| l.contains("-D") || l.contains("del") || l.contains("flush"))
        .join("\n");
    let mut child = Command::new("sh").arg("-c").arg(cmd).spawn().unwrap();
    child.wait().expect("iptables was not set up properly");
}

#[cfg(target_os = "linux")]
async fn packet_shuffle(
    ctx: AnyCtx<Config>,
    send_captured: Sender<Bytes>,
    recv_injected: Receiver<Bytes>,
) -> anyhow::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let tun_device = configure_tun_device();
    let fd_num = tun_device.as_raw_fd();
    let up_file = smol::Async::new(unsafe { std::fs::File::from_raw_fd(fd_num) })
        .context("cannot init up_file")?;

    // wait until we have a connection
    open_conn(&ctx, "").await?;
    setup_routing().unwrap();
    let (mut read, mut write) = up_file.split();
    let up = async {
        loop {
            let injected = recv_injected.recv().await?;
            let _ = write.write(&injected).await?;
        }
    };
    let dn = async {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = read.read(&mut buf).await?;
            let buf = &buf[..n];
            tracing::trace!(n, "captured packet from TUN");
            send_captured.send(Bytes::copy_from_slice(buf)).await?;
        }
    };
    up.race(dn).await
}

#[cfg(target_os = "linux")]
fn configure_tun_device() -> tun::platform::Device {
    let device = tun::platform::Device::new(
        tun::Configuration::default()
            .name("tun-geph")
            .address(FAKE_LOCAL_ADDR)
            .netmask("255.255.255.0")
            .destination("100.64.0.1")
            .mtu(16384)
            .up(),
    )
    .expect("could not initialize TUN device");
    device
}

struct SingleWhitelister {
    dest: IpAddr,
}

impl Drop for SingleWhitelister {
    fn drop(&mut self) {
        tracing::debug!("DROPPING whitelist to {}", self.dest);
        Command::new("sh")
            .arg("-c")
            .arg(format!(
                "/usr/bin/env ip rule del to {} lookup main pref 1",
                self.dest
            ))
            .status()
            .expect("cannot run iptables");
    }
}

impl SingleWhitelister {
    fn new(dest: IpAddr) -> Self {
        Command::new("sh")
            .arg("-c")
            .arg(format!(
                "/usr/bin/env ip rule add to {} lookup main pref 1",
                dest
            ))
            .status()
            .expect("cannot run iptables");
        Self { dest }
    }
}

static WHITELIST: Lazy<DashMap<IpAddr, SingleWhitelister>> = Lazy::new(DashMap::new);
