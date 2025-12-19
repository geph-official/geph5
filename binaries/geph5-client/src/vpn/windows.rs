#[cfg(feature = "windivert")]
mod windivert;

use std::{net::IpAddr, time::Duration};

use anyctx::AnyCtx;

use bytes::Bytes;

use dashmap::DashSet;

use once_cell::sync::Lazy;
use pnet_packet::{ipv4::Ipv4Packet, ipv6::Ipv6Packet};
use smol::channel::{Receiver, Sender};

use crate::{session::open_conn, Config};

pub(super) async fn packet_shuffle(
    ctx: AnyCtx<Config>,
    send_captured: Sender<Bytes>,
    recv_injected: Receiver<Bytes>,
) -> anyhow::Result<()> {
    smol::future::block_on(async {
        // wait until we have a connection
        while open_conn(&ctx, "", "").await.is_err() {
            smol::Timer::after(Duration::from_millis(100));
        }
    });
    #[cfg(feature = "windivert")]
    std::thread::spawn({
        let ctx = ctx.clone();
        move || up_shuffle(ctx, send_captured)
    });
    #[cfg(feature = "windivert")]
    std::thread::spawn({
        let ctx = ctx.clone();
        move || dn_shuffle(ctx, recv_injected)
    });
    smol::future::pending().await
}

#[cfg(feature = "windivert")]
fn up_shuffle(ctx: AnyCtx<Config>, send_captured: Sender<bytes::Bytes>) -> anyhow::Result<()> {
    let handle = windivert::PacketHandle::open("outbound and not loopback", -100)?;
    loop {
        let fallible = || {
            let raw_pkt = handle.receive()?;
            let dest = if let Some(ip_pkt) = Ipv4Packet::new(&raw_pkt) {
                IpAddr::V4(ip_pkt.get_destination())
            } else if let Some(ip_pkt) = Ipv6Packet::new(&raw_pkt) {
                IpAddr::V6(ip_pkt.get_destination())
            } else {
                anyhow::bail!("cannot parse packet as IPv4 or IPv6");
            };
            if WHITELIST.contains(&dest) {
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

#[cfg(feature = "windivert")]
fn dn_shuffle(ctx: AnyCtx<Config>, recv_injected: Receiver<bytes::Bytes>) -> anyhow::Result<()> {
    let handle = windivert::PacketHandle::open("false", -200)?;
    loop {
        let pkt = recv_injected.recv_blocking()?;
        handle.inject(&pkt, false)?
    }
}

static WHITELIST: Lazy<DashSet<IpAddr>> = Lazy::new(DashSet::new);

pub(super) fn vpn_whitelist(addr: IpAddr) {
    WHITELIST.insert(addr);
}
