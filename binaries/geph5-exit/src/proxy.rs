use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use geph5_misc_rpc::tunnel_command::{RichTunnelResponse, TunnelCommand};

use futures_concurrency::future::Race;
use geph5_rt::TimeoutExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UdpSocket};

use crate::{
    allow::proxy_allowed,
    dns::{FilterOptions, dns_resolve, raw_dns_respond},
    ipv6::EyeballDialer,
    ratelimit::RateLimiter,
};

const TCP_IDLE_TIMEOUT: Duration = Duration::from_secs(1800);

static ACTIVE_PROXY_TASKS: AtomicUsize = AtomicUsize::new(0);

/// Obtains the current number of active proxy tasks.
pub fn get_task_count() -> usize {
    ACTIVE_PROXY_TASKS.load(Ordering::Relaxed)
}

struct ActiveProxyTask;

impl ActiveProxyTask {
    fn new() -> Self {
        ACTIVE_PROXY_TASKS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for ActiveProxyTask {
    fn drop(&mut self) {
        ACTIVE_PROXY_TASKS.fetch_sub(1, Ordering::Relaxed);
    }
}

struct IdleActivity {
    start: tokio::time::Instant,
    last_activity_ms: AtomicU64,
}

impl IdleActivity {
    fn new() -> Self {
        Self {
            start: tokio::time::Instant::now(),
            last_activity_ms: AtomicU64::new(0),
        }
    }

    fn touch(&self) {
        let elapsed_ms = self.start.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.last_activity_ms
            .fetch_max(elapsed_ms, Ordering::Relaxed);
    }

    fn deadline(&self, timeout: Duration) -> tokio::time::Instant {
        let last_ms = self.last_activity_ms.load(Ordering::Relaxed);
        self.start + Duration::from_millis(last_ms) + timeout
    }
}

#[tracing::instrument(skip_all)]
pub async fn proxy_stream(
    dialer: EyeballDialer,
    sess_metadata: Arc<serde_json::Value>,
    ratelimit: RateLimiter,
    stream: picomux::Stream,
    is_free: bool,
) -> anyhow::Result<()> {
    let _active_task = ActiveProxyTask::new();
    let mut stream = stream;
    let cmd_str = String::from_utf8_lossy(stream.metadata());
    let cmd = TunnelCommand::from_str(&cmd_str)?;
    let is_rich = matches!(cmd, TunnelCommand::Rich(_));
    // Box the large DNS/connect setup state so it is freed after setup, keeping
    // the long-lived proxy future small across millions of concurrent streams.
    let opened = match Box::pin(open_tunnel(
        &dialer,
        &sess_metadata,
        &cmd,
        is_rich,
        is_free,
        &mut stream,
    ))
    .await
    {
        Ok(opened) => opened,
        Err(err) => return fail_rich_open(stream, is_rich, err).await,
    };
    match opened {
        OpenedTunnel::Tcp { dest_tcp } => proxy_tcp(stream, dest_tcp, ratelimit).await,
        OpenedTunnel::Dns { filter } => proxy_dns(stream, filter).await,
        OpenedTunnel::Udp { udp_socket } => {
            // Keep UDP forwarding state separate from the common proxy future.
            Box::pin(proxy_udp(stream, udp_socket, ratelimit)).await
        }
    }
}

enum OpenedTunnel {
    Tcp { dest_tcp: TcpStream },
    Dns { filter: FilterOptions },
    Udp { udp_socket: UdpSocket },
}

async fn open_tunnel(
    dialer: &EyeballDialer,
    sess_metadata: &serde_json::Value,
    cmd: &TunnelCommand,
    is_rich: bool,
    is_free: bool,
    stream: &mut picomux::Stream,
) -> anyhow::Result<OpenedTunnel> {
    let protocol = cmd.protocol().to_string();
    let dest_host = cmd.host().to_string();
    let filter: FilterOptions =
        serde_json::from_value(sess_metadata["filter"].clone()).unwrap_or_default();
    let dest_addrs = dns_resolve(&dest_host, filter)
        .await
        .context("failed to resolve DNS")?;
    if !dest_addrs
        .iter()
        .all(|addr| proxy_allowed(*addr, is_free, &protocol))
    {
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
            let resp = RichTunnelResponse::Success {
                resolved_addr,
                open_ms: Some(latency.as_millis() as u32),
            };
            if is_rich {
                geph5_misc_rpc::write_prepend_length(&serde_json::to_vec(&resp)?, stream).await?;
            }
            Ok(OpenedTunnel::Tcp { dest_tcp })
        }
        "udp" => {
            let addr = *dest_addrs
                .iter()
                .find(|s| s.is_ipv4())
                .context("UDP only supports ipv4 for now")?;
            if addr.port() == 53 {
                let resp = RichTunnelResponse::Success {
                    resolved_addr: addr,
                    open_ms: None,
                };
                if is_rich {
                    geph5_misc_rpc::write_prepend_length(&serde_json::to_vec(&resp)?, stream)
                        .await?;
                }
                return Ok(OpenedTunnel::Dns { filter });
            }
            if addr.port() == 443 {
                anyhow::bail!("special-case banning QUIC to improve traffic management")
            }
            let udp_socket: UdpSocket = UdpSocket::bind("0.0.0.0:0")
                .await
                .context("UDP bind failed")?;
            udp_socket.connect(addr).await?;

            let resp = RichTunnelResponse::Success {
                resolved_addr: addr,
                open_ms: None,
            };
            if is_rich {
                geph5_misc_rpc::write_prepend_length(&serde_json::to_vec(&resp)?, stream).await?;
            }
            Ok(OpenedTunnel::Udp { udp_socket })
        }
        prot => anyhow::bail!("unknown protocol {prot}"),
    }
}

async fn proxy_tcp(
    stream: picomux::Stream,
    dest_tcp: TcpStream,
    ratelimit: RateLimiter,
) -> anyhow::Result<()> {
    proxy_tcp_with_timeout(stream, dest_tcp, ratelimit, TCP_IDLE_TIMEOUT).await
}

async fn proxy_tcp_with_timeout(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    dest: impl AsyncRead + AsyncWrite + Unpin,
    ratelimit: RateLimiter,
    idle_timeout: Duration,
) -> anyhow::Result<()> {
    let (read_stream, mut write_stream) = tokio::io::split(stream);
    let (read_dest, mut write_dest) = tokio::io::split(dest);
    let activity = IdleActivity::new();
    let up = ratelimit.io_copy(read_stream, &mut write_dest, || activity.touch());
    let dn = ratelimit.io_copy(read_dest, &mut write_stream, || activity.touch());
    let idle = async {
        loop {
            let deadline = activity.deadline(idle_timeout);
            tokio::time::sleep_until(deadline).await;
            if tokio::time::Instant::now() >= activity.deadline(idle_timeout) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timeout in TCP idle",
                ));
            }
        }
    };
    (up, dn, idle).race().await?;
    Ok(())
}

