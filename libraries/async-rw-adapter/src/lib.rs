use std::{
    collections::VecDeque,
    io::{Read, Write},
    pin::Pin,
    task::Poll,
};

use futures_util::{AsyncRead, Future};

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

pub struct AsyncWriteAdapter<Fut: Future<Output = std::io::Result<usize>>, Fun: FnMut(&[u8]) -> Fut>
{
    write_fut_gen: Fun,
    last_in_prog: Pin<Box<Option<Fut>>>,
}

#[cfg(test)]
mod tests {
    use futures_util::AsyncReadExt;

    use super::*;

    #[test]
    fn test() {
        let mut rd = AsyncReadAdapter::new(|| async move {
            let mut buf = vec![10; 100];
            Ok(buf)
        });
        let mut buf = [0u8; 1000];
        pollster::block_on(async {
            rd.read_exact(&mut buf).await.unwrap();
        });
        eprintln!("{:?}", buf);
    }
}
