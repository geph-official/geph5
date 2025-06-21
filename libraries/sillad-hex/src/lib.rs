use std::pin::Pin;
use futures_util::{AsyncRead, AsyncWrite};
use pin_project::pin_project;
use std::task::{Context, Poll};
use async_trait::async_trait;
use sillad::{dialer::Dialer, listener::Listener, Pipe};

#[pin_project]
pub struct HexPipe<P: Pipe> {
    #[pin]
    inner: P,
    read_buf: Vec<u8>,
    leftover: Vec<u8>,
}

impl<P: Pipe> HexPipe<P> {
    pub fn new(inner: P) -> Self {
        Self { inner, read_buf: Vec::new(), leftover: Vec::new() }
    }
}

impl<P: Pipe> AsyncRead for HexPipe<P> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        let mut this = self.project();
        if !this.read_buf.is_empty() {
            let n = buf.len().min(this.read_buf.len());
            buf[..n].copy_from_slice(&this.read_buf[..n]);
            this.read_buf.drain(..n);
            return Poll::Ready(Ok(n));
        }
        let mut tmp = [0u8; 4096];
        match Pin::new(&mut this.inner).poll_read(cx, &mut tmp) {
            Poll::Ready(Ok(0)) => {
                if this.leftover.is_empty() {
                    Poll::Ready(Ok(0))
                } else {
                    Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "incomplete hex")))
                }
            }
            Poll::Ready(Ok(n)) => {
                this.leftover.extend_from_slice(&tmp[..n]);
                let decode_len = this.leftover.len() / 2 * 2;
                let decoded = match hex::decode(&this.leftover[..decode_len]) {
                    Ok(v) => v,
                    Err(_) => return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad hex"))),
                };
                this.read_buf.extend_from_slice(&decoded);
                this.leftover.drain(..decode_len);
                let n = buf.len().min(this.read_buf.len());
                buf[..n].copy_from_slice(&this.read_buf[..n]);
                this.read_buf.drain(..n);
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<P: Pipe> AsyncWrite for HexPipe<P> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let mut this = self.project();
        let encoded = hex::encode(buf);
        this.inner.as_mut().poll_write(cx, encoded.as_bytes()).map_ok(|_| buf.len())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut this = self.project();
        this.inner.as_mut().poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut this = self.project();
        this.inner.as_mut().poll_close(cx)
    }
}

impl<P: Pipe> Pipe for HexPipe<P> {
    fn shared_secret(&self) -> Option<&[u8]> { self.inner.shared_secret() }
    fn protocol(&self) -> &str { "hex" }
    fn remote_addr(&self) -> Option<&str> { self.inner.remote_addr() }
}

pub struct HexDialer<D: Dialer> { pub inner: D }

#[async_trait]
impl<D: Dialer> Dialer for HexDialer<D> {
    type P = HexPipe<D::P>;
    async fn dial(&self) -> std::io::Result<Self::P> { self.inner.dial().await.map(HexPipe::new) }
}

pub struct HexListener<L: Listener> { pub inner: L }

#[async_trait]
impl<L: Listener> Listener for HexListener<L> {
    type P = HexPipe<L::P>;
    async fn accept(&mut self) -> std::io::Result<Self::P> { self.inner.accept().await.map(HexPipe::new) }
}
