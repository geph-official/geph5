use std::{net::SocketAddr, net::TcpStream};

use async_io::Async;
use async_trait::async_trait;
use futures_lite::{AsyncRead, AsyncWrite};
use pin_project::pin_project;

use crate::{dialer::Dialer, Pipe};

/// A TcpDialer is a dialer for TCP endpoints. It is configured by its fields.
pub struct TcpDialer {
    pub dest_addr: SocketAddr,
}

#[async_trait]
impl Dialer for TcpDialer {
    type P = TcpPipe;
    async fn dial(&self) -> std::io::Result<Self::P> {
        let inner = Async::<TcpStream>::connect(self.dest_addr).await?;
        Ok(TcpPipe(inner))
    }
}

#[pin_project]
pub struct TcpPipe(#[pin] Async<TcpStream>);

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

impl Pipe for TcpPipe {}
