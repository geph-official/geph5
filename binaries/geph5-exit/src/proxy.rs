use std::{net::SocketAddr, time::Instant};

use anyhow::Context;
use futures_util::{io::BufReader, AsyncReadExt, AsyncWriteExt};
use rand::seq::SliceRandom;
use sillad::{dialer::Dialer, tcp::TcpDialer};
use smol::{future::FutureExt as _, net::UdpSocket};

use crate::ratelimit::RateLimiter;

#[tracing::instrument(skip_all)]
pub async fn proxy_stream(ratelimit: RateLimiter, stream: picomux::Stream) -> anyhow::Result<()> {
    let dest_host = String::from_utf8_lossy(stream.metadata());
    let (dest_host, protocol): (&str, &str) = if dest_host.contains('-') {
        dest_host.split_once('-').unwrap()
    } else {
        (&dest_host, "tcp")
    };
    let dest_addr = dns_resolve(dest_host).await?;
    tracing::debug!(
        protocol,
        dest_host = display(&dest_host),
        dest_addr = display(dest_addr),
        "DNS resolved"
    );
    match protocol {
        "tcp" => {
            let start = Instant::now();
            let dest_tcp = TcpDialer { dest_addr }.dial().await?;
            tracing::debug!(
                protocol,
                dest_host = display(dest_host),
                dest_addr = display(dest_addr),
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
            let udp_socket = UdpSocket::bind("0.0.0.0:0").await?;
            udp_socket.connect(dest_host).await?;
            let (read_stream, mut write_stream) = stream.split();
            let up_loop = async {
                let mut read_stream = BufReader::new(read_stream);
                let mut len_buf = [0; 2];
                loop {
                    read_stream.read_exact(&mut len_buf).await?;
                    let mut packet_buf = vec![0; u16::from_le_bytes(len_buf) as usize];
                    read_stream.read_exact(&mut packet_buf).await?;
                    udp_socket.send(&packet_buf).await?;
                }
            };
            let dn_loop = async {
                let mut buf = [0u8; 2048];
                loop {
                    // Receive data into the buffer starting from the third byte
                    let len = udp_socket.recv(&mut buf[2..]).await?;

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
