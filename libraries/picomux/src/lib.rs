mod frame;

use std::{
    collections::VecDeque,
    convert::Infallible,
    io::{ErrorKind, Write},
    sync::Arc,
};

use ahash::AHashMap;
use async_event::Event;
use async_task::Task;
use bytes::Bytes;
use frame::{Frame, CMD_FIN, CMD_NOP, CMD_PSH, CMD_SYN};
use futures_util::{future::Shared, io::BufReader, AsyncRead, AsyncWrite, FutureExt};
use parking_lot::Mutex;
use tachyonix::{Receiver, Sender};

pub struct PicoMux {
    task: Shared<Task<Arc<std::io::Result<Infallible>>>>,
    recv_incoming: Receiver<Stream>,
}

impl PicoMux {
    /// Creates a new picomux wrapping the given underlying connection.
    pub fn new(inner: impl AsyncRead + AsyncWrite + 'static + Send + Sync + Clone + Unpin) -> Self {
        let (send_incoming, recv_incoming) = tachyonix::channel(10000);
        let task =
            smolscale::spawn(picomux_inner(inner, send_incoming).map(|s| Arc::new(s))).shared();
        Self {
            task,
            recv_incoming,
        }
    }
}

async fn picomux_inner(
    inner: impl AsyncRead + AsyncWrite + 'static + Send + Sync + Clone + Unpin,
    send_incoming: Sender<Stream>,
) -> Result<Infallible, std::io::Error> {
    let mut inner_read = BufReader::with_capacity(100_000, inner.clone());

    let mut buffer_table: AHashMap<u32, Arc<Mutex<StreamBack>>> = AHashMap::new();
    let (send_write, recv_write) = tachyonix::channel(1);

    loop {
        let frame = Frame::read(&mut inner_read).await?;

        match frame.header.command {
            CMD_SYN => {
                if buffer_table.contains_key(&frame.header.stream_id) {
                    return Err(std::io::Error::new(ErrorKind::InvalidData, "duplicate SYN"));
                }
                let new_back = Arc::new(Mutex::new(StreamBack::default()));
                let stream = Stream {
                    back: new_back.clone(),
                    send_write: send_write.clone(),
                };
                if send_incoming.try_send(stream).is_err() {
                    tracing::warn!(
                        stream_id = frame.header.stream_id,
                        "dropping stream because the accept queue is full"
                    );
                    continue;
                }
                buffer_table.insert(frame.header.stream_id, new_back);
            }
            CMD_PSH => {
                let mut back = buffer_table
                    .get(&frame.header.stream_id)
                    .ok_or_else(|| {
                        std::io::Error::new(ErrorKind::InvalidData, "invalid stream id for PSH")
                    })?
                    .lock();
                back.read_buffer.write_all(&frame.body)?;
                back.notify.notify_all();
            }
            CMD_FIN => {
                let back = buffer_table
                    .remove(&frame.header.stream_id)
                    .ok_or_else(|| {
                        std::io::Error::new(ErrorKind::InvalidData, "invalid stream id for FIN")
                    })?;
                let mut back = back.lock();
                back.closed = true;
                back.notify.notify_one();
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

#[derive(Default)]
struct StreamBack {
    read_buffer: VecDeque<u8>,
    closed: bool,
    notify: Event,
}

pub struct Stream {
    back: Arc<Mutex<StreamBack>>,
    send_write: Sender<Bytes>,
}
