use anyctx::AnyCtx;
use anyhow::Context;
use bytes::Bytes;
use dashmap::DashMap;
use futures_util::{AsyncReadExt, AsyncWriteExt};

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use smol::{
    channel::{Receiver, Sender},
    future::FutureExt as _,
    net::UdpSocket,
};
use std::{
    net::{IpAddr, Ipv4Addr},
    process::Command,
    sync::LazyLock,
};

use crate::{client_inner::open_conn, spoof_dns::fake_dns_respond, Config};

const FAKE_LOCAL_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 64, 89, 64));

pub fn vpn_whitelist(addr: IpAddr) {
    WHITELIST.entry(addr).or_insert_with(|| {
        tracing::warn!(addr = display(addr), "*** WHITELIST ***");
        SingleWhitelister::new(addr)
    });
}

#[allow(clippy::redundant_closure)]
fn setup_routing() -> anyhow::Result<()> {
    let cmd = include_str!("linux_routing_setup.sh");
    let mut child = Command::new("sh").arg("-c").arg(cmd).spawn().unwrap();
    child.wait().context("iptables was not set up properly")?;

    unsafe {
        libc::atexit(teardown_routing);
    }
    ctrlc::set_handler(|| teardown_routing())?;

    anyhow::Ok(())
}

static GEPH_DNS: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));

extern "C" fn teardown_routing() {
    tracing::debug!(
        "!!!!!!!!!!!!!!!!!!!!!!! teardown_routing starting !!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
    );
    WHITELIST.clear();
    std::env::set_var("GEPH_DNS", GEPH_DNS.lock().clone());
    let cmd = include_str!("linux_routing_teardown.sh");
    let mut child = Command::new("sh").arg("-c").arg(cmd).spawn().unwrap();
    child.wait().expect("iptables was not set up properly");
    std::process::exit(0);
}

pub(super) async fn packet_shuffle(
    ctx: AnyCtx<Config>,
    send_captured: Sender<Bytes>,
    recv_injected: Receiver<Bytes>,
) -> anyhow::Result<()> {
    let dns_proxy = UdpSocket::bind("127.0.0.1:0").await?;
    *GEPH_DNS.lock() = dns_proxy.local_addr()?.to_string();
    std::env::set_var("GEPH_DNS", GEPH_DNS.lock().clone());

    tracing::info!(
        addr = display(dns_proxy.local_addr().unwrap()),
        "start DNS proxy"
    );
    let dns_proxy_loop = async {
        loop {
            let mut buf = [0u8; 8192];
            let (n, src) = dns_proxy.recv_from(&mut buf).await?;
            tracing::trace!(n, src = display(src), "received DNS packet");
            if ctx.init().spoof_dns {
                if let Ok(resp) = fake_dns_respond(&ctx, &buf[..n]) {
                    let _ = dns_proxy.send_to(&resp, src).await;
                }
            } else {
                let dns_proxy = dns_proxy.clone();
                let ctx = ctx.clone();
                smolscale::spawn(async move {
                    let buf = &buf[..n];
                    let mut conn = open_conn(&ctx, "udp", "1.1.1.1:53").await?;
                    conn.write_all(&(buf.len() as u16).to_le_bytes()).await?;
                    conn.write_all(buf).await?;
                    let mut len_buf = [0u8; 2];
                    conn.read_exact(&mut len_buf).await?;
                    let len = u16::from_le_bytes(len_buf) as usize;
                    let mut buf = vec![0u8; len];
                    conn.read_exact(&mut buf).await?;
                    dns_proxy.send_to(&buf, src).await?;
                    anyhow::Ok(())
                })
                .detach();
            }
        }
    };

    use std::os::fd::{AsRawFd, FromRawFd};
    let tun_device = configure_tun_device();
    let fd_num = tun_device.as_raw_fd();
    let up_file = smol::Async::new(unsafe { std::fs::File::from_raw_fd(fd_num) })
        .context("cannot init up_file")?;

    // wait until we have a connection
    open_conn(&ctx, "", "").await?;
    setup_routing().unwrap();
    scopeguard::defer!(teardown_routing());
    let (mut read, mut write) = up_file.split();
    let inject = async {
        loop {
            let injected = recv_injected.recv().await?;
            tracing::trace!(n = injected.len(), "going to inject into the TUN");
            let _ = write.write(&injected).await?;
        }
    };
    let capture = async {
        let mut buf = vec![0u8; 65536];
        loop {
            let n = read.read(&mut buf).await?;
            let buf = &buf[..n];
            tracing::trace!(n, "captured packet from TUN");
            send_captured.send(Bytes::copy_from_slice(buf)).await?;
        }
    };
    inject.race(capture).race(dns_proxy_loop).await
}

#[cfg(target_os = "linux")]
fn configure_tun_device() -> tun::Device {
    let device = tun::Device::new(
        tun::Configuration::default()
            .tun_name("tun-geph")
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
