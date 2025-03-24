mod bdp;
mod buffer_table;
mod frame;
mod outgoing;

use std::{
    convert::Infallible,
    fmt::Debug,
    io::ErrorKind,
    ops::Deref,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::Poll,
    time::{Duration, Instant},
};

use anyhow::Context;

use async_task::Task;

use atomic_float::AtomicF64;
use bdp::BwEstimate;
use buffer_table::BufferTable;
use bytes::Bytes;
use frame::{Frame, CMD_FIN, CMD_MORE, CMD_NOP, CMD_PING, CMD_PONG, CMD_PSH, CMD_SYN};
use futures_lite::{Future, FutureExt as LiteExt};
use futures_util::{
    future::Shared, io::BufReader, AsyncRead, AsyncWrite, AsyncWriteExt, FutureExt,
};

use async_io::Timer;
use outgoing::Outgoing;
use parking_lot::Mutex;
use pin_project::pin_project;
use rand::Rng;
use smol_timeout2::TimeoutExt;
use smolscale::reaper::TaskReaper;
use tachyonix::{Receiver, Sender};
use tap::Tap;

use crate::frame::{Header, PingInfo};

const INIT_WINDOW: usize = 10;
const MAX_WINDOW: usize = 1500;
const MSS: usize = 8192;

#[derive(Clone, Copy, Debug)]
pub struct LivenessConfig {
    pub ping_interval: Duration,
    pub timeout: Duration,
}

impl Default for LivenessConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(1800),
            timeout: Duration::from_secs(30),
        }
    }
}

pub struct PicoMux {
    task: Shared<Task<Arc<std::io::Result<Infallible>>>>,
    send_open_req: Sender<(Bytes, oneshot::Sender<Stream>)>,

    recv_accepted: async_channel::Receiver<Stream>,
    send_liveness: async_channel::Sender<LivenessConfig>,
    liveness: LivenessConfig,

    last_ping: Arc<Mutex<Option<Duration>>>,
}

impl PicoMux {
    /// Creates a new picomux wrapping the given underlying connection.
    pub fn new(
        read: impl AsyncRead + 'static + Send + Unpin,
        write: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let (send_open_req, recv_open_req) = tachyonix::channel(1);
        let (send_accepted, recv_accepted) = async_channel::bounded(100);
        let (send_liveness, recv_liveness) = async_channel::unbounded();
        let liveness = LivenessConfig::default();
        send_liveness.try_send(liveness).unwrap();
        let last_ping = Arc::new(Mutex::new(None));
        let task = smolscale::spawn(
            picomux_inner(
                read,
                write,
                send_accepted,
                recv_open_req,
                recv_liveness,
                last_ping.clone(),
            )
            .map(Arc::new),
        )
        .shared();
        Self {
            task,
            recv_accepted,
            send_open_req,

            send_liveness,
            liveness,

            last_ping,
        }
    }

    /// Returns whether the mux is alive.
    pub fn is_alive(&self) -> bool {
        self.task.peek().is_none()
    }

    /// Waits for the whole mux to die of some error.
    pub async fn wait_until_dead(&self) -> anyhow::Result<()> {
        self.wait_error().await?
    }

    /// Sets the liveness maintenance configuration for this session.
    pub fn set_liveness(&mut self, liveness: LivenessConfig) {
        self.liveness = liveness;
        let _ = self.send_liveness.try_send(liveness);
    }

    /// Accepts a new stream from the peer.
    pub async fn accept(&self) -> std::io::Result<Stream> {
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

    /// Reads the latency from the last successful ping.
    pub fn last_latency(&self) -> Option<Duration> {
        *self.last_ping.lock()
    }

    /// Opens a new stream to the peer, putting the given metadata in the stream.
    pub async fn open(&self, metadata: &[u8]) -> std::io::Result<Stream> {
        {
            tracing::debug!("forcing a ping based on open");
            let _ = self.send_liveness.try_send(self.liveness);
        }
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
                Err(err) => Err(std::io::Error::new(err.kind(), err.to_string())),
                _ => unreachable!(),
            }
        }
    }
}

static MUX_ID_CTR: AtomicU64 = AtomicU64::new(0);

