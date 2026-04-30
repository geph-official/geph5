use std::{
    io::ErrorKind,
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use async_io::Async;
use async_trait::async_trait;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use futures_lite::{AsyncRead, AsyncWrite};
use pin_project::pin_project;
use rand::Rng as _;

use crate::{
    Pipe,
    dialer::{Dialer, DialerExt},
    listener::Listener,
};

/// A TcpListener is a listener for TCP endpoints.
pub struct TcpListener {
    inner: Async<std::net::TcpListener>,
}

const INITIAL_ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(5);
const MAX_ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

impl TcpListener {
    /// Creates a new TcpListener by listening to a particular address.
    pub async fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        Self::bind_with_v6_only(addr, false).await
    }

    /// Creates a new TcpListener and optionally forces IPv6 sockets to be IPv6-only.
    pub async fn bind_with_v6_only(addr: SocketAddr, v6_only: bool) -> std::io::Result<Self> {
        let domain = match addr {
            SocketAddr::V4(_) => Domain::IPV4,
            SocketAddr::V6(_) => Domain::IPV6,
        };
        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_address(true)?;
        if matches!(addr, SocketAddr::V6(_)) {
            socket.set_only_v6(v6_only)?;
        }
        socket.bind(&SockAddr::from(addr))?;
        socket.listen(1024)?;
        let listener: std::net::TcpListener = socket.into();
        listener.set_nonblocking(true)?;
        let inner = Async::new(listener)?;
        Ok(Self { inner })
    }

    /// Get the local listening address.
    pub async fn local_addr(&self) -> SocketAddr {
        self.inner.as_ref().local_addr().unwrap()
    }
}

