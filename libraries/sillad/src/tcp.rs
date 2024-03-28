use std::{net::SocketAddr, net::TcpStream};

use async_io::Async;
use async_trait::async_trait;
use futures_lite::{AsyncRead, AsyncWrite};
use pin_project::pin_project;

use crate::{dialer::Dialer, listener::Listener, Pipe};

/// A TcpListener is a listener for TCP endpoints.
pub struct TcpListener {
    inner: Async<std::net::TcpListener>,
}

impl TcpListener {
    /// Creates a new TcpListener by listening to a particular address.
    pub async fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        let new = Async::<std::net::TcpListener>::bind(addr)?;
        Ok(Self { inner: new })
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
        let (conn, _) = self.inner.accept().await?;
        set_tcp_options(&conn)?;
        let addr = conn.as_ref().peer_addr()?.to_string();
        Ok(TcpPipe(conn, addr))
    }
}

fn set_tcp_options(conn: &Async<TcpStream>) -> std::io::Result<()> {
    conn.get_ref().set_nodelay(true)?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        use std::os::fd::AsRawFd;
        let lowat: libc::c_int = 32768;
        let ret = libc::setsockopt(
            conn.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_NOTSENT_LOWAT,
            &lowat as *const _ as *const libc::c_void,
            std::mem::size_of_val(&lowat) as libc::socklen_t,
        );
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A TcpDialer is a dialer for TCP endpoints. It is configured by its fields.
pub struct TcpDialer {
    pub dest_addr: SocketAddr,
}

#[async_trait]
impl Dialer for TcpDialer {
    type P = TcpPipe;
    async fn dial(&self) -> std::io::Result<Self::P> {
        let inner = Async::<TcpStream>::connect(self.dest_addr).await?;
        set_tcp_options(&inner)?;
        Ok(TcpPipe(inner, self.dest_addr.to_string()))
    }
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
