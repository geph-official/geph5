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
            .inspect_err(|e| {
                tracing::warn!(
                    err = display(e),
                    addr = debug(&remote_addr),
                    "TLS connection failed"
                )
            })
            .map_err(std::io::Error::other)?;
        tracing::warn!(addr = debug(&remote_addr), "TLS connection SUCCESS");
        Ok(TlsPipe {
            inner: tls_stream,
            remote_addr,
        })
    }
}

/// TlsListener wraps a Listener to accept TLS connections .
pub struct TlsListener<L: Listener> {
    // Channel that will yield successful TLS connections.
    incoming: tachyonix::Receiver<TlsPipe<L::P>>,
    // Keep the background task alive (cancels on drop).
    _accept_task: async_task::Task<()>,
}

impl<L: Listener> TlsListener<L>
where
    L::P: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub fn new(mut inner: L, acceptor: TlsAcceptor) -> Self {
        // Create a channel to send successfully negotiated TLS connections.
        let (tx, rx) = tachyonix::channel(1);

        let acceptor_clone = acceptor.clone();
        let accept_task = smolscale::spawn(async move {
            loop {
                // Pull the next raw connection from the underlying listener.
                let raw_conn = match inner.accept().await {
                    Ok(conn) => conn,
                    Err(err) => {
                        // Underlying listener failure: log and break out of the loop.
                        eprintln!("Underlying listener error: {:?}", err);
                        break;
                    }
                };

                // For each raw connection, spawn a task to perform the TLS handshake.
                let tx2 = tx.clone();
                let acceptor2 = acceptor_clone.clone();
                let remote_addr = raw_conn.remote_addr().map(|s| s.to_string());
                smolscale::spawn(async move {
                    match acceptor2.accept(raw_conn).await {
                        Ok(tls_stream) => {
                            let pipe = TlsPipe {
                                inner: tls_stream,
                                remote_addr,
                            };
                            let _ = tx2.send(pipe).await;
                        }
                        Err(e) => {
                            // Handshake failure: log but do not send an error.
                            eprintln!("TLS handshake error (ignored): {:?}", e);
                        }
                    }
                })
                .detach();
            }
        });

        TlsListener {
            incoming: rx,
            _accept_task: accept_task,
        }
    }
}

#[async_trait]
impl<L: Listener> Listener for TlsListener<L>
where
    L::P: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type P = TlsPipe<L::P>;

    async fn accept(&mut self) -> std::io::Result<Self::P> {
        // If a TLS connection is available, return it.
        // If the channel is closed (due to an underlying listener failure), return an error.
        match self.incoming.recv().await {
            Ok(pipe) => Ok(pipe),
            Err(_) => Err(std::io::Error::other("Underlying listener failure")),
        }
    }
}
