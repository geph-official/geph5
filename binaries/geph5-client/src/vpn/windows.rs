mod windivert;

use std::{net::IpAddr, time::Duration};

use anyctx::AnyCtx;
use anyhow::Context;
use bytes::Bytes;
use clone_macro::clone;
use dashmap::DashSet;
use ipstack_geph::{IpStack, IpStackConfig};
use once_cell::sync::Lazy;
use smol::channel::{Receiver, Sender};

use crate::Config;

pub struct VpnCapture {
    ipstack: IpStack,
}

impl VpnCapture {
    pub fn new(ctx: AnyCtx<Config>) -> Self {
        let (send_captured, recv_captured) = smol::channel::unbounded();
        let (send_injected, recv_injected) = smol::channel::unbounded();
        std::thread::spawn(clone!([ctx], || up_shuffle(ctx, send_captured)
            .inspect_err(|e| tracing::error!(err = debug(e), "up_shuffle stopped"))));
        std::thread::spawn(clone!([ctx], || dn_shuffle(ctx, recv_injected)
            .inspect_err(|e| tracing::error!(err = debug(e), "dn_shuffle stopped"))));
        let ipstack = IpStack::new(IpStackConfig::default(), recv_captured, send_injected);
        Self { ipstack }
    }

    pub fn ipstack(&self) -> &IpStack {
        &self.ipstack
    }
}

fn up_shuffle(ctx: AnyCtx<Config>, send_captured: Sender<Bytes>) -> anyhow::Result<()> {
    let handle = windivert::PacketHandle::open("outbound and not loopback", -100)?;
    loop {
        let fallible = || {
            let raw_pkt = handle.receive()?;
            let ip_pkt = pnet_packet::ipv4::Ipv4Packet::new(&raw_pkt)
                .context("cannot parse packet as IPv4")?;
            if WHITELIST.contains(&IpAddr::V4(ip_pkt.get_destination())) {
                handle.inject(&raw_pkt, true)?;
                anyhow::Ok(None)
            } else {
                anyhow::Ok(Some(raw_pkt))
            }
        };
        match fallible() {
            Err(err) => {
                tracing::warn!(err = debug(err), "windivert up failed");
                std::thread::sleep(Duration::from_secs(1));
            }
            Ok(None) => {}
            Ok(Some(pkt)) => send_captured.send_blocking(pkt.into())?,
        }
    }
}

fn dn_shuffle(ctx: AnyCtx<Config>, recv_injected: Receiver<Bytes>) -> anyhow::Result<()> {
    let handle = windivert::PacketHandle::open("false", -200)?;
    loop {
        let pkt = recv_injected.recv_blocking()?;
        handle.inject(&pkt, false)?
    }
}

static WHITELIST: Lazy<DashSet<IpAddr>> = Lazy::new(DashSet::new);

pub fn vpn_whitelist(addr: IpAddr) {
    WHITELIST.insert(addr);
}
