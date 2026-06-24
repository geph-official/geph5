//! Buffer-pooled reads: read into a shared thread-local buffer so no per-read
//! allocation happens until the read actually unblocks. A tokio-io port of the
//! `async-io-bufpool` crate (which was built on futures-io traits).

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, ReadBuf};

thread_local! {
    static BUFFER: RefCell<[u8; 65536]> = const { RefCell::new([0u8; 65536]) };
}

/// Reads from `rdr` into a thread-local buffer, returning the bytes read.
///
/// `Ok(None)` indicates EOF.
pub async fn pooled_read(rdr: impl AsyncRead, limit: usize) -> std::io::Result<Option<Bytes>> {
    pooled_read_callback(rdr, limit, |b: &[u8]| Bytes::copy_from_slice(b)).await
}

/// Like [`pooled_read`], but instead of allocating, invokes `resolve` on the
/// freshly read bytes (still backed by the thread-local buffer).
///
/// `Ok(None)` indicates EOF.
pub async fn pooled_read_callback<T>(
    rdr: impl AsyncRead,
    limit: usize,
    resolve: impl FnMut(&[u8]) -> T,
) -> std::io::Result<Option<T>> {
    PooledOnceReader {
        rdr,
        resolve,
        limit,
    }
    .await
}

pin_project! {
    struct PooledOnceReader<T, F> {
        #[pin]
        rdr: T,
        resolve: F,
        limit: usize,
    }
}

impl<T: AsyncRead, U, F: FnMut(&[u8]) -> U> Future for PooledOnceReader<T, F> {
    type Output = std::io::Result<Option<U>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        BUFFER.with(|buf| {
            let mut buf = buf.borrow_mut();
            let this = self.project();
            let limit = (*this.limit).min(buf.len());
            let mut read_buf = ReadBuf::new(&mut buf[..limit]);
            match this.rdr.poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => {
                    let filled = read_buf.filled();
                    if filled.is_empty() {
                        Poll::Ready(Ok(None))
                    } else {
                        Poll::Ready(Ok(Some((this.resolve)(filled))))
                    }
                }
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => Poll::Pending,
            }
        })
    }
}
