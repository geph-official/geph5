//! Geph-owned TCP dialer that pins its outbound sockets to the physical interface
//! in full-tunnel VPN mode.
//!
//! Implemented as a `sillad::Dialer` — sillad's extension point for custom dial
//! behavior — so the interface-binding policy lives here in the engine.
//!
//! When the manager spawns the engine in full-tunnel mode it sets
//! `GEPH_VPN_BIND_IF4` / `GEPH_VPN_BIND_IF6` to the physical interface indices.
//! With those set, [`BoundTcpDialer`] pins each socket to that interface before
//! connecting (Windows `IP_UNICAST_IF`, macOS `IP_BOUND_IF`), so the engine's own
//! bridge/exit/broker traffic leaves the real NIC instead of looping back into the
//! tunnel. Without them (non-VPN, or platforms without pinning) it is a plain
//! connect. This is the single chokepoint for all of the engine's raw outbound
//! TCP — bridges, exits, LAN passthrough, the direct-TCP broker source, and the
//! broker egress forwarder.

use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use async_trait::async_trait;
use pin_project::pin_project;
use sillad::{Pipe, dialer::Dialer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Whether full-tunnel socket binding is active (the manager set the bind-index
/// env). Used to decide whether the broker's non-sillad HTTP clients must be
/// routed through the loopback forwarder. Cached.
pub fn binding_active() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var_os("GEPH_VPN_BIND_IF4").is_some()
            || std::env::var_os("GEPH_VPN_BIND_IF6").is_some()
    })
}

/// A TCP dialer that connects to a single address, pinned to the physical
/// interface when full-tunnel binding is active (otherwise a plain connect).
pub struct BoundTcpDialer {
    pub dest_addr: SocketAddr,
}

#[async_trait]
impl Dialer for BoundTcpDialer {
    type P = BoundTcpPipe;
    async fn dial(&self) -> std::io::Result<Self::P> {
        let inner = loop {
            match connect_bound(self.dest_addr).await {
                Ok(inner) => break inner,
                Err(err) if should_retry_connect(&err) => {
                    let _ = &err;
                    tracing::warn!(
                        addr = %self.dest_addr,
                        "retrying TCP connect after OS-level timeout"
                    );
                }
                Err(err) => return Err(err),
            }
        };
        let _ = inner.set_nodelay(true);
        Ok(BoundTcpPipe(inner, self.dest_addr.to_string()))
    }
}

/// Connect to the first of `addrs` that succeeds, in order, with the physical-NIC
/// pin. The caller resolves any hostname to `SocketAddr`s beforehand.
pub async fn connect_addrs(addrs: &[SocketAddr]) -> std::io::Result<BoundTcpPipe> {
    let mut last_err = std::io::Error::other("no addresses to connect to");
    for &dest_addr in addrs {
        match (BoundTcpDialer { dest_addr }).dial().await {
            Ok(pipe) => return Ok(pipe),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

/// A `Pipe` over a connected TCP stream (Geph's own, so `sillad` needs no public
/// constructor for its `TcpPipe`).
#[pin_project]
pub struct BoundTcpPipe(#[pin] tokio::net::TcpStream, String);

impl AsyncRead for BoundTcpPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.project().0.poll_read(cx, buf)
    }
}

impl AsyncWrite for BoundTcpPipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.project().0.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().0.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().0.poll_shutdown(cx)
    }
}

impl Pipe for BoundTcpPipe {
    fn protocol(&self) -> &str {
        "tcp"
    }

    fn remote_addr(&self) -> Option<&str> {
        Some(&self.1)
    }
}

