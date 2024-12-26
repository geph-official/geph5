//! This module provides functionality for setting up a system-level VPN.
#[cfg(target_os = "linux")]
mod linux;
use bytes::Bytes;
use crossbeam_queue::SegQueue;
use dashmap::DashMap;

use ipstack_geph::{IpStack, IpStackConfig};
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(any(target_os = "android", target_os = "ios"))]
mod dummy;

#[cfg(any(target_os = "android", target_os = "ios"))]
pub use dummy::*;

use std::{net::Ipv4Addr, time::Instant};

use anyctx::AnyCtx;
use anyhow::Context;
use futures_util::{AsyncReadExt, AsyncWriteExt, TryFutureExt as _};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

use rand::Rng;
use simple_dns::{Packet, QTYPE};
use smol::future::FutureExt;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

use crate::{
    client::CtxField,
    client_inner::open_conn,
    spoof_dns::{fake_dns_allocate, fake_dns_respond},
    taskpool::add_task,
    Config,
};

/// Force a particular packet to be sent through VPN mode, regardless of whether VPN mode is on.
pub async fn send_vpn_packet(ctx: &AnyCtx<Config>, bts: Bytes) {
    tracing::trace!(
        len = bts.len(),
        chan_len = ctx.get(VPN_CAPTURE).len(),
        "vpn forcing up"
    );
    ctx.get(VPN_CAPTURE).push((bts, Instant::now()));
    ctx.get(VPN_EVENT).notify_all();

    smol::future::yield_now().await;
}

/// Receive a packet from VPN mode, regardless of whether VPN mode is on.
pub async fn recv_vpn_packet(ctx: &AnyCtx<Config>) -> Bytes {
    ctx.get(VPN_EVENT)
        .wait_until(|| ctx.get(VPN_INJECT).pop())
        .await
}

static VPN_EVENT: CtxField<async_event::Event> = |_| async_event::Event::new();

static VPN_CAPTURE: CtxField<SegQueue<(Bytes, Instant)>> = |_| SegQueue::new();

static VPN_INJECT: CtxField<SegQueue<Bytes>> = |_| SegQueue::new();

pub async fn vpn_loop(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    let (send_captured, recv_captured) = smol::channel::unbounded();
    let (send_injected, recv_injected) = smol::channel::unbounded();

    let ipstack = IpStack::new(
        #[cfg(target_os = "ios")]
        IpStackConfig {
            mtu: 1450,
            tcp_timeout: std::time::Duration::from_secs(3600),
            udp_timeout: std::time::Duration::from_secs(600),
        },
        #[cfg(not(target_os = "ios"))]
        IpStackConfig::default(),
        recv_captured,
        send_injected,
    );
    let _shuffle = if ctx.init().vpn {
        smolscale::spawn(
            packet_shuffle(ctx.clone(), send_captured, recv_injected)
                .inspect_err(|e| tracing::warn!(e = debug(e), "packet_shuffle stopped")),
        )
    } else {
        let ctx = ctx.clone();
        smolscale::spawn(async move {
            let up_loop = async {
                loop {
                    let (bts, time) = ctx
                        .get(VPN_EVENT)
                        .wait_until(|| ctx.get(VPN_CAPTURE).pop())
                        .await;

                    tracing::trace!(
                        len = bts.len(),
                        elapsed = debug(time.elapsed()),
                        packet = display(hex::encode(&bts)),
                        "vpn shuffling up"
                    );
                    send_captured.send(bts).await?;
                }
            };
            let dn_loop = async {
                loop {
                    let bts = recv_injected.recv().await?;
                    tracing::trace!(len = bts.len(), "vpn shuffling down");
                    ctx.get(VPN_INJECT).push(bts);
                    ctx.get(VPN_EVENT).notify_all();

                    smol::future::yield_now().await;
                }
            };
            up_loop.race(dn_loop).await
        })
    };
    loop {
        let captured = ipstack
            .accept()
            .await
            .context("could not accept from ipstack")?;
        match captured {
            ipstack_geph::stream::IpStackStream::Tcp(captured) => {
                let peer_addr = captured.peer_addr();
                tracing::trace!(
                    local_addr = display(captured.local_addr()),
                    peer_addr = display(peer_addr),
                    "captured a TCP"
                );
                let ctx = ctx.clone();

                let task = smolscale::spawn(async move {
                    let tunneled = open_conn(&ctx, "tcp", &peer_addr.to_string()).await?;
                    tracing::trace!(peer_addr = display(peer_addr), "dialed through VPN");
                    let (read_tunneled, write_tunneled) = tunneled.split();
                    let (read_captured, write_captured) = captured.split();
                    smol::io::copy(read_tunneled, write_captured)
                        .race(smol::io::copy(read_captured, write_tunneled))
                        .await?;
                    anyhow::Ok(())
                });

                #[cfg(target_os = "ios")]
                {
                    add_task(task);
                }
                #[cfg(not(target_os = "ios"))]
                {
                    task.detach();
                }
            }
            ipstack_geph::stream::IpStackStream::Udp(captured) => {
                let peer_addr = captured.peer_addr();
                tracing::trace!(
                    local_addr = display(captured.local_addr()),
                    peer_addr = display(peer_addr),
                    "captured a UDP"
                );
                let peer_addr = if captured.peer_addr().port() == 53 {
                    "1.1.1.1:53".parse()?
                } else {
                    peer_addr
                };

                let ctx = ctx.clone();
                let task = smolscale::spawn::<anyhow::Result<()>>(async move {
                    if peer_addr.port() == 53 && ctx.init().spoof_dns {
                        // fakedns handling
                        loop {
                            let pkt = captured.recv().await?;
                            captured.send(&fake_dns_respond(&ctx, &pkt)?).await?;
                        }
                    } else {
                        let tunneled = open_conn(&ctx, "udp", &peer_addr.to_string()).await?;
                        let (mut read_tunneled, mut write_tunneled) = tunneled.split();
                        let up_loop = async {
                            loop {
                                let to_up = captured.recv().await?;
                                write_tunneled
                                    .write_all(&(to_up.len() as u16).to_le_bytes())
                                    .await?;
                                write_tunneled.write_all(&to_up).await?;
                                write_tunneled.flush().await?;
                            }
                        };
                        let dn_loop = async {
                            loop {
                                let mut len_buf = [0u8; 2];
                                read_tunneled.read_exact(&mut len_buf).await?;
                                let len = u16::from_le_bytes(len_buf) as usize;
                                let mut buf = vec![0u8; len];
                                read_tunneled.read_exact(&mut buf).await?;
                                captured.send(&buf).await?;
                            }
                        };
                        up_loop.race(dn_loop).await
                    }
                });
                #[cfg(target_os = "ios")]
                {
                    add_task(task);
                }
                #[cfg(not(target_os = "ios"))]
                {
                    task.detach();
                }
            }
            ipstack_geph::stream::IpStackStream::UnknownTransport(_) => {
                tracing::warn!("captured an UnknownTransport")
            }
            ipstack_geph::stream::IpStackStream::UnknownNetwork(_) => {
                tracing::warn!("captured an UnknownNetwork")
            }
        }
    }
}
