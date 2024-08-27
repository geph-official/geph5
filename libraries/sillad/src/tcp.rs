use std::{
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use async_io::Async;
use async_trait::async_trait;

use futures_lite::{AsyncRead, AsyncWrite};
use pin_project::pin_project;
use rand::Rng as _;

use crate::{
    dialer::{Dialer, DialerExt},
    listener::Listener,
    Pipe,
};

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
        let mut delay_secs = 10.0f64;
        loop {
            let fallible = async {
                let (conn, _) = self
                    .inner
                    .accept()
                    .await
                    .inspect_err(|e| tracing::error!(err = debug(e), "failed to accept"))?;
                set_tcp_options(&conn).inspect_err(|e| {
                    tracing::error!(err = debug(e), "failed to set TCP options")
                })?;
                let addr = conn.as_ref().peer_addr()?.to_string();
                anyhow::Ok(TcpPipe(conn, addr))
            };
            match fallible.await {
                Ok(pipe) => {
                    return Ok(pipe);
                }
                Err(e) => {
                    tracing::error!(
                        err = debug(e),
                        delay_secs,
                        "backing off and retrying accept"
                    );
                    delay_secs = rand::thread_rng().gen_range(delay_secs..delay_secs * 2.0);
                    async_io::Timer::after(Duration::from_secs_f64(delay_secs)).await;
                }
            }
        }
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
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "no addresses given",
            )),
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
