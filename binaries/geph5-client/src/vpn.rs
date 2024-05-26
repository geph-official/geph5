//! This module provides functionality for setting up a system-level VPN.

use std::{
    net::{IpAddr, Ipv4Addr},
    os::fd::FromRawFd,
};

use anyhow::Context;
use bytes::Bytes;
use futures_util::{AsyncReadExt, AsyncWriteExt, TryFutureExt};
use ipstack_geph::{IpStack, IpStackConfig};
use smol::{
    channel::{Receiver, Sender},
    future::FutureExt as _,
};

const FAKE_LOCAL_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 64, 89, 64));

pub struct VpnCapture {
    ipstack: IpStack,
}

impl VpnCapture {
    pub fn new() -> Self {
        let (send_captured, recv_captured) = smol::channel::unbounded();
        let (send_injected, recv_injected) = smol::channel::unbounded();
        smolscale::spawn(
            packet_shuffle(send_captured, recv_injected)
                .inspect_err(|e| tracing::error!(err = debug(e), "packet shuffle stopped")),
        )
        .detach();
        let ipstack = IpStack::new(IpStackConfig::default(), recv_captured, send_injected);
        Self { ipstack }
    }

    pub fn ipstack(&self) -> &IpStack {
        &self.ipstack
    }
}

async fn packet_shuffle(
    send_captured: Sender<Bytes>,
    recv_injected: Receiver<Bytes>,
) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    let tun_device = configure_tun_device();
    let fd_num = tun_device.as_raw_fd();
    let up_file = smol::Async::new(unsafe { std::fs::File::from_raw_fd(fd_num) })
        .context("cannot init up_file")?;
    let (mut read, mut write) = up_file.split();
    let up = async {
        loop {
            let injected = recv_injected.recv().await?;
            let _ = write.write(&injected).await?;
        }
    };
    let dn = async {
        let mut buf = vec![0u8; 65536];
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
