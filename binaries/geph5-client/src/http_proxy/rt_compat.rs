use std::{
    cmp::max,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Debug)]
pub struct HyperRtCompat<T>(pub(crate) T);

impl<T> HyperRtCompat<T> {
    pub fn new(io: T) -> Self {
        HyperRtCompat(io)
    }

    fn p(self: Pin<&mut Self>) -> Pin<&mut T> {
        // SAFETY: The simplest of projections. This is just
        // a wrapper, we don't do anything that would undo the projection.
        unsafe { self.map_unchecked_mut(|me| &mut me.0) }
    }
}

impl<T> tokio::io::AsyncRead for HyperRtCompat<T>
where
    T: hyper::rt::Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        tbuf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let init = tbuf.initialized().len();
        let filled = tbuf.filled().len();
        let new_filled = unsafe {
            let mut buf = hyper::rt::ReadBuf::uninit(tbuf.unfilled_mut());

            match hyper::rt::Read::poll_read(self.p(), cx, buf.unfilled()) {
                // sad, we can't get the initialized size from hyper::rt::ReadBuf
                // so we assume that unfilled area is completely uninitialized
                Poll::Ready(Ok(())) => buf.filled().len(),
                other => return other,
            }
        };

        if filled + new_filled > init {
            let n_init = max(filled + new_filled - init, 0);
            unsafe {
                tbuf.assume_init(n_init);
            }
        }
        tbuf.set_filled(filled + new_filled);

        Poll::Ready(Ok(()))
    }
}

impl<T> tokio::io::AsyncWrite for HyperRtCompat<T>
where
    T: hyper::rt::Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        hyper::rt::Write::poll_write(self.p(), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        hyper::rt::Write::poll_flush(self.p(), cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        hyper::rt::Write::poll_shutdown(self.p(), cx)
    }

    fn is_write_vectored(&self) -> bool {
        hyper::rt::Write::is_write_vectored(&self.0)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        hyper::rt::Write::poll_write_vectored(self.p(), cx, bufs)
    }
}

impl<T> hyper::rt::Read for HyperRtCompat<T>
where
    T: tokio::io::AsyncRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let n = unsafe {
            let mut tbuf = tokio::io::ReadBuf::uninit(buf.as_mut());
            match tokio::io::AsyncRead::poll_read(self.p(), cx, &mut tbuf) {
                Poll::Ready(Ok(())) => tbuf.filled().len(),
                other => return other,
            }
        };

        unsafe {
            buf.advance(n);
        }
        Poll::Ready(Ok(()))
    }
}

impl<T> hyper::rt::Write for HyperRtCompat<T>
where
    T: tokio::io::AsyncWrite,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        tokio::io::AsyncWrite::poll_write(self.p(), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_flush(self.p(), cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_shutdown(self.p(), cx)
    }

    fn is_write_vectored(&self) -> bool {
        tokio::io::AsyncWrite::is_write_vectored(&self.0)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        tokio::io::AsyncWrite::poll_write_vectored(self.p(), cx, bufs)
    }
}

impl<T> hyper_util::client::legacy::connect::Connection for HyperRtCompat<T>
where
    T: hyper_util::client::legacy::connect::Connection,
{
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        self.0.connected()
    }
}
