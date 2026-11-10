//! The SOCKS5 listener uses `fast-socks5` for authentication negotiation, address
//! encoding/decoding, and UDP headers. It supports the no-authentication method,
//! TCP CONNECT to IPv4, IPv6, and domain destinations, and UDP ASSOCIATE over
//! IPv4 or IPv6 local connections. Destination domains are resolved by the exit.
//! The listener explicitly rejects BIND and unsupported authentication methods.
//! Malformed requests receive SOCKS failure replies; malformed UDP datagrams and
//! fragmented datagrams are discarded without closing the association.
//!
//! Current exits provide IPv4 UDP forwarding and a DNS service. General IPv6 UDP
//! destinations remain unsupported. DNS requests addressed to an IPv6 resolver
//! can use the existing exit DNS service through its IPv4 endpoint; replies keep
//! the original logical resolver address, matching the exit's DNS interception.
//! An association accepts UDP only from its TCP peer's IP and the requested source
//! port, or the first valid datagram's port when the request specifies zero.
//! Closing its TCP connection cancels all its forwarding tasks. Each association
//! allows up to 256 destination flows with 32 queued datagrams per flow; excess
//! datagrams are dropped, and flows expire after 60 seconds without traffic in
//! either direction.
//!
//! The existing exit protocol returns the resolved destination address but not
//! the exit's outbound socket's bound address. CONNECT success therefore reports
//! an unspecified address and port zero, rather than incorrectly echoing the
//! destination. Reporting the exact CONNECT bound endpoint requires an exit
//! protocol extension.
//!
//! Run the SOCKS regression tests with `cargo test -p geph5-client --lib socks5::`.
//! They use local IPv4/IPv6 sockets and mock tunnel streams, without contacting a
//! broker or a deployed exit.

use crate::{litecopy::litecopy, session::open_conn_with_addr, taskpool::add_task};

use anyctx::AnyCtx;
use anyhow::Context;
use async_channel as channel;
use fast_socks5::{
    ReplyError, consts, new_udp_header, parse_udp_request,
    server::Socks5ServerProtocol,
    util::target_addr::{TargetAddr, read_address},
};
use futures_concurrency::future::{Race as _, TryJoin as _};
use futures_util::future::BoxFuture;
use sillad::{Pipe as _, listener::Listener as _};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::UdpSocket,
    task::JoinSet,
    time::timeout,
};

use super::Config;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_UDP_FLOWS: usize = 256;
const UDP_QUEUE_LEN: usize = 32;

