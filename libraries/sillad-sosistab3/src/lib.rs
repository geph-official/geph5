use std::{
    collections::VecDeque,
    fmt::Debug,
    io::{ErrorKind, Read, Write},
    task::Poll,
};

use bipe::BipeWriter;
use futures_util::{io::ReadHalf, AsyncRead, AsyncReadExt, AsyncWrite};
use pin_project::pin_project;

use serde::{Deserialize, Serialize};
use sillad::Pipe;
use state::State;

mod dedup;
pub mod dialer;
mod handshake;
pub mod listener;
mod state;

#[derive(Clone, Copy)]
pub struct Cookie {
    key: [u8; 32],
    params: ObfsParams,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, Debug)]
pub struct ObfsParams {
    // whether or not to pad write lengths
    pub obfs_lengths: bool,
    // whether or not to add delays
    pub obfs_timing: bool,
}

impl Debug for Cookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format!(
            "{}---{}",
            hex::encode(self.key),
            serde_json::to_string(&self.params).unwrap()
        )
        .fmt(f)
    }
}

impl Cookie {
    /// Derives a cookie from a string.
    pub fn new(s: &str) -> Self {
        let (cookie, params) = if let Some((a, b)) = s.split_once("---") {
            (a, serde_json::from_str(b).unwrap_or_default())
        } else {
            (s, ObfsParams::default())
        };
        let derived_cookie = blake3::derive_key("cookie", cookie.as_bytes());
        Self {
            key: derived_cookie,
            params,
        }
    }

    /// Randomly generates a cookie.
    pub fn random() -> Self {
        Self {
            key: rand::random(),
            params: ObfsParams::default(),
        }
    }

    /// Randomly create a cookie with the given parameters.
    pub fn random_with_params(params: ObfsParams) -> Self {
        Self {
            key: rand::random(),
            params,
        }
    }

    /// Derives a key given the direction.
    pub fn derive_key(&self, is_server: bool) -> [u8; 32] {
        blake3::derive_key(if is_server { "server" } else { "client" }, &self.key)
    }
}

/// An established sosistab3 connection.
#[pin_project]
pub struct SosistabPipe<P: Pipe> {
    #[pin]
    lower_read: ReadHalf<P>,
    #[pin]
    bipe_write: BipeWriter,

    addr: Option<String>,

    state: State,

    read_buf: VecDeque<u8>,
    read_closed: bool,
    raw_read_buf: Vec<u8>,

    to_write_buf: Vec<u8>,
}

impl<P: Pipe> SosistabPipe<P> {
    fn new(lower: P, state: State) -> Self {
        let addr = lower.remote_addr().map(|s| s.to_string());
        let (lower_read, mut lower_write) = lower.split();
        let (bipe_write, bipe_read) = bipe::bipe(32768);
        smolscale::spawn(async move {
            let _ = futures_util::io::copy_buf(
                futures_util::io::BufReader::with_capacity(32768, bipe_read),
                &mut lower_write,
            )
            .await;
        })
        .detach();
        Self {
            lower_read,
            bipe_write,
            state,
            read_buf: Default::default(),
            addr,
            read_closed: false,
            raw_read_buf: Default::default(),
            to_write_buf: Default::default(),
        }
    }
}

impl<P: Pipe> AsyncWrite for SosistabPipe<P> {
    #[tracing::instrument(name = "sosistab_write", skip(self, cx, buf))]
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.project();
        this.bipe_write.poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.project();
        this.bipe_write.poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.project().bipe_write.poll_close(cx)
    }
}

impl<P: Pipe> AsyncRead for SosistabPipe<P> {
    #[tracing::instrument(name = "sosistab_read", skip(self, cx, buf))]
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut this = self.project();
        loop {
            if !this.read_buf.is_empty() || *this.read_closed {
                tracing::trace!(buf_len = this.read_buf.len(), "reading from the read_buf");
                return Poll::Ready(this.read_buf.read(buf));
            } else {
                // we reuse buf as a temporary buffer
                let n = futures_util::ready!(this.lower_read.as_mut().poll_read(cx, buf));
                match n {
                    Err(e) => return Poll::Ready(Err(e)),
                    Ok(n) => {
                        if n == 0 {
                            *this.read_closed = true;
                            continue;
                        }
                        this.raw_read_buf.write_all(&buf[..n]).unwrap();
                        tracing::trace!(
                            n,
                            raw_buf_len = this.raw_read_buf.len(),
                            buf_len = this.read_buf.len(),
                            "read returned from lower"
                        );
                        // attempt to decrypt in order to fill the read_buf. we decrypt as many fragments as possible until we cannot decrypt anymore. at that point, we would need more fresh data to decrypt more.
                        loop {
                            match this.state.decrypt(this.raw_read_buf, &mut this.read_buf) {
                                Ok(result) => {
                                    tracing::trace!(
                                        n,
                                        raw_read_len = this.raw_read_buf.len(),
                                        buf_len = this.read_buf.len(),
                                        "decryption is successful"
                                    );
                                    this.raw_read_buf.drain(..result);
                                }
                                Err(err) => {
                                    tracing::trace!(
                                        n,
                                        raw_read_len = this.raw_read_buf.len(),
                                        buf_len = this.read_buf.len(),
                                        "could not decrypt yet due to {:?}",
                                        err
                                    );
                                    if err.kind() == ErrorKind::BrokenPipe {
                                        return Poll::Ready(Err(err));
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

impl<P: Pipe> Pipe for SosistabPipe<P> {
    fn protocol(&self) -> &str {
        "sosistab3"
    }

    fn remote_addr(&self) -> Option<&str> {
        self.addr.as_deref()
    }

    fn shared_secret(&self) -> Option<&[u8]> {
        Some(self.state.shared_secret())
    }
}
