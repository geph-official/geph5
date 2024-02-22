use std::{
    collections::VecDeque,
    fmt::Debug,
    io::{ErrorKind, Read, Write},
    task::Poll,
};

use futures_util::{AsyncRead, AsyncWrite};
use pin_project::pin_project;
use serde::{Deserialize, Serialize};
use sillad::Pipe;
use state::State;

pub mod dialer;
mod handshake;
pub mod listener;
mod state;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Cookie([u8; 32]);

impl Debug for Cookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        hex::encode(self.0).fmt(f)
    }
}

impl Cookie {
    /// Derives a cookie from a string.
    pub fn new(s: &str) -> Self {
        let derived_cookie = blake3::derive_key("cookie", s.as_bytes());
        Self(derived_cookie)
    }

    /// Derives a key given the direction.
    pub fn derive_key(&self, is_server: bool) -> [u8; 32] {
        blake3::derive_key(if is_server { "server" } else { "client" }, &self.0)
    }
}

/// An established sosistab3 connection.
#[pin_project]
pub struct SosistabPipe<P: Pipe> {
    #[pin]
    lower: P,
    state: State,

    read_buf: VecDeque<u8>,
    raw_read_buf: Vec<u8>,

    to_write_buf: Vec<u8>,
}

impl<P: Pipe> SosistabPipe<P> {
    fn new(lower: P, state: State) -> Self {
        Self {
            lower,
            state,
            read_buf: Default::default(),
            raw_read_buf: Default::default(),
            to_write_buf: Default::default(),
        }
    }
}

impl<P: Pipe> AsyncWrite for SosistabPipe<P> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut this = self.project();
        if this.to_write_buf.is_empty() {
            this.state.encrypt(buf, this.to_write_buf);
        }
        loop {
            let res = futures_util::ready!(this.lower.as_mut().poll_write(cx, this.to_write_buf));
            match res {
                Ok(n) => {
                    this.to_write_buf.drain(..n);
                    if this.to_write_buf.is_empty() {
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
        self.project().lower.poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.project().lower.poll_close(cx)
    }
}

impl<P: Pipe> AsyncRead for SosistabPipe<P> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut this = self.project();
        let mut frag_buf = [0u8; 8192];
        loop {
            if !this.read_buf.is_empty() {
                return Poll::Ready(this.read_buf.read(buf));
            } else {
                let n = futures_util::ready!(this.lower.as_mut().poll_read(cx, &mut frag_buf));
                match n {
                    Err(e) => return Poll::Ready(Err(e)),
                    Ok(n) => {
                        this.raw_read_buf.write_all(&frag_buf[..n]).unwrap();
                        // attempt to decrypt in order to fill the read_buf
                        match this.state.decrypt(this.raw_read_buf, &mut this.read_buf) {
                            Ok(result) => {
                                this.raw_read_buf.drain(..result);
                            }
                            Err(err) => {
                                if err.kind() == ErrorKind::BrokenPipe {
                                    return Poll::Ready(Err(err));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

impl<P: Pipe> Pipe for SosistabPipe<P> {}