#[tracing::instrument(skip_all, fields(mux_id=MUX_ID_CTR.fetch_add(1, Ordering::Relaxed)))]
async fn picomux_inner(
    read: impl AsyncRead + 'static + Send + Unpin,
    write: impl AsyncWrite + Send + Unpin + 'static,
    send_accepted: async_channel::Sender<Stream>,
    mut recv_open_req: Receiver<(Bytes, oneshot::Sender<Stream>)>,
    recv_liveness: async_channel::Receiver<LivenessConfig>,
    last_ping: Arc<Mutex<Option<Duration>>>,
) -> Result<Infallible, std::io::Error> {
    let reaper = TaskReaper::new();
    let mut inner_read = BufReader::with_capacity(MSS * 4, read);

    let outgoing = Outgoing::new(write);
    let (send_pong, recv_pong) = async_channel::unbounded();
    let buffer_table = BufferTable::new();

    let last_bw_estimate = Arc::new(AtomicF64::new(1_000_000.0));

    let create_stream = |stream_id, metadata: Bytes| {
        let mut buffer_recv = buffer_table.create_entry(stream_id);
        let (mut write_incoming, read_incoming) = bipe::bipe(MSS * 2);
        let (write_outgoing, mut read_outgoing) = bipe::bipe(MSS * 2);
        let stream = Stream {
            write_outgoing,
            read_incoming,
            metadata,
            on_write: Box::new(|_| {}),
            on_read: Box::new(|_| {}),
        };

        // jelly bean movers
        let outgoing_task = {
            let outgoing = outgoing.clone();
            let last_bw_estimate = last_bw_estimate.clone();
            async move {
                let mut remote_window = INIT_WINDOW;
                let mut target_remote_window = MAX_WINDOW;

                let mut bw_estimate = BwEstimate::new(last_bw_estimate.load(Ordering::Relaxed));
                loop {
                    let min_quantum = (target_remote_window / 10).clamp(1, 500);
                    let frame = buffer_recv.recv().await;
                    if frame.header.command == CMD_FIN {
                        anyhow::bail!("received remote FIN");
                    }
                    let queue_delay = buffer_recv.queue_delay().unwrap();
                    tracing::trace!(
                        stream_id,
                        queue_delay = debug(queue_delay),
                        remote_window,
                        target_remote_window,
                        "queue delay measured"
                    );
                    bw_estimate.sample(frame.body.len());
                    write_incoming
                        .write_all(&frame.body)
                        .await
                        .context("could not write to incoming")?;
                    remote_window -= 1;

                    // assume the delay is 500ms
                    let estimate = bw_estimate.read();
                    last_bw_estimate.store(estimate, Ordering::Relaxed);
                    target_remote_window =
                        ((estimate / MSS as f64 / 2.0) as usize).clamp(INIT_WINDOW, MAX_WINDOW);
                    tracing::debug!(
                        target_remote_window,
                        "setting target remote send window based on bw"
                    );

                    if remote_window + min_quantum <= target_remote_window {
                        let quantum = target_remote_window - remote_window;
                        outgoing.enqueue(Frame::new(
                            stream_id,
                            CMD_MORE,
                            &(quantum as u16).to_le_bytes(),
                        ));
                        tracing::debug!(
                            stream_id,
                            remote_window,
                            target_remote_window,
                            quantum,
                            queue_delay = debug(queue_delay),
                            "sending MORE"
                        );
                        remote_window += quantum;
                    }
                }
            }
        };

        let incoming_task = {
            let buffer_table = buffer_table.clone();
            let outgoing = outgoing.clone();
            async move {
                loop {
                    let body = async_io_bufpool::pooled_read(&mut read_outgoing, 8192)
                        .await
                        .context("could not read_outgoing")?
                        .context("EOF on read_outgoing")?;

                    tracing::trace!(
                        stream_id,
                        n = body.len(),
                        "sending outgoing data into channel"
                    );
                    let frame = Frame {
                        header: Header {
                            version: 1,
                            command: CMD_PSH,
                            body_len: body.len() as _,
                            stream_id,
                        },
                        body,
                    };
                    buffer_table.wait_send_window(stream_id).await;
                    outgoing.send(frame).await?;
                }
            }
        };

        {
            let outgoing = outgoing.clone();
            reaper.attach(smolscale::spawn(async move {
                scopeguard::defer!({
                    tracing::debug!(stream_id, "enqueuing FIN to the other side");
                    outgoing.enqueue(Frame {
                        header: Header {
                            version: 1,
                            command: CMD_FIN,
                            body_len: 0,
                            stream_id,
                        },
                        body: Bytes::new(),
                    });
                });
                let _: anyhow::Result<()> =
                    incoming_task.race(outgoing_task).await.inspect_err(|e| {
                        tracing::debug!(
                            e = debug(e),
                            "incoming/outgoing task for individual stream stopped"
                        )
                    });
            }));
        }
        stream
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
                    .find(|key| !buffer_table.contains_id(*key))
                    .unwrap()
            };
            outgoing.enqueue(Frame::new_empty(stream_id, CMD_SYN).tap_mut(|f| {
                f.body = metadata.clone();
                f.header.body_len = metadata.len() as _;
            }));
            let stream = create_stream(stream_id, metadata);

            let _ = request.send(stream);
        }
    };

    // process pings
    let ping_loop = async {
        let mut lc: Option<LivenessConfig> = None;
        loop {
            if let Ok(info) = async {
                if let Some(lc) = lc {
                    Timer::after(lc.ping_interval).await;
                    Ok(lc)
                } else {
                    futures_util::future::pending().await
                }
            }
            .or(recv_liveness.recv())
            .await
            {
                lc = Some(info);
                let ping_body = serde_json::to_vec(&PingInfo {
                    next_ping_in_ms: info.ping_interval.as_millis() as _,
                })
                .unwrap();
                outgoing.enqueue(Frame {
                    header: Header {
                        version: 1,
                        command: CMD_PING,
                        body_len: ping_body.len() as _,
                        stream_id: 0,
                    },
                    body: ping_body.into(),
                });
                let start = Instant::now();
                if recv_pong.recv().timeout(info.timeout).await.is_none() {
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        "ping-pong timed out",
                    ));
                }
                tracing::info!(latency = debug(start.elapsed()), "PONG received");
                last_ping.lock().replace(start.elapsed());
            } else {
                return futures_util::future::pending().await;
            }
        }
    };

    open_req_loop
        .race(ping_loop)
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
                        if buffer_table.contains_id(stream_id) {
                            return Err(std::io::Error::new(
                                ErrorKind::InvalidData,
                                "duplicate SYN",
                            ));
                        }
                        let stream = create_stream(stream_id, frame.body.clone());
                        if let Err(err) = send_accepted.try_send(stream) {
                            match err {
                                async_channel::TrySendError::Full(_) => {
                                    tracing::warn!("receive queue is empty, ignoring SYN");
                                }
                                async_channel::TrySendError::Closed(_) => {
                                    return Err(std::io::Error::new(
                                        ErrorKind::NotConnected,
                                        "dead",
                                    ))
                                }
                            }
                        }
                    }
                    CMD_MORE => {
                        let window_increase = u16::from_le_bytes(
                            (&frame.body[..]).try_into().ok().ok_or_else(|| {
                                std::io::Error::new(
                                    ErrorKind::InvalidData,
                                    "corrupt window increase message",
                                )
                            })?,
                        );
                        buffer_table.incr_send_window(stream_id, window_increase);
                    }
                    CMD_PSH | CMD_FIN => {
                        if frame.header.command == CMD_FIN {
                            tracing::debug!(stream_id, "FIN received");
                        }
                        buffer_table.send_to(stream_id, frame);
                    }

                    CMD_NOP => {}
                    CMD_PING => {
                        let ping_info: PingInfo =
                            serde_json::from_slice(&frame.body).map_err(|e| {
                                std::io::Error::new(
                                    ErrorKind::InvalidData,
                                    format!("invalid PING data {e}"),
                                )
                            })?;
                        tracing::debug!(
                            next_ping_in_ms = ping_info.next_ping_in_ms,
                            "responding to a PING"
                        );

                        outgoing.enqueue(Frame::new_empty(0, CMD_PONG))
                    }
                    CMD_PONG => {
                        let _ = send_pong.send(()).await;
                    }
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

