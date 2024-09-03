use std::{
    net::SocketAddr,
    sync::LazyLock,
    time::{Duration, Instant},
};

use anyhow::Context;
use futures_util::{io::BufReader, AsyncReadExt, AsyncWriteExt};
use moka::future::Cache;

use sillad::{dialer::Dialer, tcp::HappyEyeballsTcpDialer};
use smol::{future::FutureExt as _, net::UdpSocket};

use crate::{allow::proxy_allowed, ratelimit::RateLimiter};

#[tracing::instrument(skip_all)]
pub async fn proxy_stream(ratelimit: RateLimiter, stream: picomux::Stream) -> anyhow::Result<()> {
    let dest_host = String::from_utf8_lossy(stream.metadata());
    let (protocol, dest_host): (&str, &str) = if dest_host.contains('$') {
        dest_host.split_once('$').unwrap()
    } else {
        ("tcp", &dest_host)
    };
    let dest_addrs = dns_resolve(dest_host).await?;
    if !dest_addrs.iter().all(|addr| proxy_allowed(*addr)) {
        anyhow::bail!("Proxying to {} is not allowed", dest_host);
    }
    match protocol {
        "tcp" => {
            let start = Instant::now();
            let dest_tcp = HappyEyeballsTcpDialer(dest_addrs).dial().await?;
            tracing::debug!(
                protocol,
                dest_host = display(dest_host),
                latency = debug(start.elapsed()),
                "TCP established resolved"
            );
            let (read_stream, mut write_stream) = stream.split();
            let (read_dest, mut write_dest) = dest_tcp.split();
            smol::future::race(
                ratelimit.io_copy(read_stream, &mut write_dest),
                ratelimit.io_copy(read_dest, &mut write_stream),
            )
            .await?;
            Ok(())
        }
        "udp" => {
            let udp_socket: UdpSocket = UdpSocket::bind("0.0.0.0:0").await?;
            let addr = *dest_addrs
                .iter()
                .find(|s| s.is_ipv4())
                .context("UDP only supports ipv4 for now")?;
            if addr.port() == 443 {
                anyhow::bail!("special-case banning QUIC to improve traffic management")
            }
            udp_socket.connect(addr).await?;
            let (read_stream, mut write_stream) = stream.split();
            let up_loop = async {
                let mut read_stream = BufReader::new(read_stream);
                let mut len_buf = [0; 2];
                loop {
                    read_stream.read_exact(&mut len_buf).await?;
                    let mut packet_buf = vec![0; u16::from_le_bytes(len_buf) as usize];
                    read_stream.read_exact(&mut packet_buf).await?;
                    ratelimit.wait(packet_buf.len()).await;
                    udp_socket.send(&packet_buf).await?;
                }
            };
            let dn_loop = async {
                let mut buf = [0u8; 8192];
                loop {
                    // Receive data into the buffer starting from the third byte
                    let len = udp_socket.recv(&mut buf[2..]).await?;
                    ratelimit.wait(len).await;

                    // Store the length of the data in the first two bytes
                    let len_bytes = (len as u16).to_le_bytes();
                    buf[0] = len_bytes[0];
                    buf[1] = len_bytes[1];

                    // Write both the length and the data in a single call
                    write_stream.write_all(&buf[..len + 2]).await?;
                }
            };
            up_loop.race(dn_loop).await
        }
        prot => {
            anyhow::bail!("unknown protocol {prot}")
        }
    }
}

async fn dns_resolve(name: &str) -> anyhow::Result<Vec<SocketAddr>> {
    static CACHE: LazyLock<Cache<String, Vec<SocketAddr>>> = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(240))
            .build()
    });
    let addr = CACHE
        .try_get_with(name.to_string(), async {
            let choices = smol::net::resolve(name).await?;
            anyhow::Ok(choices)
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(addr)
}