type OpenedTunnel = (Box<dyn sillad::Pipe>, SocketAddr);
type TunnelOpener = Arc<
    dyn Fn(&'static str, String) -> BoxFuture<'static, anyhow::Result<OpenedTunnel>> + Send + Sync,
>;

#[tracing::instrument(skip_all)]
pub async fn socks5_loop(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    let Some(listen_addr) = ctx.init().socks5_listen else {
        return std::future::pending().await;
    };
    // sillad explicitly enables dual-stack IPv6 listeners on every platform.
    let mut listener = sillad::tcp::TcpListener::bind(listen_addr).await?;
    let open: TunnelOpener = {
        let ctx = ctx.clone();
        Arc::new(move |protocol, target| {
            let ctx = ctx.clone();
            Box::pin(async move { open_conn_with_addr(&ctx, protocol, &target).await })
        })
    };
    let reaper = geph5_rt::TaskReaper::new();
    loop {
        let client = listener.accept().await?;
        // Use the accepted socket's address, not the configured wildcard. An
        // IPv4 client of a dual-stack listener needs an IPv4 UDP relay socket.
        let local = canonical_addr(client.local_addr()?);
        let peer = canonical_addr(
            client
                .remote_addr()
                .context("SOCKS client has no peer address")?
                .parse()?,
        );
        let open = open.clone();
        let task = geph5_rt::spawn(async move {
            let result = handle_client(client, local, peer, open).await;
            if let Err(err) = &result {
                tracing::debug!(%peer, err = %format!("{err:#}"), "SOCKS connection ended");
            }
            result
        });
        if let Some(task_limit) = ctx.init().task_limit {
            add_task(task_limit, task);
        } else {
            reaper.attach(task);
        }
    }
}

async fn handle_client<S>(
    mut client: S,
    local: SocketAddr,
    peer: SocketAddr,
    open: TunnelOpener,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = timeout(HANDSHAKE_TIMEOUT, async {
        // The library selects no-auth only if the client actually offered it,
        // and sends 05 FF otherwise. It does not resolve destination domains.
        Socks5ServerProtocol::accept_no_auth(&mut client).await?;
        read_request(&mut client).await
    })
    .await
    .context("SOCKS handshake timed out")??;
    let Some((command, target)) = request else {
        return Ok(());
    };
    match command {
        consts::SOCKS5_CMD_TCP_CONNECT => {
            let result = timeout(CONNECT_TIMEOUT, open("tcp", target.to_string()))
                .await
                .map_err(|_| {
                    anyhow::Error::from(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "SOCKS connect timed out",
                    ))
                })
                .and_then(|result| result);
            let (tunnel, resolved) = match result {
                Ok(tunnel) => tunnel,
                Err(err) => {
                    write_reply(&mut client, reply_for_error(&err), unspecified_addr(local))
                        .await?;
                    return Err(err);
                }
            };
            // The exit reports the destination, not its local socket address.
            // An unspecified BND address is honest; echoing DST.ADDR was not.
            write_reply(
                &mut client,
                ReplyError::Succeeded,
                unspecified_addr(resolved),
            )
            .await?;
            let (read_client, write_client) = tokio::io::split(client);
            let (read_tunnel, write_tunnel) = tokio::io::split(tunnel);
            (
                litecopy(read_tunnel, write_client),
                litecopy(read_client, write_tunnel),
            )
                .try_join()
                .await?;
            Ok(())
        }
        consts::SOCKS5_CMD_UDP_ASSOCIATE => {
            handle_udp_associate(client, local, peer, target, open).await
        }
        _ => {
            write_reply(
                &mut client,
                ReplyError::CommandNotSupported,
                unspecified_addr(local),
            )
            .await
        }
    }
}

/// fast-socks5's high-level read_command ignores RSV and its reply helpers
/// use write rather than write_all. Keep the small fixed header here and use
/// the library's auth, address and UDP codecs for the variable wire formats.
async fn read_request<S>(client: &mut S) -> anyhow::Result<Option<(u8, TargetAddr)>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let [version, command, reserved, atyp] = {
        let mut header = [0; 4];
        client.read_exact(&mut header).await?;
        header
    };
    let failure = if version != consts::SOCKS5_VERSION || reserved != 0 {
        Some(ReplyError::GeneralFailure)
    } else if !matches!(
        command,
        consts::SOCKS5_CMD_TCP_CONNECT | consts::SOCKS5_CMD_UDP_ASSOCIATE
    ) {
        Some(ReplyError::CommandNotSupported)
    } else {
        None
    };
    if let Some(failure) = failure {
        write_reply(client, failure, "0.0.0.0:0".parse()?).await?;
        return Ok(None);
    }
    let target = match read_address(client, atyp).await {
        Ok(target) if valid_target(&target) => canonical_target(target),
        _ => {
            write_reply(
                client,
                ReplyError::AddressTypeNotSupported,
                "0.0.0.0:0".parse()?,
            )
            .await?;
            return Ok(None);
        }
    };
    Ok(Some((command, target)))
}

fn valid_target(target: &TargetAddr) -> bool {
    match target {
        TargetAddr::Ip(_) => true,
        // Preserve DNS names for resolution at the exit. Disallow delimiters
        // that could turn a domain into a different host:port when formatted.
        TargetAddr::Domain(host, _) => {
            !host.is_empty()
                && !host.chars().any(|c| {
                    c.is_control() || c.is_whitespace() || matches!(c, ':' | '[' | ']' | '/' | '\\')
                })
        }
    }
}

fn canonical_addr(addr: SocketAddr) -> SocketAddr {
    if let SocketAddr::V6(v6) = addr
        && let Some(v4) = v6.ip().to_ipv4_mapped()
    {
        SocketAddr::new(IpAddr::V4(v4), v6.port())
    } else {
        addr
    }
}

fn canonical_target(target: TargetAddr) -> TargetAddr {
    match target {
        TargetAddr::Ip(addr) => TargetAddr::Ip(canonical_addr(addr)),
        _ => target,
    }
}

