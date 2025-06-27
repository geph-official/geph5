use std::pin::Pin;

use async_task::Task;
use async_trait::async_trait;
use futures_util::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use pin_project::pin_project;
use sillad::{dialer::Dialer, listener::Listener, Pipe};

use bipe::{BipeReader, BipeWriter};

#[pin_project]
pub struct HexPipe {
    #[pin]
    read_incoming: BipeReader,
    _read_task: Task<()>,
    #[pin]
    write_outgoing: BipeWriter,
    _write_task: Task<()>,
    addr: Option<String>,
}

impl HexPipe {
    pub fn new<P: Pipe>(pipe: P) -> Self {
        let addr = pipe.remote_addr().map(|s| s.to_string());
        let (mut read_half, mut write_half) = pipe.split();
        let (mut write_incoming, read_incoming) = bipe::bipe(32768);
        let (write_outgoing, mut read_outgoing) = bipe::bipe(32768);

        let _read_task = smolscale::spawn(async move {
            let mut leftover = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        leftover.extend_from_slice(&buf[..n]);
                        let decode_len = leftover.len() / 2 * 2;
                        if decode_len == 0 {
                            continue;
                        }
                        match hex::decode(&leftover[..decode_len]) {
                            Ok(decoded) => {
                                if write_incoming.write_all(&decoded).await.is_err() {
                                    break;
                                }
                                leftover.drain(..decode_len);
                            }
                            Err(_) => break,
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let _write_task = smolscale::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match read_outgoing.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let encoded = hex::encode(&buf[..n]);
                        if write_half.write_all(encoded.as_bytes()).await.is_err() {
                            break;
                        }
                        if write_half.flush().await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            read_incoming,
            _read_task,
            write_outgoing,
            _write_task,
            addr,
        }
    }
}

impl AsyncRead for HexPipe {
    fn poll_read(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut [u8]) -> std::task::Poll<std::io::Result<usize>> {
        self.project().read_incoming.poll_read(cx, buf)
    }
}

impl AsyncWrite for HexPipe {
    fn poll_write(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        self.project().write_outgoing.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        self.project().write_outgoing.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        self.project().write_outgoing.poll_close(cx)
    }
}

impl Pipe for HexPipe {
    fn protocol(&self) -> &str { "hex" }
    fn remote_addr(&self) -> Option<&str> { self.addr.as_deref() }
}

pub struct HexDialer<D: Dialer> { pub inner: D }

#[async_trait]
impl<D: Dialer> Dialer for HexDialer<D> {
    type P = HexPipe;
    async fn dial(&self) -> std::io::Result<Self::P> { self.inner.dial().await.map(HexPipe::new) }
}

pub struct HexListener<L: Listener> { pub inner: L }

#[async_trait]
impl<L: Listener> Listener for HexListener<L> {
    type P = HexPipe;
    async fn accept(&mut self) -> std::io::Result<Self::P> { self.inner.accept().await.map(HexPipe::new) }
}

