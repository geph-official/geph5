//! This module provides functionality for setting up a system-level VPN.
#[cfg(target_os = "linux")]
mod linux;
use async_channel::{Receiver, Sender};
use bytes::Bytes;

use ipstack_geph::{IpStack, IpStackConfig};
#[cfg(target_os = "linux")]
use linux::*;

#[cfg(any(target_os = "android", target_os = "ios"))]
mod dummy;

#[cfg(any(target_os = "android", target_os = "ios"))]
use dummy::*;

use std::{net::IpAddr, time::Instant};

use anyctx::AnyCtx;
use anyhow::Context;
use futures_concurrency::future::Race as _;
use futures_util::TryFutureExt as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

use crate::{
    Config, client::CtxField, litecopy::litecopy, session::open_conn, spoof_dns::fake_dns_respond,
    taskpool::add_task,
};

/// Whitelist a vpn address if needed
pub fn smart_vpn_whitelist(ctx: &AnyCtx<Config>, addr: IpAddr) {
    if ctx.init().vpn {
        vpn_whitelist(addr);
    }
}

/// Force a particular packet to be sent through VPN mode, regardless of whether VPN mode is on.
pub async fn send_vpn_packet(ctx: &AnyCtx<Config>, bts: Bytes) {
    tracing::trace!(
        len = bts.len(),
        chan_len = ctx.get(VPN_CAPTURE_CHAN).0.len(),
        "vpn forcing up"
    );
    let _ = ctx
        .get(VPN_CAPTURE_CHAN)
        .0
        .send((bts, Instant::now()))
        .await;
}

/// Receive a packet from VPN mode, regardless of whether VPN mode is on.
pub async fn recv_vpn_packet(ctx: &AnyCtx<Config>) -> Bytes {
    ctx.get(VPN_INJECT_CHAN).1.recv().await.unwrap()
}

static VPN_CAPTURE_CHAN: CtxField<(Sender<(Bytes, Instant)>, Receiver<(Bytes, Instant)>)> =
    |_| async_channel::bounded(100);

static VPN_INJECT_CHAN: CtxField<(Sender<Bytes>, Receiver<Bytes>)> =
    |_| async_channel::bounded(100);

pub async fn vpn_loop(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    let (send_captured, recv_captured) = async_channel::bounded(100);
    let (send_injected, recv_injected) = async_channel::bounded(100);

    let ipstack = IpStack::new(
        #[cfg(target_os = "ios")]
        IpStackConfig {
            mtu: 1450,
            tcp_timeout: std::time::Duration::from_secs(3600),
            udp_timeout: std::time::Duration::from_secs(600),
        },
        #[cfg(not(target_os = "ios"))]
        IpStackConfig {
            mtu: 16384,
            tcp_timeout: std::time::Duration::from_secs(3600),
            udp_timeout: std::time::Duration::from_secs(600),
        },
        recv_captured,
        send_injected,
    );
    let _shuffle = if ctx.init().vpn {
        geph5_rt::spawn(
            packet_shuffle(ctx.clone(), send_captured, recv_injected)
                .inspect_err(|e| tracing::warn!(e = debug(e), "packet_shuffle stopped")),
        )
    } else {
        let ctx = ctx.clone();
        geph5_rt::spawn(async move {
            let up_loop = async {
                loop {
                    let (bts, time) = ctx.get(VPN_CAPTURE_CHAN).1.recv().await?;

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

                    let _ = ctx.get(VPN_INJECT_CHAN).0.send(bts).await;
                }
            };
            (up_loop, dn_loop).race().await
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
                let ctx_clone = ctx.clone();

                let task = geph5_rt::spawn(async move {
                    let mut captured = captured;
                    let tunneled = match open_conn(&ctx_clone, "tcp", &peer_addr.to_string()).await {
                        Ok(tunneled) => tunneled,
                        Err(err) => {
                            // The exit reports the dial error (e.g. "Connection
                            // refused (os error 111)" vs "Network unreachable").
                            // If the destination actively refused us, RST so the
                            // app fails fast with "connection refused". Otherwise
                            // (unreachable/timeout) just drop the stream, leaving
                            // the SYN unanswered so the app's Happy Eyeballs can
                            // fall back to another address (e.g. IPv4 when this
                            // exit can't reach the IPv6 destination).
                            if format!("{err:#}").contains("os error 111") {
                                captured.reset();
                            }
                            return Err(err);
                        }
                    };
                    tracing::trace!(peer_addr = display(peer_addr), "dialed through VPN");
                    let (read_tunneled, write_tunneled) = tokio::io::split(tunneled);
                    let (read_captured, write_captured) = tokio::io::split(captured);
                    (
                        litecopy(read_tunneled, write_captured),
                        litecopy(read_captured, write_tunneled),
                    )
                        .race()
                        .await?;
                    anyhow::Ok(())
                });

                if let Some(task_limit) = ctx.init().task_limit {
                    add_task(task_limit, task);
                } else {
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
                    if captured.peer_addr().is_ipv6() {
                        "[2606:4700:4700::1111]:53".parse()?
                    } else {
                        "1.1.1.1:53".parse()?
                    }
                } else {
                    peer_addr
                };
                let ctx_clone = ctx.clone();
                let task = geph5_rt::spawn(async move {
                    if peer_addr.port() == 53 && ctx_clone.init().spoof_dns {
                        // fakedns handling
                        loop {
                            let pkt = captured.recv().await?;
                            captured.send(&fake_dns_respond(&ctx_clone, &pkt)?).await?;
                        }
                    } else {
                        let tunneled = open_conn(&ctx_clone, "udp", &peer_addr.to_string()).await?;
                        let (mut read_tunneled, mut write_tunneled) = tokio::io::split(tunneled);
                        let up_loop = async {
                            loop {
                                let to_up = captured.recv().await?;
                                write_tunneled
                                    .write_all(&(to_up.len() as u16).to_le_bytes())
                                    .await?;
                                write_tunneled.write_all(&to_up).await?;
                                write_tunneled.flush().await?;
                            }
                            #[allow(unreachable_code)]
                            anyhow::Ok(())
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
                            #[allow(unreachable_code)]
                            anyhow::Ok(())
                        };
                        (up_loop, dn_loop).race().await
                    }
                });
                if let Some(task_limit) = ctx.init().task_limit {
                    add_task(task_limit, task);
                } else {
                    task.detach();
                }
            }
            ipstack_geph::stream::IpStackStream::UnknownTransport(_) => {
                // tracing::warn!("captured an UnknownTransport")
            }
            ipstack_geph::stream::IpStackStream::UnknownNetwork(_) => {
                tracing::warn!("captured an UnknownNetwork")
            }
        }
    }
}