#[pin_project]
pub struct Stream {
    #[pin]
    read_incoming: bipe::BipeReader,
    #[pin]
    write_outgoing: bipe::BipeWriter,
    metadata: Bytes,
    on_write: Box<dyn Fn(usize) + Send + Sync + 'static>,
    on_read: Box<dyn Fn(usize) + Send + Sync + 'static>,
}

impl Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        "stream".fmt(f)
    }
}

impl Stream {
    pub fn metadata(&self) -> &[u8] {
        &self.metadata
    }

    pub fn set_on_write(&mut self, on_write: impl Fn(usize) + Send + Sync + 'static) {
        self.on_write = Box::new(on_write);
    }

    pub fn set_on_read(&mut self, on_read: impl Fn(usize) + Send + Sync + 'static) {
        self.on_read = Box::new(on_read);
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
            let this = self.project();
            let r = this.read_incoming.poll_read(cx, buf);
            if r.is_ready() {
                (this.on_read)(buf.len());
            }
            r
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
        // if fastrand::f32() < 0.1 {
        //     cx.waker().wake_by_ref();
        //     Poll::Pending
        // } else {
        let this = self.project();
        let r = this.write_outgoing.poll_write(cx, buf);
        if r.is_ready() {
            (this.on_write)(buf.len());
        }
        r
        // }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().write_outgoing.poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().write_outgoing.poll_close(cx)
    }
}

impl sillad::Pipe for Stream {
    fn protocol(&self) -> &str {
        "sillad-stream"
    }

    fn remote_addr(&self) -> Option<&str> {
        None
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
            let (picomux_a, picomux_b) = setup_picomux_pair().await;

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
