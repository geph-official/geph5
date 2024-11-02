use std::{
    hash::BuildHasherDefault,
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::AHasher;
use dashmap::DashMap;
use futures_intrusive::sync::SharedSemaphore;

use crate::{frame::Frame, INIT_WINDOW, MAX_WINDOW};

#[allow(clippy::type_complexity)]
type Inner = DashMap<
    u32,
    (async_channel::Sender<(Frame, Instant)>, SharedSemaphore),
    BuildHasherDefault<AHasher>,
>;

/// A table containing all the buffers for the streams within a mux.
#[derive(Clone)]
pub struct BufferTable {
    inner: Arc<Inner>,
}

impl BufferTable {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::with_hasher(
                BuildHasherDefault::<AHasher>::default(),
            )),
        }
    }

    pub fn contains_id(&self, id: u32) -> bool {
        self.inner.contains_key(&id)
    }

    pub fn create_entry(&self, stream_id: u32) -> BufferReceive {
        let (send_incoming, recv_incoming) = async_channel::unbounded::<(Frame, Instant)>();
        let send_more = SharedSemaphore::new(false, INIT_WINDOW);
        self.inner.insert(stream_id, (send_incoming, send_more));
        BufferReceive {
            id: stream_id,
            recv: recv_incoming,

            inner: self.inner.clone(),

            queue_delay: None,
        }
    }

    pub fn send_to(&self, stream_id: u32, frame: Frame) {
        if let Some(inner) = self.inner.get(&stream_id) {
            // if inner.0.len() > MAX_WINDOW * 2 {
            //     tracing::warn!(
            //         stream_id,
            //         frame = debug(frame.header),
            //         "individual buffer is full, so dropping message"
            //     );
            // } else {
            let _ = inner.0.try_send((frame, Instant::now()));
            // }
        }
    }

    /// Waits until the send window for the given stream is at least 1, then decrement it by 1.
    pub async fn wait_send_window(&self, stream_id: u32) {
        let semaph = if let Some(inner) = self.inner.get(&stream_id) {
            inner.1.clone()
        } else {
            futures_util::future::pending().await
        };
        semaph.acquire(1).await;
    }

    /// Increases the send window for the given stream.
    pub fn incr_send_window(&self, stream_id: u32, amount: u16) {
        if let Some(inner) = self.inner.get(&stream_id) {
            inner.1.release(amount as _);
        }
    }
}

/// The receiving end for a stream-specific buffer.
pub struct BufferReceive {
    id: u32,
    recv: async_channel::Receiver<(Frame, Instant)>,
    inner: Arc<Inner>,

    queue_delay: Option<Duration>,
}

impl BufferReceive {
    pub async fn recv(&mut self) -> Frame {
        if let Ok((frame, insert_time)) = self.recv.recv().await {
            self.queue_delay = Some(insert_time.elapsed());
            return frame;
        }

        futures_util::future::pending().await
    }

    pub fn queue_delay(&self) -> Option<Duration> {
        self.queue_delay
    }
}

impl Drop for BufferReceive {
    fn drop(&mut self) {
        self.inner.remove(&self.id);
    }
}
