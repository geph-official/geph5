mod frame;

use std::{
    convert::Infallible,
    fmt::Debug,
    hash::BuildHasherDefault,
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

use ahash::AHasher;
use anyhow::Context;

use async_task::Task;

use bytes::Bytes;
use dashmap::DashMap;
use frame::{Frame, CMD_FIN, CMD_MORE, CMD_NOP, CMD_PING, CMD_PONG, CMD_PSH, CMD_SYN};
use futures_intrusive::sync::SharedSemaphore;
use futures_lite::{Future, FutureExt as LiteExt};
use futures_util::{
    future::Shared, io::BufReader, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, FutureExt,
};

use async_io::Timer;
use parking_lot::Mutex;
use rand::Rng;
use smol_timeout::TimeoutExt;
use tachyonix::{Receiver, Sender, TrySendError};
use tap::Tap;

use crate::frame::{Header, PingInfo};

const INIT_WINDOW: usize = 10;
const MAX_WINDOW: usize = 500;
const MSS: usize = 4096;

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
    last_forced_ping: Mutex<Instant>,
    recv_accepted: Receiver<Stream>,
    send_liveness: Sender<LivenessConfig>,
    liveness: LivenessConfig,
}

impl PicoMux {
    /// Creates a new picomux wrapping the given underlying connection.
    pub fn new(
        read: impl AsyncRead + 'static + Send + Unpin,
        write: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let (send_open_req, recv_open_req) = tachyonix::channel(1);
        let (send_accepted, recv_accepted) = tachyonix::channel(10000);
        let (send_liveness, recv_liveness) = tachyonix::channel(1000);
        let liveness = LivenessConfig::default();
        send_liveness.try_send(liveness).unwrap();
        let task = smolscale::spawn(
            picomux_inner(read, write, send_accepted, recv_open_req, recv_liveness).map(Arc::new),
        )
        .shared();
        Self {
            task,
            recv_accepted,
            send_open_req,
            last_forced_ping: Mutex::new(Instant::now()),
            send_liveness,
            liveness,
        }
    }

    /// Returns whether the mux is alive.
    pub fn is_alive(&self) -> bool {
        self.task.peek().is_none()
    }

