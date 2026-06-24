//! VPN plumbing for the stdio interface and the iOS/Android FFI.
//!
//! On desktop platforms, this module runs the IP stack and exposes channels
//! for pushing packets in and pulling packets out. The stdio binary reads
//! from / writes to stdin and stdout, while the iOS and Android apps drive
//! the same channels through `send_pkt` / `recv_pkt` and (on Android) a
//! `vpn_fd` file descriptor. There is no longer any platform-specific
//! system-level packet capture here.

use async_channel::{Receiver, Sender};
use bytes::Bytes;

use ipstack_geph::{IpStack, IpStackConfig};

use std::time::Instant;

use anyctx::AnyCtx;
use anyhow::Context;
use futures_concurrency::future::Race as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    client::CtxField, litecopy::litecopy, session::open_conn, spoof_dns::fake_dns_respond,
    taskpool::add_task,
};

/// Force a particular packet to be sent through VPN mode, regardless of whether VPN mode is on.
pub async fn send_vpn_packet(ctx: &AnyCtx<crate::Config>, bts: Bytes) {
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
pub async fn recv_vpn_packet(ctx: &AnyCtx<crate::Config>) -> Bytes {
    ctx.get(VPN_INJECT_CHAN).1.recv().await.unwrap()
}

static VPN_CAPTURE_CHAN: CtxField<(Sender<(Bytes, Instant)>, Receiver<(Bytes, Instant)>)> =
    |_| async_channel::bounded(100);

static VPN_INJECT_CHAN: CtxField<(Sender<Bytes>, Receiver<Bytes>)> =
    |_| async_channel::bounded(100);

pub async fn vpn_loop(ctx: &AnyCtx<crate::Config>) -> anyhow::Result<()> {
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
    let _shuffle = {
        let ctx = ctx.clone();
        geph5_rt::spawn(async move {
            let up_loop = async {
                loop {
                    let (bts, time) = match ctx.get(VPN_CAPTURE_CHAN).1.recv().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };

                    tracing::trace!(
                        len = bts.len(),
                        elapsed = debug(time.elapsed()),
                        packet = display(hex::encode(&bts)),
                        "vpn shuffling up"
                    );
                    if send_captured.send(bts).await.is_err() {
                        break;
                    }
                }
            };
            let dn_loop = async {
                loop {
                    let bts = match recv_injected.recv().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    tracing::trace!(len = bts.len(), "vpn shuffling down");

                    if ctx.get(VPN_INJECT_CHAN).0.send(bts).await.is_err() {
                        break;
                    }
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
                    let tunneled = open_conn(&ctx_clone, "tcp", &peer_addr.to_string()).await?;
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
