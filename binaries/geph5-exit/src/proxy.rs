use std::{
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use geph5_misc_rpc::tunnel_command::{RichTunnelResponse, TunnelCommand};

use futures_util::{AsyncReadExt, AsyncWriteExt, io::BufReader};

use smol::{future::FutureExt as _, net::UdpSocket};

use crate::{
    allow::proxy_allowed,
    dns::{FilterOptions, dns_resolve, raw_dns_respond},
    ipv6::EyeballDialer,
    ratelimit::RateLimiter,
};

use smol_timeout2::TimeoutExt;

#[tracing::instrument(skip_all)]
pub async fn proxy_stream(
    dialer: EyeballDialer,
    sess_metadata: Arc<serde_json::Value>,
    ratelimit: RateLimiter,
    mut stream: picomux::Stream,
    is_free: bool,
) -> anyhow::Result<()> {
    let cmd_str = String::from_utf8_lossy(stream.metadata());
    let cmd = TunnelCommand::from_str(&cmd_str)?;
    let protocol = cmd.protocol().to_string();
    let dest_host = cmd.host().to_string();
    let filter: FilterOptions =
        serde_json::from_value(sess_metadata["filter"].clone()).unwrap_or_default();
    let dest_addrs = dns_resolve(&dest_host, filter)
        .await
        .context("failed to resolve DNS")?;
    if !dest_addrs.iter().all(|addr| proxy_allowed(*addr, is_free)) {
        anyhow::bail!("Proxying to {} is not allowed", dest_host);
    }

    match protocol.as_str() {
        "tcp" => {
            let start = Instant::now();
            let dest_tcp = dialer
                .connect(dest_addrs.clone())
                .timeout(Duration::from_secs(5))
                .await
                .context(format!("timeout in TCP dial to {:?}", dest_addrs))??;
            let latency = start.elapsed();
            let resolved_addr = dest_tcp.peer_addr()?;
            tracing::trace!(
                protocol,
                dest_host = display(dest_host),
                latency = debug(latency),
                "TCP established resolved"
            );
            let resp = RichTunnelResponse {
                resolved_addr,
                open_ms: Some(latency.as_millis() as u32),
            };
            if matches!(cmd, TunnelCommand::Rich(_)) {
                geph5_misc_rpc::write_prepend_length(&serde_json::to_vec(&resp)?, &mut stream)
                    .await?;
            }
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
                let resp = RichTunnelResponse {
                    resolved_addr: addr,
                    open_ms: None,
                };
                if matches!(cmd, TunnelCommand::Rich(_)) {
                    geph5_misc_rpc::write_prepend_length(&serde_json::to_vec(&resp)?, &mut stream)
                        .await?;
                }
                return proxy_dns(stream, filter).await;
            }
            if addr.port() == 443 {
                anyhow::bail!("special-case banning QUIC to improve traffic management")
            }
            let udp_socket: UdpSocket = UdpSocket::bind("0.0.0.0:0")
                .await
                .context("UDP bind failed")?;
            udp_socket.connect(addr).await?;

            let resp = RichTunnelResponse {
                resolved_addr: addr,
                open_ms: None,
            };
            if matches!(cmd, TunnelCommand::Rich(_)) {
                geph5_misc_rpc::write_prepend_length(&serde_json::to_vec(&resp)?, &mut stream)
                    .await?;
            }

            let (read_stream, mut write_stream) = stream.split();
            let up_loop = async {
                let mut read_stream = BufReader::new(read_stream);
                let mut len_buf = [0; 2];
                loop {
                    read_stream
                        .read_exact(&mut len_buf)
                        .timeout(Duration::from_secs(60))
                        .await
                        .context("timeout in udp up")??;
                    let mut packet_buf = vec![0; u16::from_le_bytes(len_buf) as usize];
                    read_stream
                        .read_exact(&mut packet_buf)
                        .timeout(Duration::from_secs(60))
                        .await
                        .context("timeout in udp up")??;
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
                        .context("timeout in udp down")??;
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
