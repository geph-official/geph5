//! A tokio-io adapter over a raw file descriptor (e.g. a TUN device), built on
//! `tokio::io::unix::AsyncFd`. Replaces `smol::Async<File>` for the VPN fd.

use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Wraps a non-blocking fd that is `Read + Write` (such as a TUN device file) as
/// a tokio-io stream. The fd is forced into non-blocking mode on construction.
pub struct AsyncFdStream<T: AsRawFd> {
    inner: AsyncFd<T>,
}

impl<T: AsRawFd> AsyncFdStream<T> {
    /// Wraps the given fd, setting `O_NONBLOCK` and registering with the reactor.
    pub fn new(io: T) -> io::Result<Self> {
        let fd = io.as_raw_fd();
        // AsyncFd requires the fd to be non-blocking.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            inner: AsyncFd::new(io)?,
        })
    }
}

impl<T: AsRawFd + Read + Unpin> AsyncRead for AsyncFdStream<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            let mut guard = ready!(this.inner.poll_read_ready_mut(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|inner| inner.get_mut().read(unfilled)) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(err)) => return Poll::Ready(Err(err)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl<T: AsRawFd + Write + Unpin> AsyncWrite for AsyncFdStream<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        loop {
            let mut guard = ready!(this.inner.poll_write_ready_mut(cx))?;
            match guard.try_io(|inner| inner.get_mut().write(buf)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
