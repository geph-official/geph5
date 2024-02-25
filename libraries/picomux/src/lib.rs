mod frame;

use std::{
    convert::Infallible, hash::BuildHasherDefault, io::ErrorKind, ops::Deref, pin::Pin, sync::Arc,
    task::Poll, time::Duration,
};

use ahash::AHasher;
use anyhow::Context;

use async_task::Task;

use bytes::Bytes;
use dashmap::DashMap;
use frame::{Frame, CMD_FIN, CMD_NOP, CMD_PSH, CMD_SYN};
use futures_lite::{Future, FutureExt as LiteExt};
use futures_util::{
    future::Shared, io::BufReader, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, FutureExt,
};

use rand::Rng;
use smol_timeout::TimeoutExt;
use tachyonix::{Receiver, Sender};
use tap::Tap;

use crate::frame::Header;

pub struct PicoMux {
    task: Shared<Task<Arc<std::io::Result<Infallible>>>>,
    send_open_req: Sender<(Bytes, oneshot::Sender<Stream>)>,
    recv_accepted: Receiver<Stream>,
}

impl PicoMux {
    /// Creates a new picomux wrapping the given underlying connection.
    pub fn new(
        read: impl AsyncRead + 'static + Send + Unpin,
        write: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let (send_open_req, recv_open_req) = tachyonix::channel(1);
        let (send_accepted, recv_accepted) = tachyonix::channel(10000);
        let task = smolscale::spawn(
            picomux_inner(read, write, send_accepted, recv_open_req).map(Arc::new),
        )
        .shared();
        Self {
            task,
            recv_accepted,
            send_open_req,
        }
    }

    /// Accepts a new stream from the peer.
    pub async fn accept(&mut self) -> std::io::Result<Stream> {
        let err = self.wait_error();
        async {
            if let Ok(val) = self.recv_accepted.recv().await {
                Ok(val)
            } else {
                futures_util::future::pending().await
            }
        }
        .race(err)
        .await
    }

    /// Opens a new stream to the peer, putting the given metadata in the stream.
    pub async fn open(&self, metadata: &[u8]) -> std::io::Result<Stream> {
        let (send, recv) = oneshot::channel();
        let _ = self
            .send_open_req
            .send((Bytes::copy_from_slice(metadata), send))
            .await;
        async {
            if let Ok(val) = recv.await {
                Ok(val)
            } else {
                futures_util::future::pending().await
            }
        }
        .race(self.wait_error())
        .await
    }

    fn wait_error<T>(&self) -> impl Future<Output = std::io::Result<T>> + 'static {
        let res = self.task.clone();
        async move {
            let res = res.await;
            match res.deref() {
                Err(err) => Err(std::io::Error::new(
                    err.kind(),
                    err.get_ref().map(|e| e.to_string()).unwrap_or_default(),
                )),
                _ => unreachable!(),
            }
        }
    }
}

