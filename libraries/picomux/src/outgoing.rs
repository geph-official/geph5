use std::{pin::Pin, sync::Arc};

use futures_lite::{AsyncWrite, AsyncWriteExt};

use crate::frame::Frame;

/// A writer for outgoing data.
#[derive(Clone)]
pub struct Outgoing {
    inner: Arc<async_lock::Mutex<Pin<Box<dyn AsyncWrite + Send + 'static>>>>,
}

impl Outgoing {
    /// Creates a new outgoing writer.
    pub fn new(write: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            inner: Arc::new(async_lock::Mutex::new(Box::pin(write))),
        }
    }

    /// Send a frame to the outgoing writer, returning once it's fully written.
    pub async fn send(&self, outgoing: Frame) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().await;
        tracing::trace!(
            stream_id = outgoing.header.stream_id,
            command = outgoing.header.command,
            body_len = outgoing.body.len(),
            "sending outgoing data into transport"
        );
        inner.write_all(&outgoing.bytes()).await?;
        Ok(())
    }

    /// Infallibly, non-blockingly enqueues a frame to be sent to the outgoing writer.
    pub fn enqueue(&self, outgoing: Frame) {
        let this = self.clone();
        smolscale::spawn(async move {
            let _ = this.send(outgoing).await;
        })
        .detach();
    }
}
