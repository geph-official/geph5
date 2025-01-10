use std::pin::Pin;

use async_native_tls::{TlsAcceptor, TlsConnector, TlsStream};
use async_trait::async_trait;
use futures_lite::{AsyncRead, AsyncWrite};

use sillad::{dialer::Dialer, listener::Listener, Pipe};

/// TlsPipe wraps a TLS stream to implement the Pipe trait.
pub struct TlsPipe<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> {
    inner: TlsStream<T>,
    remote_addr: Option<String>,
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncRead for TlsPipe<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncWrite for TlsPipe<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Pipe for TlsPipe<T> {
    fn protocol(&self) -> &str {
        "tls"
    }

    fn remote_addr(&self) -> Option<&str> {
        self.remote_addr.as_deref()
    }
}

/// TlsDialer wraps a Dialer to establish a TLS connection.
pub struct TlsDialer<D: Dialer> {
    inner: D,
    connector: TlsConnector,
    domain: String,
}

impl<D: Dialer> TlsDialer<D> {
    pub fn new(inner: D, connector: TlsConnector, domain: String) -> Self {
        Self {
            inner,
            connector,
            domain,
        }
    }
}

#[async_trait]
impl<D: Dialer> Dialer for TlsDialer<D>
where
    D::P: AsyncRead + AsyncWrite + Unpin + Send,
{
    type P = TlsPipe<D::P>;

    async fn dial(&self) -> std::io::Result<Self::P> {
        let stream = self.inner.dial().await?;
        let remote_addr = stream.remote_addr().map(|s| s.to_string());
        let tls_stream = self
            .connector
            .connect(&self.domain, stream)
            .await
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
        Ok(TlsPipe {
            inner: tls_stream,
            remote_addr,
        })
    }
}

/// TlsListener wraps a Listener to accept TLS connections.
pub struct TlsListener<L: Listener> {
    inner: L,
    acceptor: TlsAcceptor,
}

impl<L: Listener> TlsListener<L> {
    pub fn new(inner: L, acceptor: TlsAcceptor) -> Self {
        Self { inner, acceptor }
    }
}

#[async_trait]
impl<L: Listener> Listener for TlsListener<L>
where
    L::P: AsyncRead + AsyncWrite + Unpin + Send,
{
    type P = TlsPipe<L::P>;

    async fn accept(&mut self) -> std::io::Result<Self::P> {
        let stream = self.inner.accept().await?;
        let remote_addr = stream.remote_addr().map(|s| s.to_string());
        let tls_stream = self
            .acceptor
            .accept(stream)
            .await
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
        Ok(TlsPipe {
            inner: tls_stream,
            remote_addr,
        })
    }
}
