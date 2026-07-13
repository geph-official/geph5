use crate::{litecopy::litecopy, session::open_conn, taskpool::add_task};

use anyctx::AnyCtx;
use anyhow::Context;

use async_channel as channel;
use futures_concurrency::future::Race as _;
use futures_concurrency::future::TryJoin as _;
use sillad::listener::Listener as _;
use socksv5::v5::{
    SocksV5AuthMethod, SocksV5Command, SocksV5Host, SocksV5RequestStatus, read_handshake,
    read_request, write_auth_method, write_request_status,
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::UdpSocket;

use super::Config;

#[tracing::instrument(skip_all)]
pub async fn socks5_loop(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    if let Some(listen_addr) = ctx.init().socks5_listen {
        let mut listener = sillad::tcp::TcpListener::bind(listen_addr).await?;
        // Owns the per-connection handlers so they are all cancelled when this
        // loop's future is dropped (replaces the old `nursery!` scoped executor).
        let reaper: geph5_rt::TaskReaper<anyhow::Result<()>> =
            geph5_rt::TaskReaper::new();
        loop {
            let client = listener.accept().await?;
            let ctx_clone = ctx.clone();
            let task = geph5_rt::spawn(async move {
                let ctx = ctx_clone;
                tracing::trace!("socks5 connection accepted");
                let (mut read_client, mut write_client) = tokio::io::split(client);
                let _handshake = read_handshake(&mut read_client).await?;
                write_auth_method(&mut write_client, SocksV5AuthMethod::Noauth).await?;
                let request = read_request(&mut read_client).await?;
                match request.command {
                    SocksV5Command::Connect => {
                        let port = request.port;
                        let domain = host_to_string(&request.host)?;
                        let remote_addr = format!("{domain}:{port}");
                        tracing::trace!(
                            remote_addr = display(&remote_addr),
                            "socks5 connect request received"
                        );
                        let stream = open_conn(&ctx, "tcp", &remote_addr).await?;
                        write_request_status(
                            &mut write_client,
                            SocksV5RequestStatus::Success,
                            clone_host(&request.host),
                            port,
                        )
                        .await?;
                        tracing::trace!(remote_addr = display(&remote_addr), "connection opened");
                        let (read_stream, write_stream) = tokio::io::split(stream);
                        // try_join, not race: a half-close in one direction must
                        // not drop the other direction (which may still be
                        // carrying the response). An error in either still
                        // short-circuits.
                        (
                            litecopy(read_stream, write_client),
                            litecopy(read_client, write_stream),
                        )
                            .try_join()
                            .await?;
                        anyhow::Ok(())
                    }
                    SocksV5Command::UdpAssociate => {
                        handle_udp_associate(&ctx, listen_addr, read_client, write_client).await
                    }
                    _ => {
                        write_request_status(
                            &mut write_client,
                            SocksV5RequestStatus::CommandNotSupported,
                            clone_host(&request.host),
                            request.port,
                        )
                        .await?;
                        anyhow::Ok(())
                    }
                }
            });
            if let Some(task_limit) = ctx.init().task_limit {
                add_task(task_limit, task);
            } else {
                reaper.attach(task);
            }
        }
    } else {
        std::future::pending().await
    }
}

fn host_to_string(host: &SocksV5Host) -> anyhow::Result<String> {
    Ok(match host {
        SocksV5Host::Domain(dom) => String::from_utf8_lossy(dom).parse()?,
        SocksV5Host::Ipv4(v4) => Ipv4Addr::new(v4[0], v4[1], v4[2], v4[3]).to_string(),
        _ => anyhow::bail!("IPv6 not supported"),
    })
}

fn clone_host(host: &SocksV5Host) -> SocksV5Host {
    match host {
        SocksV5Host::Domain(dom) => SocksV5Host::Domain(dom.clone()),
        SocksV5Host::Ipv4(v4) => SocksV5Host::Ipv4(*v4),
        SocksV5Host::Ipv6(v6) => SocksV5Host::Ipv6(*v6),
    }
}

#[tracing::instrument(skip_all)]
async fn handle_udp_associate(
    ctx: &AnyCtx<Config>,
    listen_addr: SocketAddr,
    mut read_client: impl AsyncRead + Unpin,
    mut write_client: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    let bind_ip = match listen_addr.ip() {
        IpAddr::V4(v4) => v4,
        _ => {
            write_request_status(
                &mut write_client,
                SocksV5RequestStatus::AddrtypeNotSupported,
                SocksV5Host::Ipv4([0, 0, 0, 0]),
                0,
            )
            .await?;
            return Ok(());
        }
    };

    let udp_socket = Arc::new(
        UdpSocket::bind(SocketAddr::new(IpAddr::V4(bind_ip), 0))
            .await
            .context("failed to bind UDP associate socket")?,
    );
    let bnd = udp_socket.local_addr()?;
    write_request_status(
        &mut write_client,
        SocksV5RequestStatus::Success,
        SocksV5Host::Ipv4(match bnd.ip() {
            IpAddr::V4(v4) => v4.octets(),
            _ => [0, 0, 0, 0],
        }),
        bnd.port(),
    )
    .await?;

    let tunnels: Arc<
        parking_lot::Mutex<HashMap<String, (SocksV5Host, u16, channel::Sender<Vec<u8>>)>>,
    > = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let ctx = ctx.clone();
    let udp_socket_clone = udp_socket.clone();
    let tunnels_clone = tunnels.clone();
    let udp_loop = async move {
        let mut buf = [0u8; 65535];
        let mut client_addr: Option<SocketAddr> = None;
        loop {
            let (len, src) = udp_socket_clone.recv_from(&mut buf).await?;
            if let Some(allowed) = client_addr {
                if allowed != src {
                    continue;
                }
            } else {
                client_addr = Some(src);
            }
            if let Some((target, host, port, payload)) = parse_udp_datagram(&buf[..len])? {
                let sender = {
                    let mut guard = tunnels_clone.lock();
                    if let Some(existing) = guard.get(&target) {
                        existing.2.clone()
                    } else {
                        let (tx, rx) = channel::bounded(32);
                        guard.insert(target.clone(), (clone_host(&host), port, tx.clone()));
                        geph5_rt::spawn(run_udp_tunnel(
                            ctx.clone(),
                            target.clone(),
                            clone_host(&host),
                            port,
                            udp_socket_clone.clone(),
                            client_addr.unwrap(),
                            rx,
                        ))
                        .detach();
                        tx
                    }
                };

                if sender.send(payload.to_vec()).await.is_err() {
                    tunnels_clone.lock().remove(&target);
                }
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };

    let tcp_watch = async move {
        let mut buf = [0u8; 1];
        let _ = read_client.read(&mut buf).await;
        anyhow::Ok(())
    };

    (udp_loop, tcp_watch).race().await
}

fn parse_udp_datagram(buf: &[u8]) -> anyhow::Result<Option<(String, SocksV5Host, u16, &[u8])>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    if buf[2] != 0 {
        return Ok(None);
    }
    let atyp = buf[3];
    match atyp {
        0x01 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let addr = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
            let port = u16::from_be_bytes([buf[8], buf[9]]);
            let payload = &buf[10..];
            let target = format!("{addr}:{port}");
            Ok(Some((
                target,
                SocksV5Host::Ipv4(addr.octets()),
                port,
                payload,
            )))
        }
        0x03 => {
            if buf.len() < 5 {
                return Ok(None);
            }
            let len = buf[4] as usize;
            if buf.len() < 5 + len + 2 {
                return Ok(None);
            }
            let domain = buf[5..5 + len].to_vec();
            let port = u16::from_be_bytes([buf[5 + len], buf[5 + len + 1]]);
            let payload = &buf[5 + len + 2..];
            let target = format!("{}:{port}", String::from_utf8_lossy(&domain));
            Ok(Some((target, SocksV5Host::Domain(domain), port, payload)))
        }
        _ => Ok(None),
    }
}

fn write_udp_header(buf: &mut Vec<u8>, host: &SocksV5Host, port: u16) {
    buf.extend_from_slice(&[0, 0, 0]); // RSV + FRAG
    match host {
        SocksV5Host::Ipv4(v4) => {
            buf.push(0x01);
            buf.extend_from_slice(v4);
        }
        SocksV5Host::Domain(dom) => {
            buf.push(0x03);
            buf.push(dom.len() as u8);
            buf.extend_from_slice(dom);
        }
        SocksV5Host::Ipv6(v6) => {
            buf.push(0x04);
            buf.extend_from_slice(v6);
        }
    }
    buf.extend_from_slice(&port.to_be_bytes());
}

async fn run_udp_tunnel(
    ctx: AnyCtx<Config>,
    target: String,
    reply_host: SocksV5Host,
    reply_port: u16,
    udp_socket: Arc<UdpSocket>,
    client_addr: SocketAddr,
    recv: channel::Receiver<Vec<u8>>,
) -> anyhow::Result<()> {
    let pipe = open_conn(&ctx, "udp", &target).await?;
    let (mut read_tunneled, mut write_tunneled) = tokio::io::split(pipe);

    let up = async {
        while let Ok(pkt) = recv.recv().await {
            write_tunneled
                .write_all(&(pkt.len() as u16).to_le_bytes())
                .await?;
            write_tunneled.write_all(&pkt).await?;
            write_tunneled.flush().await?;
        }
        anyhow::Ok(())
    };

    let down = async {
        loop {
            let mut len_buf = [0u8; 2];
            read_tunneled.read_exact(&mut len_buf).await?;
            let len = u16::from_le_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            read_tunneled.read_exact(&mut buf).await?;
            let mut out = Vec::with_capacity(6 + buf.len());
            write_udp_header(&mut out, &reply_host, reply_port);
            out.extend_from_slice(&buf);
            udp_socket.send_to(&out, client_addr).await?;
        }
    };

    (up, down).race().await
}