async fn proxy_udp(
    stream: picomux::Stream,
    udp_socket: UdpSocket,
    ratelimit: RateLimiter,
) -> anyhow::Result<()> {
    const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
    let (read_stream, mut write_stream) = tokio::io::split(stream);
    let last_activity = std::sync::Mutex::new(tokio::time::Instant::now());
    let up_loop = async {
        let mut read_stream = BufReader::new(read_stream);
        let mut len_buf = [0; 2];
        let mut packet_buf = Vec::new();
        loop {
            read_stream
                .read_exact(&mut len_buf)
                .await
                .context("read UDP length")?;
            packet_buf.resize(u16::from_le_bytes(len_buf) as usize, 0);
            read_stream
                .read_exact(&mut packet_buf)
                .await
                .context("read UDP payload")?;
            *last_activity.lock().unwrap() = tokio::time::Instant::now();
            ratelimit.wait(packet_buf.len()).await;
            udp_socket.send(&packet_buf).await?;
        }
    };
    let dn_loop = async {
        let mut buf = vec![0u8; usize::from(u16::MAX) + 2];
        loop {
            let len = udp_socket.recv(&mut buf[2..]).await?;
            *last_activity.lock().unwrap() = tokio::time::Instant::now();
            ratelimit.wait(len).await;
            buf[..2].copy_from_slice(&(len as u16).to_le_bytes());
            write_stream.write_all(&buf[..len + 2]).await?;
        }
    };
    let idle = async {
        loop {
            let deadline = *last_activity.lock().unwrap() + UDP_IDLE_TIMEOUT;
            tokio::time::sleep_until(deadline).await;
            if tokio::time::Instant::now() >= *last_activity.lock().unwrap() + UDP_IDLE_TIMEOUT {
                anyhow::bail!("UDP tunnel idle");
            }
        }
    };
    (up_loop, dn_loop, idle).race().await
}