fn unspecified_addr(addr: SocketAddr) -> SocketAddr {
    if addr.is_ipv4() {
        SocketAddr::from(([0u8; 4], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    }
}

async fn write_reply<S: AsyncWrite + Unpin>(
    client: &mut S,
    reply: ReplyError,
    bound: SocketAddr,
) -> anyhow::Result<()> {
    let mut bytes = vec![consts::SOCKS5_VERSION, reply.as_u8(), 0];
    bytes.extend(TargetAddr::Ip(bound).to_be_bytes()?);
    timeout(REPLY_TIMEOUT, async {
        client.write_all(&bytes).await?;
        client.flush().await
    })
    .await
    .context("SOCKS reply timed out")??;
    Ok(())
}

fn reply_for_error(err: &anyhow::Error) -> ReplyError {
    for source in err.chain() {
        if let Some(err) = source.downcast_ref::<io::Error>() {
            return match err.kind() {
                io::ErrorKind::PermissionDenied => ReplyError::ConnectionNotAllowed,
                io::ErrorKind::NetworkUnreachable => ReplyError::NetworkUnreachable,
                io::ErrorKind::HostUnreachable
                | io::ErrorKind::NotFound
                | io::ErrorKind::TimedOut => ReplyError::HostUnreachable,
                io::ErrorKind::ConnectionRefused => ReplyError::ConnectionRefused,
                _ => ReplyError::GeneralFailure,
            };
        }
    }
    // RichTunnelResponse::Fail transports exit errors as text. Linux errno
    // values here are from the exit, independent of the client's host OS.
    let message = format!("{err:#}").to_lowercase();
    if message.contains("not allowed")
        || message.contains("permission denied")
        || message.contains("os error 13")
    {
        ReplyError::ConnectionNotAllowed
    } else if message.contains("network unreachable")
        || message.contains("network is unreachable")
        || message.contains("os error 101")
    {
        ReplyError::NetworkUnreachable
    } else if message.contains("connection refused") || message.contains("os error 111") {
        ReplyError::ConnectionRefused
    } else if message.contains("resolve")
        || message.contains("no addrs")
        || message.contains("host unreachable")
        || message.contains("no route to host")
        || message.contains("os error 113")
        || message.contains("timeout")
        || message.contains("timed out")
    {
        ReplyError::HostUnreachable
    } else {
        ReplyError::GeneralFailure
    }
}

async fn handle_udp_associate<S>(
    mut client: S,
    mut local: SocketAddr,
    peer: SocketAddr,
    target: TargetAddr,
    open: TunnelOpener,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // RFC 1928 section 7: the TCP peer determines the permitted source IP.
    // A nonzero request port further constrains the client's UDP endpoint;
    // otherwise learn the port from the first valid, supported datagram.
    let requested_port = match target {
        TargetAddr::Ip(addr) => {
            if !addr.ip().is_unspecified() && addr.ip() != peer.ip() {
                return write_reply(
                    &mut client,
                    ReplyError::ConnectionNotAllowed,
                    unspecified_addr(local),
                )
                .await;
            }
            addr.port()
        }
        // No local DNS: the source IP is already known from the TCP peer.
        TargetAddr::Domain(_, port) => port,
    };
    local.set_port(0);
    let socket = match UdpSocket::bind(local).await {
        Ok(socket) => Arc::new(socket),
        Err(err) => {
            let err = anyhow::Error::from(err);
            write_reply(&mut client, reply_for_error(&err), unspecified_addr(local)).await?;
            return Err(err);
        }
    };
    write_reply(&mut client, ReplyError::Succeeded, socket.local_addr()?).await?;
    udp_association(client, socket, peer.ip(), requested_port, open).await
}

async fn udp_association<S: AsyncRead + Unpin>(
    mut client: S,
    socket: Arc<UdpSocket>,
    peer_ip: IpAddr,
    requested_port: u16,
    open: TunnelOpener,
) -> anyhow::Result<()> {
    let mut client_port = (requested_port != 0).then_some(requested_port);
    let mut tunnels: HashMap<TargetAddr, channel::Sender<Vec<u8>>> = HashMap::new();
    // JoinSet cancels every flow when the TCP control connection closes or its
    // handler is cancelled, and reaps failed/idle flows so they can be reopened.
    let mut tasks = JoinSet::new();
    let mut buf = vec![0; 65535];
    let mut control = [0; 1];
    loop {
        tokio::select! {
            biased;
            result = client.read(&mut control) => {
                anyhow::ensure!(result? == 0, "unexpected data on SOCKS UDP control connection");
                return Ok(());
            }
            Some(result) = tasks.join_next() => {
                let (target, result): (TargetAddr, anyhow::Result<()>) = result?;
                tunnels.remove(&target);
                if let Err(err) = result {
                    tracing::debug!(%target, err = %format!("{err:#}"), "SOCKS UDP flow ended");
                }
            }
            received = socket.recv_from(&mut buf) => {
                let (len, source) = received?;
                let source = canonical_addr(source);
                if source.ip() != peer_ip || client_port.is_some_and(|port| source.port() != port) {
                    continue;
                }
                // Invalid packets and unsupported fragments are silently dropped,
                // never tearing down an otherwise valid association.
                let Ok((0, target, payload)) = parse_udp_request(&buf[..len]).await else {
                    continue;
                };
                if !valid_target(&target) {
                    continue;
                }
                let target = canonical_target(target);
                // Existing exits support IPv4 UDP and their DNS service. DNS
                // payloads can use that service even for a local IPv6 resolver.
                if matches!(&target, TargetAddr::Ip(addr) if addr.is_ipv6() && addr.port() != 53) {
                    continue;
                }
                client_port = Some(source.port());
                if !tunnels.contains_key(&target) {
                    if tunnels.len() >= MAX_UDP_FLOWS {
                        continue;
                    }
                    let (tx, rx) = channel::bounded(UDP_QUEUE_LEN);
                    tunnels.insert(target.clone(), tx);
                    let target = target.clone();
                    let socket = socket.clone();
                    let open = open.clone();
                    tasks.spawn(async move {
                        let result = run_udp_tunnel(target.clone(), socket, source, rx, open).await;
                        (target, result)
                    });
                }
                // UDP may drop packets. One blocked destination must not stall
                // all other flows or keep the association alive after TCP EOF.
                let _ = tunnels[&target].try_send(payload.to_vec());
            }
        }
    }
}

async fn run_udp_tunnel(
    target: TargetAddr,
    socket: Arc<UdpSocket>,
    client_addr: SocketAddr,
    recv: channel::Receiver<Vec<u8>>,
    open: TunnelOpener,
) -> anyhow::Result<()> {
    let tunnel_target = match &target {
        TargetAddr::Ip(addr) if addr.is_ipv6() && addr.port() == 53 => "1.1.1.1:53".to_owned(),
        _ => target.to_string(),
    };
    let (pipe, resolved) = timeout(CONNECT_TIMEOUT, open("udp", tunnel_target))
        .await
        .context("SOCKS UDP tunnel open timed out")??;
    // Domain replies identify the exit's resolved remote endpoint. Literal
    // addresses retain their logical identity: DNS is intercepted, and fake-DNS
    // addresses are translated to domains by open_conn_with_addr.
    let reply_addr = match &target {
        TargetAddr::Ip(addr) => *addr,
        _ => resolved,
    };
    let header = new_udp_header(reply_addr)?;
    let (mut read_tunnel, mut write_tunnel) = tokio::io::split(pipe);
    let (activity, mut activity_rx) = tokio::sync::watch::channel(());
    let up = async {
        while let Ok(packet) = recv.recv().await {
            write_tunnel
                .write_all(&(packet.len() as u16).to_le_bytes())
                .await?;
            write_tunnel.write_all(&packet).await?;
            write_tunnel.flush().await?;
            activity.send_replace(());
        }
        anyhow::Ok(())
    };
    let down = async {
        loop {
            let len = read_tunnel.read_u16_le().await?;
            let mut packet = header.clone();
            packet.resize(header.len() + len as usize, 0);
            read_tunnel.read_exact(&mut packet[header.len()..]).await?;
            socket.send_to(&packet, client_addr).await?;
            activity.send_replace(());
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let idle = async {
        loop {
            timeout(UDP_IDLE_TIMEOUT, activity_rx.changed())
                .await
                .context("idle SOCKS UDP flow")??;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    (up, down, idle).race().await
}

#[cfg(test)]
mod tests;
