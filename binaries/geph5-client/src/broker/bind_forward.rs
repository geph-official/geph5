//! Per-process egress binding for the broker's fronted `reqwest` client.
//!
//! In Windows full-tunnel mode, [`crate::bound_dialer`] pins the engine's TCP
//! sockets to the physical NIC via `IP_UNICAST_IF`. `reqwest` creates its sockets
//! internally, so to pin the fronted broker connection too we splice its TCP
//! through a tiny loopback forwarder whose *upstream* is dialed via
//! [`connect_addrs`] (hence physical-NIC-pinned).
//!
//! This keeps the discriminator strictly per-process. The broker front is an
//! *intentionally shared* domain-fronting IP (a CDN edge hosting thousands of
//! unrelated sites), so any other process reaching that same IP still follows the
//! default route into the tunnel.
//!
//! Only fronted sources with fixed `override_dns` addresses reach this (the other
//! broker sources need DNS and are ignored in VPN mode — see `broker.rs`), so the
//! forwarder only ever needs a fixed list of upstream `SocketAddr`s.

use std::{collections::HashMap, net::SocketAddr, sync::Arc, sync::OnceLock};

use anyhow::Context;
use futures_concurrency::future::Race as _;
use sillad::{
    listener::Listener,
    tcp::{TcpListener, TcpPipe},
};
use tokio::sync::Mutex;

use crate::{bound_dialer::connect_addrs, litecopy::litecopy};

static CACHE: OnceLock<Mutex<HashMap<Vec<SocketAddr>, SocketAddr>>> = OnceLock::new();

/// Loopback address of a forwarder that splices to one of `dests` (dialed via the
/// bound dialer). Listens on `127.0.0.1` at an ephemeral port; reqwest honours the
/// returned port because the broker front URLs carry no explicit port. Forwarders
/// are cached and long-lived.
pub async fn forward_addrs(mut dests: Vec<SocketAddr>) -> anyhow::Result<SocketAddr> {
    dests.sort();
    dests.dedup();
    anyhow::ensure!(!dests.is_empty(), "no upstream addresses to forward to");

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().await;
    if let Some(addr) = guard.get(&dests) {
        return Ok(*addr);
    }
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .context("bind loopback forwarder")?;
    let local = listener.local_addr().await;
    spawn_forwarder(listener, dests.clone());
    guard.insert(dests, local);
    Ok(local)
}

fn spawn_forwarder(mut listener: TcpListener, dests: Vec<SocketAddr>) {
    let dests = Arc::new(dests);
    geph5_rt::spawn(async move {
        loop {
            let downstream = match listener.accept().await {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::warn!(err = %err, "broker egress forwarder stopped accepting");
                    return;
                }
            };
            let dests = dests.clone();
            geph5_rt::spawn(async move {
                if let Err(err) = splice(downstream, &dests).await {
                    tracing::debug!(err = %err, "broker egress forwarder connection ended");
                }
            })
            .detach();
        }
    })
    .detach();
}

async fn splice(downstream: TcpPipe, dests: &[SocketAddr]) -> anyhow::Result<()> {
    // The bound dialer applies the IP_UNICAST_IF pin → physical NIC.
    let upstream = connect_addrs(dests)
        .await
        .context("forwarder upstream dial failed")?;
    let (read_down, write_down) = tokio::io::split(downstream);
    let (read_up, write_up) = tokio::io::split(upstream);
    (
        litecopy(read_down, write_up),
        litecopy(read_up, write_down),
    )
        .race()
        .await?;
    Ok(())
}
