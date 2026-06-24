use std::{
    collections::VecDeque,
    fmt::Debug,
    io::{ErrorKind, Read, Write},
    task::{Poll, ready},
};

use pin_project::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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
    lower: P,
    state: State,

    read_buf: VecDeque<u8>,
    read_closed: bool,
    raw_read_buf: Vec<u8>,

    to_write_buf: Vec<u8>,
}

impl<P: Pipe> SosistabPipe<P> {
    fn new(lower: P, state: State) -> Self {
        Self {
            lower,
            state,
            read_buf: Default::default(),
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
        // This implementation here is technically incorrect, if the caller doesn't poll the *same* buffer until completion.
        // But it seems like it's not possible to be technically correct without spawning a background thread and introducing an extra copy, and this is pretty hot code.

        let mut this = self.project();
        if this.to_write_buf.is_empty() {
            this.state.encrypt(buf, this.to_write_buf);
        }
        loop {
            tracing::trace!(bytes_to_write = this.to_write_buf.len(), "polling write");
            let res = ready!(this.lower.as_mut().poll_write(cx, this.to_write_buf));
            match res {
                Ok(n) => {
                    tracing::trace!(
                        bytes_to_write = this.to_write_buf.len(),
                        just_wrote = n,
                        plain_n = buf.len(),
                        "successfully wrote"
                    );
                    this.to_write_buf.drain(..n);
                    if this.to_write_buf.is_empty() {
                        tracing::trace!(
                            bytes_to_write = this.to_write_buf.len(),
                            just_wrote = n,
                            "returning Ready from write"
                        );
                        return Poll::Ready(Ok(buf.len()));
                    }
                }
                Err(err) => return Poll::Ready(Err(err)),
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut this = self.project();
        if !this.to_write_buf.is_empty() {
            match ready!(this.lower.as_mut().poll_write(cx, this.to_write_buf)) {
                Ok(n) => {
                    this.to_write_buf.drain(..n);
                    if !this.to_write_buf.is_empty() {
                        return Poll::Pending;
                    }
                }
                Err(err) => {
                    return Poll::Ready(Err(err));
                }
            }
        }
        this.lower.poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.project().lower.poll_shutdown(cx)
    }
}

impl<P: Pipe> AsyncRead for SosistabPipe<P> {
    #[tracing::instrument(name = "sosistab_read", skip(self, cx, buf))]
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let mut this = self.project();
        loop {
            if !this.read_buf.is_empty() || *this.read_closed {
                tracing::trace!(buf_len = this.read_buf.len(), "reading from the read_buf");
                // Drain as many decrypted bytes as the caller's buffer can hold.
                // (When read_buf is empty and the stream is closed, this copies
                // nothing, signaling EOF.)
                let to_copy = this.read_buf.len().min(buf.remaining());
                if to_copy > 0 {
                    let dst = buf.initialize_unfilled_to(to_copy);
                    // Reading from a VecDeque<u8> is infallible.
                    let n = this.read_buf.read(&mut dst[..to_copy]).unwrap_or(0);
                    buf.advance(n);
                }
                return Poll::Ready(Ok(()));
            } else {
                // Read raw ciphertext from the lower pipe into a scratch buffer.
                let mut scratch = [0u8; 16384];
                let mut scratch_buf = ReadBuf::new(&mut scratch);
                let res = ready!(this.lower.as_mut().poll_read(cx, &mut scratch_buf));
                match res {
                    Err(e) => return Poll::Ready(Err(e)),
                    Ok(()) => {
                        let filled = scratch_buf.filled();
                        let n = filled.len();
                        if n == 0 {
                            *this.read_closed = true;
                            continue;
                        }
                        this.raw_read_buf.write_all(filled).unwrap();
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
        self.lower.remote_addr()
    }

    fn shared_secret(&self) -> Option<&[u8]> {
        Some(self.state.shared_secret())
    }
}
