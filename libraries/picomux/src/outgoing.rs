use std::sync::{Arc, OnceLock};

use crossbeam_queue::SegQueue;
use futures_lite::{AsyncWrite, AsyncWriteExt};

use crate::frame::Frame;

/// A writer for outgoing data.
#[derive(Clone)]
pub struct Outgoing {
    inner: Arc<Inner>,
    err: Arc<OnceLock<anyhow::Error>>,
    _task: Arc<async_task::Task<()>>,
}

impl Outgoing {
    /// Creates a new outgoing writer.
    pub fn new(write: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        let inner = Arc::new(Inner::default());
        let err_cell = Arc::new(OnceLock::new());
        Self {
            inner: inner.clone(),
            err: err_cell.clone(),
            _task: Arc::new(smolscale::spawn(async move {
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
                if self.inner.queue.is_empty() {
                    Some(anyhow::Ok(()))
                } else {
                    None
                }
            })
            .await?;
        Ok(())
    }

    /// Infallibly, non-blockingly enqueues a frame to be sent to the outgoing writer.
    pub fn enqueue(&self, outgoing: Frame) {
        tracing::debug!(
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
}

async fn outgoing_loop(
    mut write: impl AsyncWrite + Send + Unpin + 'static,
    inner: Arc<Inner>,
) -> anyhow::Result<()> {
    scopeguard::defer!(inner.shrink_signal.notify_all());
    loop {
        let next = inner.grow_signal.wait_until(|| inner.queue.pop()).await;
        inner.shrink_signal.notify_all();
        write.write_all(&next.bytes()).await?;
    }
}
