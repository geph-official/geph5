use std::pin::{pin, Pin};

use futures_util::{AsyncRead, AsyncWrite};
use pin_project::pin_project;

pub mod dialer;
pub mod listener;
pub mod tcp;

/// Sillad overall is based on returning connection-like items that implement AsyncRead and AsyncWrite, as well as a few other things. This is called a Pipe.
pub trait Pipe: AsyncRead + AsyncWrite + Send + Unpin + 'static {
    /// If this pipe is end-to-end encrypted, returns a shared secret that appears the same on both ends iff the encryption is secure.
    fn shared_secret(&self) -> Option<&[u8]> {
        None
    }
}

/// EitherPipe is a pipe that is either left or right.
#[pin_project(project = EitherPipeProj)]
pub enum EitherPipe<L: Pipe, R: Pipe> {
    Left(#[pin] L),
    Right(#[pin] R),
}

impl<L: Pipe, R: Pipe> AsyncRead for EitherPipe<L, R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.project() {
            EitherPipeProj::Left(l) => l.poll_read(cx, buf),
            EitherPipeProj::Right(r) => r.poll_read(cx, buf),
        }
    }
}

impl<L: Pipe, R: Pipe> AsyncWrite for EitherPipe<L, R> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.project() {
            EitherPipeProj::Left(l) => l.poll_write(cx, buf),
            EitherPipeProj::Right(r) => r.poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.project() {
            EitherPipeProj::Left(l) => l.poll_flush(cx),
            EitherPipeProj::Right(r) => r.poll_flush(cx),
        }
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.project() {
            EitherPipeProj::Left(l) => l.poll_close(cx),
            EitherPipeProj::Right(r) => r.poll_close(cx),
        }
    }
}

impl<L: Pipe, R: Pipe> Pipe for EitherPipe<L, R> {
    fn shared_secret(&self) -> Option<&[u8]> {
        match self {
            EitherPipe::Left(l) => l.shared_secret(),
            EitherPipe::Right(r) => r.shared_secret(),
        }
    }
}
