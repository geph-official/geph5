use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use crossbeam_queue::SegQueue;
use geph5_rt::{Task, spawn};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::frame::Frame;

/// A writer for outgoing data.
#[derive(Clone)]
pub struct Outgoing {
    inner: Arc<Inner>,
    err: Arc<OnceLock<anyhow::Error>>,
    _task: Arc<Task<()>>,
}

impl Outgoing {
    /// Creates a new outgoing writer.
    pub fn new(write: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        let inner = Arc::new(Inner::default());
        let err_cell = Arc::new(OnceLock::new());
        // DIAG watchdog: detect an outgoing_loop parked while frames are queued.
        {
            let weak = Arc::downgrade(&inner);
            spawn(async move {
                let mut last_written = 0u64;
                let mut stuck_ticks = 0u32;
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let Some(inner) = weak.upgrade() else { return };
                    let q = inner.queue.len();
                    let w = inner.written.load(Ordering::Relaxed);
                    let iw = inner.in_write.load(Ordering::Relaxed);
                    if q > 0 && w == last_written && !iw {
                        stuck_ticks += 1;
                        tracing::warn!(
                            queue_len = q,
                            written = w,
                            stuck_secs = stuck_ticks * 2,
                            "OUTGOING-STUCK: frames queued but loop not writing"
                        );
                    } else {
                        stuck_ticks = 0;
                    }
                    last_written = w;
                }
            })
            .detach();
        }
        Self {
            inner: inner.clone(),
            err: err_cell.clone(),
            _task: Arc::new(spawn(async move {
                if let Err(err) = outgoing_loop(write, inner).await {
                    err_cell.set(err).unwrap();
                }
            })),
        }
    }

    /// Send a frame to the outgoing writer, returning once it's fully written.
    pub async fn send(&self, outgoing: Frame) -> anyhow::Result<()> {
        self.enqueue(outgoing);
        // wait until the queue is empty
        self.inner
            .shrink_signal
            .wait_until(|| {
                if let Some(err) = self.err.get() {
                    return Some(Err(anyhow::anyhow!("{:?}", err)));
                }
                if self.inner.queue.len() < 10 {
                    Some(anyhow::Ok(()))
                } else {
                    None
                }
            })
            .await?;
        Ok(())
    }

    /// Number of frames currently queued but not yet written to the wire.
    pub fn queue_len(&self) -> usize {
        self.inner.queue.len()
    }

    /// Total number of frames the outgoing loop has fully written to the wire.
    pub fn written(&self) -> u64 {
        self.inner.written.load(Ordering::Relaxed)
    }

    /// Whether the outgoing loop is currently blocked inside `write_all`.
    pub fn in_write(&self) -> bool {
        self.inner.in_write.load(Ordering::Relaxed)
    }

    /// Infallibly, non-blockingly enqueues a frame to be sent to the outgoing writer.
    pub fn enqueue(&self, outgoing: Frame) {
        tracing::trace!(
            command = outgoing.header.command,
            stream_id = outgoing.header.stream_id,
            body_len = outgoing.header.body_len,
            "sending outgoing frame"
        );
        self.inner.queue.push(outgoing);
        self.inner.grow_signal.notify_one();
    }
}

#[derive(Default)]
struct Inner {
    queue: SegQueue<Frame>,
    grow_signal: async_event::Event,
    shrink_signal: async_event::Event,
    written: AtomicU64,
    in_write: AtomicBool,
    dead: AtomicBool,
}

async fn outgoing_loop(
    mut write: impl AsyncWrite + Send + Unpin + 'static,
    inner: Arc<Inner>,
) -> anyhow::Result<()> {
    scopeguard::defer!(inner.shrink_signal.notify_all());
    loop {
        let next = inner.grow_signal.wait_until(|| inner.queue.pop()).await;
        inner.shrink_signal.notify_all();
        inner.in_write.store(true, Ordering::Relaxed);
        write.write_all(&next.bytes()).await?;
        inner.in_write.store(false, Ordering::Relaxed);
        inner.written.fetch_add(1, Ordering::Relaxed);
    }
}
