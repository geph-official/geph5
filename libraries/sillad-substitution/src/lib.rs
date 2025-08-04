use async_task::Task;
use async_trait::async_trait;
use bipe::{BipeReader, BipeWriter};
use futures_util::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use pin_project::pin_project;
use sillad::{dialer::Dialer, listener::Listener, Pipe};
use smolscale::spawn;
use std::{io, pin::Pin};

#[pin_project]
pub struct SubstitutionPipe {
    #[pin]
    read_incoming: BipeReader,
    _read_task: Task<()>,
    #[pin]
    write_outgoing: BipeWriter,
    _write_task: Task<()>,
    addr: Option<String>,
}

impl SubstitutionPipe {
    /// `map` is a 256-byte array such that plaintext byte `b` maps to `map[b as usize]`.
    pub fn new<P: Pipe>(pipe: P, map: [u8; 256]) -> Self {
        // Compute inverse map for decoding
        let mut inv_map = [0u8; 256];
        for (i, &m) in map.iter().enumerate() {
            inv_map[m as usize] = i as u8;
        }

        let addr = pipe.remote_addr().map(|s| s.to_string());
        let (mut read_half, mut write_half) = pipe.split();
        let (mut write_incoming, read_incoming) = bipe::bipe(32768);
        let (write_outgoing, mut read_outgoing) = bipe::bipe(32768);

        // Task: read encrypted bytes, decode, feed into read_incoming
        let inv_map_task = inv_map;
        let _read_task = spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        // Decode each byte
                        let mut decoded = Vec::with_capacity(n);
                        for &b in &buf[..n] {
                            decoded.push(inv_map_task[b as usize]);
                        }
                        if write_incoming.write_all(&decoded).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Task: read plaintext bytes, encode, write out
        let map_task = map;
        let _write_task = spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match read_outgoing.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        // Encode each byte
                        let mut encoded = Vec::with_capacity(n);
                        for &b in &buf[..n] {
                            encoded.push(map_task[b as usize]);
                        }
                        if write_half.write_all(&encoded).await.is_err() {
                            break;
                        }
                        let _ = write_half.flush().await;
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

impl AsyncRead for SubstitutionPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<io::Result<usize>> {
        self.project().read_incoming.poll_read(cx, buf)
    }
}

impl AsyncWrite for SubstitutionPipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        self.project().write_outgoing.poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.project().write_outgoing.poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.project().write_outgoing.poll_close(cx)
    }
}

impl Pipe for SubstitutionPipe {
    fn protocol(&self) -> &str {
        "substitution"
    }
    fn remote_addr(&self) -> Option<&str> {
        self.addr.as_deref()
    }
}

/// A Dialer wrapper that injects the substitution map when dialing.
pub struct SubstitutionDialer<D: Dialer> {
    pub inner: D,
    pub map: [u8; 256],
}

#[async_trait]
impl<D: Dialer> Dialer for SubstitutionDialer<D> {
    type P = SubstitutionPipe;

    async fn dial(&self) -> io::Result<Self::P> {
        self.inner
            .dial()
            .await
            .map(|pipe| SubstitutionPipe::new(pipe, self.map))
    }
}

/// A Listener wrapper that injects the substitution map on accept.
pub struct SubstitutionListener<L: Listener> {
    pub inner: L,
    pub map: [u8; 256],
}

#[async_trait]
impl<L: Listener> Listener for SubstitutionListener<L> {
    type P = SubstitutionPipe;

    async fn accept(&mut self) -> io::Result<Self::P> {
        self.inner
            .accept()
            .await
            .map(|pipe| SubstitutionPipe::new(pipe, self.map))
    }
}