#[tracing::instrument(skip(read, write, send_accepted, recv_open_req))]
async fn picomux_inner(
    read: impl AsyncRead + 'static + Send + Unpin,
    mut write: impl AsyncWrite + Send + Unpin + 'static,
    send_accepted: Sender<Stream>,
    mut recv_open_req: Receiver<(Bytes, oneshot::Sender<Stream>)>,
) -> Result<Infallible, std::io::Error> {
    let mut inner_read = BufReader::with_capacity(100_000, read);

    let (send_outgoing, mut recv_outgoing) = tachyonix::channel(1);
    let buffer_table: DashMap<_, _, BuildHasherDefault<AHasher>> = DashMap::default();

    // writes outgoing frames
    let outgoing_loop = async {
        loop {
            let outgoing: Frame = recv_outgoing
                .recv()
                .await
                .expect("send_outgoing should never be dropped here");
            tracing::trace!(
                stream_id = outgoing.header.stream_id,
                command = outgoing.header.command,
                body_len = outgoing.body.len(),
                "sending outgoing data into transport"
            );
            if outgoing.header.command == CMD_FIN {
                tracing::debug!(
                    stream_id = outgoing.header.stream_id,
                    "removing on outgoing FIN"
                );
                if buffer_table.remove(&outgoing.header.stream_id).is_some() {
                    write.write_all(&outgoing.bytes()).await?;
                }
            }
            write.write_all(&outgoing.bytes()).await?;
        }
    };

    let create_stream = |stream_id, metadata: Bytes| {
        let (send_incoming, mut recv_incoming) = tachyonix::channel(100);
        let (mut write_incoming, read_incoming) = bipe::bipe(32768);
        let (write_outgoing, mut read_outgoing) = bipe::bipe(32768);
        let stream = Stream {
            write_outgoing,
            read_incoming,
            metadata,
        };
        // jelly bean movers
        smolscale::spawn::<anyhow::Result<()>>(async move {
            loop {
                let frame: Frame = recv_incoming.recv().await?;
                write_incoming.write_all(&frame.body).await?;
            }
        })
        .detach();
        let send_outgoing = send_outgoing.clone();
        smolscale::spawn::<anyhow::Result<()>>(async move {
            scopeguard::defer!({
                let send_outgoing = send_outgoing.clone();
                smolscale::spawn(async move {
                    send_outgoing
                        .send(Frame {
                            header: Header {
                                version: 1,
                                command: CMD_FIN,
                                body_len: 0,
                                stream_id,
                            },
                            body: Bytes::new(),
                        })
                        .await
                })
                .detach();
            });
            let mut buf = [0u8; 16384];
            loop {
                let n = read_outgoing.read(&mut buf).await?;
                if n == 0 {
                    return Ok(());
                }
                let frame = Frame {
                    header: Header {
                        version: 1,
                        command: CMD_PSH,
                        body_len: n as _,
                        stream_id,
                    },
                    body: Bytes::copy_from_slice(&buf[..n]),
                };
                tracing::trace!(stream_id, n, "sending outgoing data into channel");
                send_outgoing
                    .send(frame)
                    .await
                    .ok()
                    .context("cannot send")?;
            }
        })
        .detach();
        (stream, send_incoming)
    };

    // receive open requests
    let open_req_loop = async {
        loop {
            let (metadata, request) = recv_open_req.recv().await.map_err(|_e| {
                std::io::Error::new(ErrorKind::BrokenPipe, "open request channel died")
            })?;
            let stream_id = {
                let mut rng = rand::thread_rng();
                std::iter::repeat_with(|| rng.gen())
                    .find(|key| !buffer_table.contains_key(key))
                    .unwrap()
            };
            let _ = send_outgoing
                .send(Frame::new_empty(stream_id, CMD_SYN).tap_mut(|f| {
                    f.body = metadata.clone();
                    f.header.body_len = metadata.len() as _;
                }))
                .await;
            let (stream, send_incoming) = create_stream(stream_id, metadata);
            // thread safety: there can be no race because we are racing the futures in the foreground and there's no await point between when we obtain the id and when we insert
            assert!(buffer_table.insert(stream_id, send_incoming).is_none());
            let _ = request.send(stream);
        }
    };

    outgoing_loop
        .race(open_req_loop)
        .race(async {
            loop {
                let frame = Frame::read(&mut inner_read).await?;
                let stream_id = frame.header.stream_id;
                tracing::trace!(
                    command = frame.header.command,
                    stream_id,
                    body_len = frame.header.body_len,
                    "got incoming frame"
                );
                match frame.header.command {
                    CMD_SYN => {
                        if buffer_table.contains_key(&stream_id) {
                            return Err(std::io::Error::new(
                                ErrorKind::InvalidData,
                                "duplicate SYN",
                            ));
                        }
                        let (stream, send_incoming) = create_stream(stream_id, frame.body.clone());
                        if send_accepted.try_send(stream).is_err() {
                            tracing::warn!(
                                stream_id = frame.header.stream_id,
                                "dropping stream because the accept queue is full"
                            );
                            continue;
                        }
                        buffer_table.insert(frame.header.stream_id, send_incoming);
                    }
                    CMD_PSH => {
                        let back = buffer_table.get(&stream_id);
                        if let Some(back) = back {
                            if back
                                .send(frame.clone())
                                .timeout(Duration::from_millis(200))
                                .await
                                .is_none()
                            {
                                tracing::warn!(
                                    stream_id = frame.header.stream_id,
                                    "dropping stream because the read queue is full"
                                );
                                buffer_table.remove(&stream_id);
                            }
                        } else {
                            tracing::warn!(
                                stream_id = frame.header.stream_id,
                                "PSH to a stream that is no longer here"
                            );
                        }
                    }
                    CMD_FIN => {
                        buffer_table.remove(&frame.header.stream_id);
                    }
                    CMD_NOP => {}
                    other => {
                        return Err(std::io::Error::new(
                            ErrorKind::InvalidData,
                            format!("invalid command {other}"),
                        ));
                    }
                }
            }
        })
        .await
}

