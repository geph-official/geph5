use anyctx::AnyCtx;
use async_compat::{Compat, CompatExt};
use hyper::Uri;
use hyper_util::client::legacy::connect::Connection;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{self, Poll},
};

use crate::{Config, session::open_conn};

#[derive(Clone)]
pub struct Connector {
    ctx: AnyCtx<Config>,
}

impl Connector {
    pub fn new(ctx: AnyCtx<Config>) -> Self {
        Self { ctx }
    }
}

impl tower_service::Service<Uri> for Connector {
    type Error = std::io::Error;
    type Future = Connecting;
    type Response = HyperRtCompat<TunneledConnection>;

    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let ctx = self.ctx.clone();
        Connecting {
            fut: Box::pin(async move {
                let host = dst.host().ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "URI must include host")
                })?;
                let port = dst
                    .port_u16()
                    .unwrap_or_else(|| if dst.scheme_str() == Some("https") { 443 } else { 80 });
                let remote = format!("{host}:{port}");
                open_conn(&ctx, "tcp", &remote)
                    .await
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))
                    .map(|conn| HyperRtCompat::new(TunneledConnection(conn.compat())))
            }),
        }
    }
}

#[pin_project]
pub struct Connecting {
    #[pin]
    fut: Pin<Box<dyn Future<Output = std::io::Result<HyperRtCompat<TunneledConnection>>> + Send>>,
}

impl Future for Connecting {
    type Output = std::io::Result<HyperRtCompat<TunneledConnection>>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

pub struct TunneledConnection(Compat<Box<dyn sillad::Pipe>>);

impl TunneledConnection {
    pub fn new(conn: Box<dyn sillad::Pipe>) -> Self {
        Self(conn.compat())
    }
}

impl Connection for TunneledConnection {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        hyper_util::client::legacy::connect::Connected::new()
    }
}

impl tokio::io::AsyncRead for TunneledConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for TunneledConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[derive(Debug)]
pub struct HyperRtCompat<T>(pub(crate) T);

impl<T> HyperRtCompat<T> {
    pub fn new(io: T) -> Self {
        Self(io)
    }

    fn project(self: Pin<&mut Self>) -> Pin<&mut T> {
        unsafe { self.map_unchecked_mut(|me| &mut me.0) }
    }
}

impl<T> tokio::io::AsyncRead for HyperRtCompat<T>
where
    T: hyper::rt::Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        tbuf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let filled = tbuf.filled().len();
        let init = tbuf.initialized().len();
        let new_filled = unsafe {
            let mut buf = hyper::rt::ReadBuf::uninit(tbuf.unfilled_mut());
            match hyper::rt::Read::poll_read(self.project(), cx, buf.unfilled()) {
                Poll::Ready(Ok(())) => buf.filled().len(),
                other => return other,
            }
        };

        if filled + new_filled > init {
            unsafe {
                tbuf.assume_init(filled + new_filled - init);
            }
        }
        tbuf.set_filled(filled + new_filled);
        Poll::Ready(Ok(()))
    }
}

impl<T> tokio::io::AsyncWrite for HyperRtCompat<T>
where
    T: hyper::rt::Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        hyper::rt::Write::poll_write(self.project(), cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        hyper::rt::Write::poll_flush(self.project(), cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        hyper::rt::Write::poll_shutdown(self.project(), cx)
    }

    fn is_write_vectored(&self) -> bool {
        hyper::rt::Write::is_write_vectored(&self.0)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        hyper::rt::Write::poll_write_vectored(self.project(), cx, bufs)
    }
}

impl<T> hyper::rt::Read for HyperRtCompat<T>
where
    T: tokio::io::AsyncRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let n = unsafe {
            let mut tbuf = tokio::io::ReadBuf::uninit(buf.as_mut());
            match tokio::io::AsyncRead::poll_read(self.project(), cx, &mut tbuf) {
                Poll::Ready(Ok(())) => tbuf.filled().len(),
                other => return other,
            }
        };
        unsafe {
            buf.advance(n);
        }
        Poll::Ready(Ok(()))
    }
}

impl<T> hyper::rt::Write for HyperRtCompat<T>
where
    T: tokio::io::AsyncWrite,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        tokio::io::AsyncWrite::poll_write(self.project(), cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_flush(self.project(), cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_shutdown(self.project(), cx)
    }

    fn is_write_vectored(&self) -> bool {
        tokio::io::AsyncWrite::is_write_vectored(&self.0)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        tokio::io::AsyncWrite::poll_write_vectored(self.project(), cx, bufs)
    }
}

impl<T> hyper_util::client::legacy::connect::Connection for HyperRtCompat<T>
where
    T: hyper_util::client::legacy::connect::Connection,
{
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        self.0.connected()
    }
}