/// Connect to `dest`, pinning the socket to the physical interface when one is
/// configured for the address family (Windows `IP_UNICAST_IF`, macOS
/// `IP_BOUND_IF`); a plain connect otherwise.
async fn connect_bound(dest: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
    #[cfg(windows)]
    {
        if let Some(stream) = windows_bind::connect_unicast_if(dest).await? {
            return Ok(stream);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(stream) = macos_bind::connect_bound_if(dest).await? {
            return Ok(stream);
        }
    }
    tokio::net::TcpStream::connect(dest).await
}

#[cfg(windows)]
fn should_retry_connect(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::TimedOut || err.raw_os_error() == Some(10060)
}

#[cfg(not(windows))]
fn should_retry_connect(_: &std::io::Error) -> bool {
    false
}

// IP_UNICAST_IF pinning. The bind index is read once from the manager-supplied
// env; absent (or 0), `connect_unicast_if` returns None and the caller falls back
// to an ordinary connect.
#[cfg(windows)]
mod windows_bind {
    use std::net::SocketAddr;
    use std::os::windows::io::AsRawSocket;
    use std::sync::OnceLock;

    use windows_sys::Win32::Networking::WinSock::{SOCKET, setsockopt};

    // From <ws2ipdef.h>: IPPROTO_IP = 0, IPPROTO_IPV6 = 41; IP_UNICAST_IF =
    // IPV6_UNICAST_IF = 31.
    const IPPROTO_IP: i32 = 0;
    const IPPROTO_IPV6: i32 = 41;
    const IP_UNICAST_IF: i32 = 31;
    const IPV6_UNICAST_IF: i32 = 31;

    fn env_index(var: &str) -> Option<u32> {
        std::env::var(var)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&i| i != 0)
    }

    fn bind_if4() -> Option<u32> {
        static V: OnceLock<Option<u32>> = OnceLock::new();
        *V.get_or_init(|| env_index("GEPH_VPN_BIND_IF4"))
    }

    fn bind_if6() -> Option<u32> {
        static V: OnceLock<Option<u32>> = OnceLock::new();
        *V.get_or_init(|| env_index("GEPH_VPN_BIND_IF6"))
    }

    /// If an interface is configured for `dest`'s family, create a socket pinned
    /// to it and connect; otherwise `Ok(None)` to fall back to a plain connect.
    pub async fn connect_unicast_if(
        dest: SocketAddr,
    ) -> std::io::Result<Option<tokio::net::TcpStream>> {
        let (level, optname, idx) = match dest {
            SocketAddr::V4(_) => match bind_if4() {
                Some(i) => (IPPROTO_IP, IP_UNICAST_IF, i),
                None => return Ok(None),
            },
            SocketAddr::V6(_) => match bind_if6() {
                Some(i) => (IPPROTO_IPV6, IPV6_UNICAST_IF, i),
                None => return Ok(None),
            },
        };

        let socket = match dest {
            SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
            SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
        };
        // Quirk: IP_UNICAST_IF takes the index in *network* byte order, while
        // IPV6_UNICAST_IF takes it in host byte order.
        let value: u32 = if optname == IP_UNICAST_IF {
            idx.to_be()
        } else {
            idx
        };
        let rc = unsafe {
            setsockopt(
                socket.as_raw_socket() as SOCKET,
                level,
                optname,
                &value as *const u32 as *const u8,
                std::mem::size_of::<u32>() as i32,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(socket.connect(dest).await?))
    }
}

// IP_BOUND_IF pinning — the macOS analogue of `windows_bind`. The bind index is
// read once from the manager-supplied env; absent (or 0), `connect_bound_if`
// returns None and the caller falls back to an ordinary connect.
#[cfg(target_os = "macos")]
mod macos_bind {
    use std::net::SocketAddr;
    use std::os::fd::AsRawFd;
    use std::sync::OnceLock;

    // From <netinet/in.h>: IPPROTO_IP = 0, IP_BOUND_IF = 25.
    // From <netinet/in.h> (IPPROTO_IPV6 = 41) and <netinet6/in6.h>: IPV6_BOUND_IF = 125.
    const IPPROTO_IP: i32 = 0;
    const IPPROTO_IPV6: i32 = 41;
    const IP_BOUND_IF: i32 = 25;
    const IPV6_BOUND_IF: i32 = 125;

    fn env_index(var: &str) -> Option<u32> {
        std::env::var(var)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&i| i != 0)
    }

    fn bind_if4() -> Option<u32> {
        static V: OnceLock<Option<u32>> = OnceLock::new();
        *V.get_or_init(|| env_index("GEPH_VPN_BIND_IF4"))
    }

    fn bind_if6() -> Option<u32> {
        static V: OnceLock<Option<u32>> = OnceLock::new();
        *V.get_or_init(|| env_index("GEPH_VPN_BIND_IF6"))
    }

    /// If an interface is configured for `dest`'s family, create a socket bound to
    /// it and connect; otherwise `Ok(None)` to fall back to a plain connect.
    pub async fn connect_bound_if(
        dest: SocketAddr,
    ) -> std::io::Result<Option<tokio::net::TcpStream>> {
        let (level, optname, idx) = match dest {
            SocketAddr::V4(_) => match bind_if4() {
                Some(i) => (IPPROTO_IP, IP_BOUND_IF, i),
                None => return Ok(None),
            },
            SocketAddr::V6(_) => match bind_if6() {
                Some(i) => (IPPROTO_IPV6, IPV6_BOUND_IF, i),
                None => return Ok(None),
            },
        };

        let socket = match dest {
            SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
            SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
        };
        // IP_BOUND_IF/IPV6_BOUND_IF both take the interface index in host byte
        // order (no Windows-style network-byte-order quirk).
        let value: u32 = idx;
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                optname,
                &value as *const u32 as *const libc::c_void,
                std::mem::size_of::<u32>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(socket.connect(dest).await?))
    }
}