#[async_trait]
impl Listener for TcpListener {
    type P = TcpPipe;
    async fn accept(&mut self) -> std::io::Result<Self::P> {
        let mut retry_delay = INITIAL_ACCEPT_RETRY_DELAY;
        loop {
            match self.inner.accept().await {
                Ok((conn, _)) => {
                    retry_delay = INITIAL_ACCEPT_RETRY_DELAY;

                    if let Err(err) = set_tcp_options(&conn) {
                        tracing::warn!(
                            err = debug(&err),
                            "dropping accepted connection after failing to set TCP options"
                        );
                        continue;
                    }

                    let addr = match conn.as_ref().peer_addr() {
                        Ok(addr) => addr.to_string(),
                        Err(err) => {
                            tracing::warn!(
                                err = debug(&err),
                                "dropping accepted connection after failing to read peer address"
                            );
                            continue;
                        }
                    };

                    return Ok(TcpPipe(conn, addr));
                }
                Err(err) if should_retry_accept(&err) => {
                    tracing::warn!(
                        err = debug(&err),
                        raw_os_error = err.raw_os_error(),
                        retry_delay_ms = retry_delay.as_millis(),
                        "transient TCP accept failure; retrying"
                    );
                    async_io::Timer::after(jitter_accept_retry_delay(retry_delay)).await;
                    retry_delay = next_accept_retry_delay(retry_delay);
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }
    }
}

fn set_tcp_options(conn: &Async<TcpStream>) -> std::io::Result<()> {
    conn.get_ref().set_nodelay(true)?;

    // #[cfg(any(target_os = "linux", target_os = "android"))]
    // unsafe {
    //     use std::os::fd::AsRawFd;
    //     let lowat: libc::c_int = 32768;
    //     let ret = libc::setsockopt(
    //         conn.as_raw_fd(),
    //         libc::IPPROTO_TCP,
    //         libc::TCP_NOTSENT_LOWAT,
    //         &lowat as *const _ as *const libc::c_void,
    //         std::mem::size_of_val(&lowat) as libc::socklen_t,
    //     );
    //     if ret != 0 {
    //         return Err(std::io::Error::last_os_error());
    //     }
    // }
    Ok(())
}

fn should_retry_accept(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::WouldBlock
            | ErrorKind::Interrupted
            | ErrorKind::ConnectionAborted
            | ErrorKind::TimedOut
            | ErrorKind::OutOfMemory
    ) || err
        .raw_os_error()
        .is_some_and(should_retry_accept_raw_os_error)
}

#[cfg(unix)]
fn should_retry_accept_raw_os_error(err: i32) -> bool {
    matches!(
        err,
        libc::EAGAIN
            | libc::ECONNABORTED
            | libc::EINTR
            | libc::ENETDOWN
            | libc::EPROTO
            | libc::ENOPROTOOPT
            | libc::EHOSTDOWN
            | libc::EHOSTUNREACH
            | libc::EOPNOTSUPP
            | libc::ENETUNREACH
            | libc::ETIMEDOUT
            | libc::ENOBUFS
            | libc::ENOMEM
            | libc::EMFILE
            | libc::ENFILE
    ) || is_linux_accept_retry_errno(err)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn is_linux_accept_retry_errno(err: i32) -> bool {
    matches!(err, libc::ENONET)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn is_linux_accept_retry_errno(_: i32) -> bool {
    false
}

#[cfg(windows)]
fn should_retry_accept_raw_os_error(err: i32) -> bool {
    matches!(
        err,
        10004
            | 10035
            | 10053
            | 10060
            | 10050
            | 10051
            | 10064
            | 10065
            | 10055
            | 10024
    )
}

fn next_accept_retry_delay(delay: Duration) -> Duration {
    std::cmp::min(delay.saturating_mul(2), MAX_ACCEPT_RETRY_DELAY)
}

fn jitter_accept_retry_delay(delay: Duration) -> Duration {
    let lower_ms = delay.as_millis() as u64;
    let upper_ms = next_accept_retry_delay(delay).as_millis() as u64;
    Duration::from_millis(rand::thread_rng().gen_range(lower_ms..=upper_ms))
}

/// A HappyEyeballsTcpDialer is a dialer for TCP endpoints which tries the given addresses in sequence intelligently.
pub struct HappyEyeballsTcpDialer(pub Vec<SocketAddr>);

#[async_trait]
impl Dialer for HappyEyeballsTcpDialer {
    type P = Box<dyn Pipe>;
    async fn dial(&self) -> std::io::Result<Self::P> {
        let res = self
            .0
            .iter()
            .enumerate()
            .map(|(idx, addr)| {
                let delay = Duration::from_millis(250 * idx as u64);
                TcpDialer { dest_addr: *addr }.delay(delay).dynamic()
            })
            .reduce(|a, b| a.race(b).dynamic());
        match res {
            None => Err(std::io::Error::other("no addresses given")),
            Some(dialer) => dialer.dial().await,
        }
    }
}

/// A TcpDialer is a dialer for TCP endpoints. It is configured by its fields.
pub struct TcpDialer {
    pub dest_addr: SocketAddr,
}

#[async_trait]
impl Dialer for TcpDialer {
    type P = TcpPipe;
    async fn dial(&self) -> std::io::Result<Self::P> {
        let inner = loop {
            match Async::<TcpStream>::connect(self.dest_addr).await {
                Ok(inner) => break inner,
                Err(err) if should_retry_connect(&err) => {
                    tracing::warn!(
                        err = debug(&err),
                        addr = display(self.dest_addr),
                        "retrying TCP connect after OS-level timeout"
                    );
                }
                Err(err) => {
                    tracing::warn!("inner dial failed: {:?}", err);
                    return Err(err);
                }
            }
        };
        let _ =
            set_tcp_options(&inner).inspect_err(|e| tracing::warn!("tcp option set fail: {:?}", e));
        Ok(TcpPipe(inner, self.dest_addr.to_string()))
    }
}

#[cfg(windows)]
fn should_retry_connect(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::TimedOut || err.raw_os_error() == Some(10060)
}

#[cfg(not(windows))]
fn should_retry_connect(_: &std::io::Error) -> bool {
    false
}

#[pin_project]
pub struct TcpPipe(#[pin] Async<TcpStream>, String);

impl AsyncRead for TcpPipe {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().0.poll_read(cx, buf)
    }
}

impl AsyncWrite for TcpPipe {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().0.poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().0.poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().0.poll_close(cx)
    }
}

impl Pipe for TcpPipe {
    fn protocol(&self) -> &str {
        "tcp"
    }

    fn remote_addr(&self) -> Option<&str> {
        Some(&self.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_common_transient_accept_kinds() {
        assert!(should_retry_accept(&std::io::Error::from(ErrorKind::Interrupted)));
        assert!(should_retry_accept(&std::io::Error::from(
            ErrorKind::ConnectionAborted
        )));
        assert!(should_retry_accept(&std::io::Error::from(ErrorKind::TimedOut)));
    }

    #[test]
    fn reject_common_hard_accept_kinds() {
        assert!(!should_retry_accept(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
        assert!(!should_retry_accept(&std::io::Error::from(ErrorKind::InvalidInput)));
    }
}
