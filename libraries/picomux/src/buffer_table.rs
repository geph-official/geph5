use std::{
    hash::BuildHasherDefault,
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::AHasher;
use dashmap::DashMap;
use futures_intrusive::sync::SharedSemaphore;

use crate::{INIT_WINDOW, MAX_WINDOW, frame::Frame};

const STREAM_ID_TOMBSTONE_TTL: Duration = Duration::from_secs(3600);
const TOMBSTONE_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

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
    tombstones: Arc<DashMap<u32, Instant, BuildHasherDefault<AHasher>>>,
    next_prune: Arc<parking_lot::Mutex<Instant>>,
}

impl BufferTable {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::with_hasher(
                BuildHasherDefault::<AHasher>::default(),
            )),
            tombstones: Arc::new(DashMap::with_hasher(
                BuildHasherDefault::<AHasher>::default(),
            )),
            next_prune: Arc::new(parking_lot::Mutex::new(
                Instant::now() + TOMBSTONE_PRUNE_INTERVAL,
            )),
        }
    }

    pub fn contains_id(&self, id: u32) -> bool {
        self.inner.contains_key(&id)
    }

    pub fn is_reserved(&self, id: u32) -> bool {
        self.prune_expired_tombstones();
        self.contains_id(id) || self.is_tombstoned(id)
    }

    pub fn create_entry(&self, stream_id: u32) -> BufferReceive {
        let (send_incoming, recv_incoming) = async_channel::unbounded::<(Frame, Instant)>();
        let send_more = SharedSemaphore::new(false, INIT_WINDOW);
        self.tombstones.remove(&stream_id);
        self.inner.insert(stream_id, (send_incoming, send_more));
        BufferReceive {
            id: stream_id,
            recv: recv_incoming,

            inner: self.inner.clone(),
            tombstones: self.tombstones.clone(),

            queue_delay: None,
        }
    }

    pub fn send_to(&self, stream_id: u32, frame: Frame) {
        if let Some(inner) = self.inner.get(&stream_id) {
            if inner.0.len() > MAX_WINDOW * 2 {
                tracing::warn!(
                    stream_id,
                    frame = debug(frame.header),
                    "individual buffer is full, so dropping message"
                );
            } else {
                let _ = inner.0.try_send((frame, Instant::now()));
            }
        }
    }

    /// Waits until the send window for the given stream is at least 1, then decrement it by 1.
    pub async fn wait_send_window(&self, stream_id: u32) {
        let semaph = if let Some(inner) = self.inner.get(&stream_id) {
            let before = inner.1.permits();
            tracing::debug!(stream_id, before, "decrementing send window");
            inner.1.clone()
        } else {
            futures_util::future::pending().await
        };
        semaph.acquire(1).await.disarm();
    }

    /// Increases the send window for the given stream.
    pub fn incr_send_window(&self, stream_id: u32, amount: u16) {
        if let Some(inner) = self.inner.get(&stream_id) {
            let before = inner.1.permits();
            tracing::debug!(
                stream_id,
                before,
                after = display(amount as usize + before),
                "increasing send window"
            );
            inner.1.release(amount as _);
        }
    }

    fn is_tombstoned(&self, id: u32) -> bool {
        if let Some(expiry) = self.tombstones.get(&id) {
            if *expiry > Instant::now() {
                return true;
            }
            drop(expiry);
            self.tombstones.remove(&id);
        }
        false
    }

    fn prune_expired_tombstones(&self) {
        let now = Instant::now();
        let mut next_prune = self.next_prune.lock();
        if now < *next_prune {
            return;
        }
        *next_prune = now + TOMBSTONE_PRUNE_INTERVAL;
        drop(next_prune);
        self.tombstones.retain(|_, expiry| *expiry > now);
    }
}

/// The receiving end for a stream-specific buffer.
pub struct BufferReceive {
    id: u32,
    recv: async_channel::Receiver<(Frame, Instant)>,
    inner: Arc<Inner>,
    tombstones: Arc<DashMap<u32, Instant, BuildHasherDefault<AHasher>>>,

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
        self.tombstones
            .insert(self.id, Instant::now() + STREAM_ID_TOMBSTONE_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_entries_are_tombstoned() {
        let table = BufferTable::new();
        let recv = table.create_entry(123);

        assert!(table.is_reserved(123));
        drop(recv);
        assert!(!table.contains_id(123));
        assert!(table.is_reserved(123));
    }

    #[test]
    fn create_entry_clears_existing_tombstone() {
        let table = BufferTable::new();
        drop(table.create_entry(123));
        assert!(table.is_reserved(123));

        let _recv = table.create_entry(123);
        assert!(table.contains_id(123));
        assert!(table.is_reserved(123));
        assert!(table.tombstones.get(&123).is_none());
    }
}
