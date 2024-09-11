//! This module provides functionality for setting up a system-level VPN.
#[cfg(target_os = "linux")]
mod linux;
use bytes::Bytes;
use crossbeam_queue::SegQueue;
use dashmap::DashMap;
use event_listener::Event;
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
use futures_util::{AsyncReadExt, AsyncWriteExt};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

use rand::Rng;
use simple_dns::{Packet, QTYPE};
use smol::{
    future::FutureExt,
    io::{BufReader, BufWriter},
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

use crate::{client::CtxField, client_inner::open_conn, Config};

static FAKE_DNS_FORWARD: CtxField<DashMap<String, Ipv4Addr>> = |_| DashMap::new();

static FAKE_DNS_BACKWARD: CtxField<DashMap<Ipv4Addr, String>> = |_| DashMap::new();

pub fn fake_dns_backtranslate(ctx: &AnyCtx<Config>, fake: Ipv4Addr) -> Option<String> {
    tracing::trace!(fake = debug(fake), "attempting to backtranslate");
    ctx.get(FAKE_DNS_BACKWARD)
        .get(&fake)
        .map(|entry| entry.clone())
}

pub fn fake_dns_allocate(ctx: &AnyCtx<Config>, dns_name: &str) -> Ipv4Addr {
    *ctx.get(FAKE_DNS_FORWARD)
        .entry(dns_name.to_string())
        .or_insert_with(|| {
            let base = u32::from_be_bytes([240, 0, 0, 0]);
            let mask = u32::from_be_bytes([240, 0, 0, 0]);
            let offset = rand::thread_rng().gen_range(0..=!mask);
            let ip_addr = base | offset;
            let ip_addr = Ipv4Addr::from(ip_addr);
            ctx.get(FAKE_DNS_BACKWARD)
                .insert(ip_addr, dns_name.to_string());
            tracing::debug!(
                from = debug(dns_name),
                to = debug(ip_addr),
                "created fake dns mapping",
            );
            ip_addr
        })
}

/// Force a particular packet to be sent through VPN mode, regardless of whether VPN mode is on.
pub async fn send_vpn_packet(ctx: &AnyCtx<Config>, bts: Bytes) {
    tracing::trace!(
        len = bts.len(),
        chan_len = ctx.get(VPN_CAPTURE).len(),
        "vpn forcing up"
    );
    ctx.get(VPN_CAPTURE).push((bts, Instant::now()));
    ctx.get(VPN_EVENT).notify(usize::MAX);
}

/// Receive a packet from VPN mode, regardless of whether VPN mode is on.
pub async fn recv_vpn_packet(ctx: &AnyCtx<Config>) -> Bytes {
    loop {
        let evt = ctx.get(VPN_EVENT).listen();
        if let Some(bts) = ctx.get(VPN_INJECT).pop() {
            return bts;
        }
        evt.await;
    }
}

static VPN_EVENT: CtxField<Event> = |_| Event::new();

static VPN_CAPTURE: CtxField<SegQueue<(Bytes, Instant)>> = |_| SegQueue::new();

static VPN_INJECT: CtxField<SegQueue<Bytes>> = |_| SegQueue::new();

pub async fn vpn_loop(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    let (send_captured, recv_captured) = smol::channel::unbounded();
    let (send_injected, recv_injected) = smol::channel::unbounded();

    let ipstack = IpStack::new(
        #[cfg(target_os = "ios")]
        IpStackConfig {
            mtu: 1450,
            tcp_timeout: Duration::from_secs(3600),
            udp_timeout: Duration::from_secs(600),
        },
        #[cfg(not(target_os = "ios"))]
        IpStackConfig::default(),
        recv_captured,
        send_injected,
    );
    let _shuffle = if ctx.init().vpn {
        smolscale::spawn(packet_shuffle(ctx.clone(), send_captured, recv_injected))
    } else {
        let ctx = ctx.clone();
        smolscale::spawn(async move {
            let up_loop = async {
                loop {
                    let evt = ctx.get(VPN_EVENT).listen();
                    if let Some((bts, time)) = ctx.get(VPN_CAPTURE).pop() {
                        tracing::trace!(
                            len = bts.len(),
                            elapsed = debug(time.elapsed()),
                            packet = display(hex::encode(&bts)),
                            "vpn shuffling up"
                        );
                        send_captured.send(bts).await?;
                    }
                    evt.await;
                }
            };
            let dn_loop = async {
                loop {
                    let bts = recv_injected.recv().await?;
                    tracing::trace!(len = bts.len(), "vpn shuffling down");
                    ctx.get(VPN_INJECT).push(bts);
                    ctx.get(VPN_EVENT).notify(usize::MAX);
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

                smolscale::spawn(async move {
                    let tunneled = open_conn(&ctx, "tcp", &peer_addr.to_string()).await?;
                    tracing::trace!(peer_addr = display(peer_addr), "dialed through VPN");
                    let (read_tunneled, write_tunneled) = tunneled.split();
                    let (read_captured, write_captured) = captured.split();
                    smol::io::copy(read_tunneled, write_captured)
                        .race(smol::io::copy(read_captured, write_tunneled))
                        .await?;
                    anyhow::Ok(())
                })
                .detach();
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
                smolscale::spawn::<anyhow::Result<()>>(async move {
                    if peer_addr.port() == 53 && ctx.init().spoof_dns {
                        // fakedns handling
                        loop {
                            let pkt = captured.recv().await?;
                            let pkt = Packet::parse(&pkt)?;
                            tracing::trace!(pkt = debug(&pkt), "got DNS packet");
                            let mut answers = vec![];
                            for question in pkt.questions.iter() {
                                if question.qtype == QTYPE::TYPE(simple_dns::TYPE::A) {
                                    answers.push(simple_dns::ResourceRecord::new(
                                        question.qname.clone(),
                                        simple_dns::CLASS::IN,
                                        1,
                                        simple_dns::rdata::RData::A(
                                            fake_dns_allocate(&ctx, &question.qname.to_string())
                                                .into(),
                                        ),
                                    ));
                                }
                            }
                            let mut response = pkt.into_reply();
                            response.answers = answers;

                            captured
                                .send(&response.build_bytes_vec_compressed()?)
                                .await?;
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
                })
                .detach();
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