    /// Sets the liveness maintenance configuration for this session.
    pub fn set_liveness(&mut self, liveness: LivenessConfig) {
        self.liveness = liveness;
        let _ = self.send_liveness.try_send(liveness);
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
        {
            let mut last_forced_ping = self.last_forced_ping.lock();
            let now = Instant::now();
            if now.saturating_duration_since(*last_forced_ping) > self.liveness.timeout {
                tracing::debug!("forcing a ping based on debounced open");
                let _ = self.send_liveness.try_send(self.liveness);
                *last_forced_ping = now;
            }
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
    mut write: impl AsyncWrite + Send + Unpin + 'static,
    send_accepted: Sender<Stream>,
    mut recv_open_req: Receiver<(Bytes, oneshot::Sender<Stream>)>,
    mut recv_liveness: Receiver<LivenessConfig>,
) -> Result<Infallible, std::io::Error> {
    let mut inner_read = BufReader::with_capacity(100_000, read);

    let (send_outgoing, mut recv_outgoing) = tachyonix::channel(1);
    let (send_pong, mut recv_pong) = tachyonix::channel(1);
    let buffer_table: DashMap<u32, _, BuildHasherDefault<AHasher>> = DashMap::default();
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
        let (send_incoming, mut recv_incoming) = tachyonix::channel(MAX_WINDOW);
        let (mut write_incoming, read_incoming) = bipe::bipe(MSS * 2);
        let (write_outgoing, mut read_outgoing) = bipe::bipe(MSS * 2);
        let stream = Stream {
            write_outgoing,
            read_incoming,
            metadata,
        };

        let send_more = SharedSemaphore::new(false, INIT_WINDOW);
        // jelly bean movers
        smolscale::spawn::<anyhow::Result<()>>({
            let send_outgoing = send_outgoing.clone();

            async move {
                let mut remote_window = INIT_WINDOW;
                let mut target_remote_window = INIT_WINDOW / 2;
                loop {
                    let min_quantum = (target_remote_window / 10).max(3).min(50);
                    let (frame, enqueued_time): (Frame, Instant) = recv_incoming.recv().await?;
                    let queue_delay = enqueued_time.elapsed();
                    tracing::trace!(
                        stream_id,
                        queue_delay = debug(queue_delay),
                        remote_window,
                        target_remote_window,
                        "queue delay measured"
                    );
                    write_incoming.write_all(&frame.body).await?;
                    remote_window -= 1;

                    // adjust the target remote window
                    if queue_delay.as_millis() > 50 {
                        target_remote_window = 3;
                    } else {
                        target_remote_window = (target_remote_window + 1).min(MAX_WINDOW);
                    }

                    if remote_window + min_quantum <= target_remote_window {
                        let quantum = target_remote_window - remote_window;
                        send_outgoing
                            .send(Frame::new(
                                stream_id,
                                CMD_MORE,
                                &(quantum as u16).to_le_bytes(),
                            ))
                            .await?;
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
        })
        .detach();

        smolscale::spawn::<anyhow::Result<()>>({
            let send_more = send_more.clone();
            let send_outgoing = send_outgoing.clone();
            async move {
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
                let mut buf = [0u8; MSS];
                loop {
                    send_more.acquire(1).await.disarm();
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
            }
        })
        .detach();
        (stream, send_incoming, send_more)
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
            let (stream, send_incoming, send_more) = create_stream(stream_id, metadata);
            // thread safety: there can be no race because we are racing the futures in the foreground and there's no await point between when we obtain the id and when we insert
            assert!(buffer_table
                .insert(stream_id, (send_incoming, send_more))
                .is_none());
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
                let _ = send_outgoing
                    .send(Frame {
                        header: Header {
                            version: 1,
                            command: CMD_PING,
                            body_len: ping_body.len() as _,
                            stream_id: 0,
                        },
                        body: ping_body.into(),
                    })
                    .await;
                let start = Instant::now();
                if recv_pong.recv().timeout(info.timeout).await.is_none() {
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        "ping-pong timed out",
                    ));
                }
                tracing::debug!(latency = debug(start.elapsed()), "PONG received");
            } else {
                return futures_util::future::pending().await;
            }
        }
    };

    outgoing_loop
        .race(open_req_loop)
        .race(ping_loop)
        .race(async {
            let mut death_after = Duration::from_secs(3600);

            let mut death_timer = Timer::after(death_after);
            loop {
                death_timer.set_after(death_after);
                tracing::trace!(death_after = debug(death_after), "setting death");
                let inner = async {
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
                            let (stream, send_incoming, send_more) =
                                create_stream(stream_id, frame.body.clone());
                            if let Err(err) = send_accepted.try_send(stream) {
                                tracing::warn!(err = debug(err), "oh dead");
                                return Err(std::io::Error::new(ErrorKind::NotConnected, "dead"));
                            }
                            buffer_table.insert(frame.header.stream_id, (send_incoming, send_more));
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
                            let back = buffer_table.get(&stream_id);
                            if let Some(back) = back {
                                back.1.release(window_increase as _);
                            } else {
                                tracing::warn!(
                                    stream_id = frame.header.stream_id,
                                    "MORE to a stream that is no longer here"
                                );
                            }
                        }
                        CMD_PSH => {
                            let back = buffer_table.get(&stream_id);
                            if let Some(back) = back {
                                if let Err(TrySendError::Full(_)) =
                                    back.0.try_send((frame.clone(), Instant::now()))
                                {
                                    tracing::error!(
                                        stream_id,
                                        "receive queue full --- this should NEVER happen"
                                    )
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
                            death_after = Duration::from_millis(ping_info.next_ping_in_ms as _);
                            let _ = send_outgoing.send(Frame::new_empty(0, CMD_PONG)).await;
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
                    Ok(())
                };
                inner
                    .or(async {
                        (&mut death_timer).await;
                        Err(std::io::Error::new(ErrorKind::TimedOut, "watchdog dead"))
                    })
                    .await?;
            }
        })
        .await
}

pub struct Stream {
    read_incoming: bipe::BipeReader,
    write_outgoing: bipe::BipeWriter,
    metadata: Bytes,
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