pub struct Stream {
    read_incoming: bipe::BipeReader,
    write_outgoing: bipe::BipeWriter,
    metadata: Bytes,
}

impl Stream {
    pub fn metadata(&self) -> &[u8] {
        &self.metadata
    }

    fn pin_project_read(self: std::pin::Pin<&mut Self>) -> Pin<&mut bipe::BipeReader> {
        // SAFETY: this is a safe pin-projection, since we never get a &mut sosistab2::Stream from a Pin<&mut Stream> elsewhere.
        // Safety requires that we either consistently lose Pin or keep it.
        // We could use the "pin_project" crate but I'm too lazy.
        unsafe { self.map_unchecked_mut(|s| &mut s.read_incoming) }
    }
    fn pin_project_write(self: std::pin::Pin<&mut Self>) -> Pin<&mut bipe::BipeWriter> {
        unsafe { self.map_unchecked_mut(|s| &mut s.write_outgoing) }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if fastrand::f32() < 0.1 {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            self.pin_project_read().poll_read(cx, buf)
        }
    }
}

impl AsyncWrite for Stream {
    #[tracing::instrument(name = "picomux_stream_write", skip(self, cx, buf))]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        tracing::trace!(buf_len = buf.len(), "about to poll write");
        if fastrand::f32() < 0.1 {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            self.pin_project_write().poll_write(cx, buf)
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.pin_project_write().poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.pin_project_write().poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::{AsyncReadExt, AsyncWriteExt};
    use tracing_test::traced_test;

    async fn setup_picomux_pair() -> (PicoMux, PicoMux) {
        let (a_write, b_read) = bipe::bipe(1);
        let (b_write, a_read) = bipe::bipe(1);

        let picomux_a = PicoMux::new(a_read, a_write);
        let picomux_b = PicoMux::new(b_read, b_write);

        (picomux_a, picomux_b)
    }

    #[traced_test]
    #[test]
    fn test_picomux_basic() {
        smolscale::block_on(async move {
            let (picomux_a, mut picomux_b) = setup_picomux_pair().await;

            let a_proc = async move {
                let mut stream_a = picomux_a.open(b"").await.unwrap();
                stream_a.write_all(b"Hello, world!").await.unwrap();
                stream_a.flush().await.unwrap();
                drop(stream_a);
                futures_util::future::pending().await
            };
            let b_proc = async move {
                let mut stream_b = picomux_b.accept().await.unwrap();

                let mut buf = vec![0u8; 13];
                stream_b.read_exact(&mut buf).await.unwrap();

                assert_eq!(buf, b"Hello, world!");
            };
            a_proc.race(b_proc).await
        })
    }
}
