use std::{net::SocketAddr, time::Instant};

use anyhow::Context;
use futures_util::AsyncReadExt;
use rand::seq::SliceRandom;
use sillad::{dialer::Dialer, tcp::TcpDialer};

use crate::ratelimit::RateLimiter;

#[tracing::instrument(skip_all)]
pub async fn proxy_stream(ratelimit: RateLimiter, stream: picomux::Stream) -> anyhow::Result<()> {
    let dest_host = String::from_utf8_lossy(stream.metadata());
    let dest_addr = dns_resolve(&dest_host).await?;
    tracing::debug!(
        dest_host = display(&dest_host),
        dest_addr = display(dest_addr),
        "DNS resolved"
    );
    let start = Instant::now();
    let dest_tcp = TcpDialer { dest_addr }.dial().await?;
    tracing::debug!(
        dest_host = display(dest_host),
        dest_addr = display(dest_addr),
        latency = debug(start.elapsed()),
        "TCP established resolved"
    );
    let (read_stream, mut write_stream) = stream.split();
    let (read_dest, mut write_dest) = dest_tcp.split();
    smol::future::race(
        futures_util::io::copy(read_stream, &mut write_dest),
        futures_util::io::copy(read_dest, &mut write_stream),
    )
    .await?;
    Ok(())
}

async fn dns_resolve(name: &str) -> anyhow::Result<SocketAddr> {
    let choices = smol::net::resolve(name)
        .await?
        .into_iter()
        .filter(|a| a.is_ipv4())
        .collect::<Vec<_>>();
    Ok(*choices
        .choose(&mut rand::thread_rng())
        .context("no IP addresses corresponding to DNS name")?)
}
