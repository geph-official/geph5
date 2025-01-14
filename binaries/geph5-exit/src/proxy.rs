use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;

use futures_util::{io::BufReader, AsyncReadExt, AsyncWriteExt};

use smol::{future::FutureExt as _, io::BufWriter, net::UdpSocket};

use crate::{
    allow::proxy_allowed,
    dns::{dns_resolve, raw_dns_respond, FilterOptions},
    ipv6::EyeballDialer,
    ratelimit::RateLimiter,
};

use smol_timeout2::TimeoutExt;

#[tracing::instrument(skip_all)]
pub async fn proxy_stream(
    dialer: EyeballDialer,
    sess_metadata: Arc<serde_json::Value>,
    ratelimit: RateLimiter,
    stream: picomux::Stream,
    is_free: bool,
) -> anyhow::Result<()> {
    let dest_host = String::from_utf8_lossy(stream.metadata());
    let (protocol, dest_host): (&str, &str) = if dest_host.contains('$') {
        dest_host.split_once('$').unwrap()
    } else {
        ("tcp", &dest_host)
    };
    let filter: FilterOptions =
        serde_json::from_value(sess_metadata["filter"].clone()).unwrap_or_default();
    let dest_addrs = dns_resolve(dest_host, filter)
        .await
        .context("failed to resolve DNS")?;
    if !dest_addrs.iter().all(|addr| proxy_allowed(*addr, is_free)) {
        anyhow::bail!("Proxying to {} is not allowed", dest_host);
    }

    match protocol {
        "tcp" => {
            let start = Instant::now();
            let dest_tcp = dialer
                .connect(dest_addrs)
                .await
                .inspect_err(|err| tracing::warn!(err = debug(err), dest_host, "fail to dial"))?;
            tracing::trace!(
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
            let addr = *dest_addrs
                .iter()
                .find(|s| s.is_ipv4())
                .context("UDP only supports ipv4 for now")?;
            if addr.port() == 53 {
                return proxy_dns(stream, filter).await;
            }
            if addr.port() == 443 {
                anyhow::bail!("special-case banning QUIC to improve traffic management")
            }
            let udp_socket: UdpSocket = UdpSocket::bind("0.0.0.0:0")
                .await
                .context("UDP bind failed")?;
            udp_socket.connect(addr).await?;
            let (read_stream, mut write_stream) = stream.split();
            let up_loop = async {
                let mut read_stream = BufReader::new(read_stream);
                let mut len_buf = [0; 2];
                loop {
                    read_stream
                        .read_exact(&mut len_buf)
                        .timeout(Duration::from_secs(60))
                        .await
                        .context("timeout")??;
                    let mut packet_buf = vec![0; u16::from_le_bytes(len_buf) as usize];
                    read_stream
                        .read_exact(&mut packet_buf)
                        .timeout(Duration::from_secs(60))
                        .await
                        .context("timeout")??;
                    ratelimit.wait(packet_buf.len()).await;
                    udp_socket.send(&packet_buf).await?;
                }
            };
            let dn_loop = async {
                let mut buf = [0u8; 8192];
                loop {
                    // Receive data into the buffer starting from the third byte
                    let len = udp_socket
                        .recv(&mut buf[2..])
                        .timeout(Duration::from_secs(60))
                        .await
                        .context("timeout")??;
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

async fn proxy_dns(stream: picomux::Stream, filter: FilterOptions) -> anyhow::Result<()> {
    let (mut read_stream, write_stream) = stream.split();
    let write_stream = Arc::new(smol::lock::Mutex::new(write_stream));
    let mut len_buf = [0; 2];
    loop {
        read_stream.read_exact(&mut len_buf).await?;
        let mut packet_buf = vec![0; u16::from_le_bytes(len_buf) as usize];
        read_stream.read_exact(&mut packet_buf).await?;
        let write_stream = write_stream.clone();
        smolscale::spawn(async move {
            let response = raw_dns_respond(packet_buf.into(), filter).await?;
            let mut stream = write_stream.lock().await;
            stream
                .write_all(&(response.len() as u16).to_le_bytes())
                .await?;
            stream.write_all(&response).await?;
            stream.flush().await?;
            anyhow::Ok(())
        })
        .detach();
    }
}
