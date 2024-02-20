use std::{
    collections::VecDeque,
    io::{Read, Write},
    pin::Pin,
    task::Poll,
};

use futures_util::{AsyncRead, AsyncWrite, Future};

#[derive(Clone)]
pub struct AsyncReadAdapter<
    B: AsRef<[u8]>,
    Fut: Future<Output = std::io::Result<B>>,
    Fun: FnMut() -> Fut,
> {
    read_fut_gen: Fun,
    buffer: VecDeque<u8>,
    last_in_prog: Pin<Box<Option<Fut>>>,
}

impl<B: AsRef<[u8]>, Fut: Future<Output = std::io::Result<B>>, Fun: FnMut() -> Fut>
    AsyncReadAdapter<B, Fut, Fun>
{
    /// Creates a new async read adapter, given a function that yields read bytes.
    pub fn new(read_fut_gen: Fun) -> Self {
        Self {
            read_fut_gen,
            buffer: VecDeque::new(),
            last_in_prog: Box::pin(None),
        }
    }
}

impl<B: AsRef<[u8]>, Fut: Future<Output = std::io::Result<B>>, Fun: FnMut() -> Fut> AsyncRead
    for AsyncReadAdapter<B, Fut, Fun>
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            if !this.buffer.is_empty() {
                return Poll::Ready(this.buffer.read(buf));
            }
            let last_in_prog = if this.last_in_prog.is_none() {
                None
            } else {
                Some(unsafe {
                    this.last_in_prog
                        .as_mut()
                        .map_unchecked_mut(|inner| inner.as_mut().unwrap())
                })
            };
            match last_in_prog {
                Some(inner) => match inner.poll(cx) {
                    Poll::Ready(rv) => {
                        // reset the state
                        unsafe {
                            let _ = this.last_in_prog.as_mut().map_unchecked_mut(|inner| {
                                *inner = None;
                                inner
                            });
                        };
                        match rv {
                            Ok(bts) => {
                                let chunk_len = bts.as_ref().len();
                                if chunk_len <= buf.len() {
                                    buf[..chunk_len].copy_from_slice(bts.as_ref());
                                    return Poll::Ready(Ok(bts.as_ref().len()));
                                } else {
                                    buf.copy_from_slice(&bts.as_ref()[..buf.len()]);
                                    this.buffer.write_all(&bts.as_ref()[buf.len()..]).unwrap();
                                    return Poll::Ready(Ok(buf.len()));
                                }
                            }
                            Err(err) => {
                                return Poll::Ready(Err(err));
                            }
                        }
                    }
                    Poll::Pending => return Poll::Pending,
                },
                None => {
                    let f = (this.read_fut_gen)();
                    unsafe {
                        let _ = this.last_in_prog.as_mut().map_unchecked_mut(|inner| {
                            *inner = Some(f);
                            inner
                        });
                    };
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct AsyncWriteAdapter<
    WrFut: Future<Output = std::io::Result<usize>>,
    ClFut: Future<Output = std::io::Result<()>>,
    Fun: FnMut(&[u8]) -> WrFut,
> {
    write_fut_gen: Fun,
    write_in_prog: Pin<Box<Option<WrFut>>>,
    close_fut: Pin<Box<ClFut>>,
}

impl<
        WrFut: Future<Output = std::io::Result<usize>>,
        ClFut: Future<Output = std::io::Result<()>>,
        Fun: FnMut(&[u8]) -> WrFut,
    > AsyncWriteAdapter<WrFut, ClFut, Fun>
{
    /// Creates a new write adapter, by combining a function that is called to write out bytes with a future that is polled to close the whole thing.
    pub fn new(write_fut_gen: Fun, close_fut: ClFut) -> Self {
        Self {
            write_fut_gen,
            write_in_prog: Box::pin(None),
            close_fut: Box::pin(close_fut),
        }
    }
}

impl<
        WrFut: Future<Output = std::io::Result<usize>>,
        ClFut: Future<Output = std::io::Result<()>>,
        Fun: FnMut(&[u8]) -> WrFut,
    > AsyncWrite for AsyncWriteAdapter<WrFut, ClFut, Fun>
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            if let Some(fut) = this.write_in_prog.as_mut().as_pin_mut() {
                // Try to complete the in-progress future
                match fut.poll(cx) {
                    Poll::Ready(Ok(size)) => {
                        // Future completed, clear it
                        this.write_in_prog.set(None);
                        return Poll::Ready(Ok(size));
                    }
                    Poll::Ready(Err(e)) => {
                        // Future completed with error, clear it
                        this.write_in_prog.set(None);
                        return Poll::Ready(Err(e));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            } else {
                // No in-progress future, create a new one
                let fut = (this.write_fut_gen)(buf);
                this.write_in_prog.set(Some(fut));
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        // For simplicity, we're assuming flush is a no-op. Real implementations might need to handle this differently.
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.close_fut).poll(cx) }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn test_read() {
        let mut rd = AsyncReadAdapter::new(|| async move {
            let buf = vec![10; 100];
            Ok(buf)
        });
        let mut buf = [0u8; 1000];
        pollster::block_on(async {
            rd.read_exact(&mut buf).await.unwrap();
        });
        eprintln!("{:?}", buf);
    }

    #[test]
    fn test_async_write_adapter() {
        // Shared state between the test and the mock write future
        let written_data = Arc::new(Mutex::new(Vec::new()));

        let write_fut_gen = {
            let written_data = Arc::clone(&written_data);
            move |buf: &[u8]| {
                let len = buf.len();
                let mut data_lock = written_data.lock().unwrap();
                data_lock.extend_from_slice(buf);
                // Simulate an async write operation, returning the number of bytes "written"
                async move { Ok(len) }
            }
        };

        // Simulate a close future that immediately resolves to Ok(())
        let close_fut = futures_util::future::ready(Ok(()));

        let mut writer = AsyncWriteAdapter::new(write_fut_gen, close_fut);

        let buf = vec![1u8; 100]; // Buffer of 100 bytes to write

        pollster::block_on(async {
            // Write all bytes to the adapter
            let write_size = writer.write(&buf).await.unwrap();
            assert_eq!(write_size, buf.len()); // Ensure the write size is as expected

            // Flush the writer (no-op in this case)
            writer.flush().await.unwrap();

            // Close the writer
            writer.close().await.unwrap();
        });

        // Check the shared state to ensure the data was "written" correctly
        let data = written_data.lock().unwrap();
        assert_eq!(*data, buf);
    }
}