async fn fail_rich_open(
    mut stream: picomux::Stream,
    is_rich: bool,
    err: anyhow::Error,
) -> anyhow::Result<()> {
    if is_rich {
        let resp = RichTunnelResponse::Fail(format!("{err:#}"));
        if let Err(write_err) =
            geph5_misc_rpc::write_prepend_length(&serde_json::to_vec(&resp)?, &mut stream).await
        {
            tracing::warn!(
                err = debug(&write_err),
                original_err = debug(&err),
                "failed to send rich tunnel failure response"
            );
        }
    }
    Err(err)
}

async fn proxy_dns(stream: picomux::Stream, filter: FilterOptions) -> anyhow::Result<()> {
    let (mut read_stream, write_stream) = tokio::io::split(stream);
    let write_stream = Arc::new(tokio::sync::Mutex::new(write_stream));
    let mut len_buf = [0; 2];
    loop {
        read_stream.read_exact(&mut len_buf).await?;
        let mut packet_buf = vec![0; u16::from_le_bytes(len_buf) as usize];
        read_stream.read_exact(&mut packet_buf).await?;
        let write_stream = write_stream.clone();
        geph5_rt::spawn(async move {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bw_accounting::BwAccount;

    #[test]
    fn proxy_future_stays_small() {
        geph5_rt::block_on(async {
            let (client, server) = tokio::io::duplex(65536);
            let (client_read, client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let client = picomux::PicoMux::new(client_read, client_write);
            let mut server = picomux::PicoMux::new(server_read, server_write);
            let (stream, _peer) =
                futures_util::future::try_join(client.open(b"example.com:443"), server.accept())
                    .await
                    .unwrap();

            let proxy = proxy_stream(
                EyeballDialer::new(None),
                Arc::new(serde_json::Value::Null),
                RateLimiter::unlimited(BwAccount::unlimited(), None),
                stream,
                false,
            );
            // A large inline setup or protocol future previously made every
            // proxy task allocate 14 KiB, including idle TCP connections.
            let size = std::mem::size_of_val(&proxy);
            assert!(size <= 4096, "proxy future grew to {size} bytes");
        });
    }

    #[test]
    fn tcp_idle_timeout_is_bidirectional() {
        geph5_rt::block_on(async {
            let (mut client_end, proxy_client) = tokio::io::duplex(4096);
            let (proxy_dest, mut dest_end) = tokio::io::duplex(4096);
            let idle_timeout = Duration::from_millis(120);

            let proxy = geph5_rt::spawn(proxy_tcp_with_timeout(
                proxy_client,
                proxy_dest,
                RateLimiter::unlimited(BwAccount::unlimited(), None),
                idle_timeout,
            ));

            // Keep only one direction active well past `idle_timeout`.
            let mut buf = [0u8; 4];
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                client_end.write_all(b"ping").await.unwrap();
                dest_end.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"ping");
            }

            // Now stop traffic in both directions and verify the tunnel times out.
            let err = tokio::time::timeout(Duration::from_secs(2), proxy)
                .await
                .expect("proxy did not time out after becoming idle")
                .expect_err("idle TCP tunnel should fail with TimedOut");
            let io_err = err
                .downcast_ref::<std::io::Error>()
                .expect("expected io::Error");
            assert_eq!(io_err.kind(), std::io::ErrorKind::TimedOut);
        });
    }
}
