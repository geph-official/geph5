mod frame;

use std::{
    collections::VecDeque,
    convert::Infallible,
    io::{ErrorKind, Write},
    pin::Pin,
    sync::Arc,
};

use ahash::AHashMap;
use anyhow::Context;
use async_event::Event;
use async_task::Task;
use bipe::BipeWriter;
use bytes::Bytes;
use frame::{Frame, CMD_FIN, CMD_NOP, CMD_PSH, CMD_SYN};
use futures_util::{
    future::Shared, io::BufReader, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Future,
    FutureExt, Sink,
};
use parking_lot::Mutex;
use recycle_box::RecycleBox;
use tachyonix::{Receiver, Sender};

use crate::frame::Header;

pub struct PicoMux {
    task: Shared<Task<Arc<std::io::Result<Infallible>>>>,
    recv_accepted: Receiver<Stream>,
}

impl PicoMux {
    /// Creates a new picomux wrapping the given underlying connection.
    pub fn new(inner: impl AsyncRead + AsyncWrite + 'static + Send + Sync + Clone + Unpin) -> Self {
        let (send_accepted, recv_accepted) = tachyonix::channel(10000);
        let task = smolscale::spawn(picomux_inner(inner, send_accepted).map(Arc::new)).shared();
        Self {
            task,
            recv_accepted,
        }
    }
}

async fn picomux_inner(
    inner: impl AsyncRead + AsyncWrite + 'static + Send + Sync + Clone + Unpin,
    send_accepted: Sender<Stream>,
) -> Result<Infallible, std::io::Error> {
    let mut inner_read = BufReader::with_capacity(100_000, inner.clone());

    let (send_outgoing, recv_outgoing) = tachyonix::channel(10000);

    let mut buffer_table: AHashMap<u32, Sender<Frame>> = AHashMap::new();

    loop {
        let frame = Frame::read(&mut inner_read).await?;
        let stream_id = frame.header.stream_id;
        match frame.header.command {
            CMD_SYN => {
                if buffer_table.contains_key(&frame.header.stream_id) {
                    return Err(std::io::Error::new(ErrorKind::InvalidData, "duplicate SYN"));
                }

                let (send_incoming, mut recv_incoming) = tachyonix::channel(10000);
                let (mut write_incoming, read_incoming) = bipe::bipe(32768);
                let (write_outgoing, mut read_outgoing) = bipe::bipe(32768);
                let stream = Stream {
                    write_outgoing: async_dup::Arc::new(async_dup::Mutex::new(write_outgoing)),
                    read_incoming: async_dup::Arc::new(async_dup::Mutex::new(read_incoming)),
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
                        let _ = send_outgoing.try_send(Frame {
                            header: Header {
                                version: 1,
                                command: CMD_FIN,
                                body_len: 0,
                                stream_id,
                            },
                            body: Bytes::new(),
                        });
                    });
                    let mut buf = [0u8; 16384];
                    loop {
                        let n = read_outgoing.read(&mut buf).await?;
                        let frame = Frame {
                            header: Header {
                                version: 1,
                                command: CMD_PSH,
                                body_len: n as _,
                                stream_id,
                            },
                            body: Bytes::copy_from_slice(&buf[..n]),
                        };
                        send_outgoing
                            .send(frame)
                            .await
                            .ok()
                            .context("cannot send")?;
                    }
                })
                .detach();
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
                let back = buffer_table.get(&frame.header.stream_id).ok_or_else(|| {
                    std::io::Error::new(ErrorKind::InvalidData, "invalid stream id for PSH")
                })?;
                if back.try_send(frame.clone()).is_err() {
                    tracing::warn!(
                        stream_id = frame.header.stream_id,
                        "dropping stream because the read queue is full"
                    );
                    let _ = send_outgoing.try_send(Frame {
                        header: Header {
                            version: 1,
                            command: CMD_FIN,
                            body_len: 0,
                            stream_id,
                        },
                        body: Bytes::new(),
                    });
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
}

#[derive(Clone)]
pub struct Stream {
    read_incoming: async_dup::Arc<async_dup::Mutex<bipe::BipeReader>>,
    write_outgoing: async_dup::Arc<async_dup::Mutex<bipe::BipeWriter>>,
}

impl Stream {
    fn pin_project_read(
        self: std::pin::Pin<&mut Self>,
    ) -> Pin<&mut async_dup::Arc<async_dup::Mutex<bipe::BipeReader>>> {
        // SAFETY: this is a safe pin-projection, since we never get a &mut sosistab2::Stream from a Pin<&mut Stream> elsewhere.
        // Safety requires that we either consistently lose Pin or keep it.
        // We could use the "pin_project" crate but I'm too lazy.
        unsafe { self.map_unchecked_mut(|s| &mut s.read_incoming) }
    }
    fn pin_project_write(
        self: std::pin::Pin<&mut Self>,
    ) -> Pin<&mut async_dup::Arc<async_dup::Mutex<bipe::BipeWriter>>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.write_outgoing) }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.pin_project_read().poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.pin_project_write().poll_write(cx, buf)
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
