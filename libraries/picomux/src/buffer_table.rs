use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::AHashMap;
use futures_intrusive::sync::SharedSemaphore;
use parking_lot::RwLock;

use crate::{INIT_WINDOW, MAX_WINDOW, frame::Frame};

const STREAM_ID_TOMBSTONE_TTL: Duration = Duration::from_secs(3600);
const TOMBSTONE_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

#[allow(clippy::type_complexity)]
type Inner = RwLock<AHashMap<u32, (async_channel::Sender<(Frame, Instant)>, SharedSemaphore)>>;
type Tombstones = RwLock<AHashMap<u32, Instant>>;

/// A table containing all the buffers for the streams within a mux.
#[derive(Clone)]
pub struct BufferTable {
    inner: Arc<Inner>,
    tombstones: Arc<Tombstones>,
    next_prune: Arc<parking_lot::Mutex<Instant>>,
}

impl BufferTable {
    pub fn new() -> Self {
        let inner = Arc::new(RwLock::new(AHashMap::default()));
        let tombstones = Arc::new(RwLock::new(AHashMap::default()));
        Self {
            inner,
            tombstones,
            next_prune: Arc::new(parking_lot::Mutex::new(
                Instant::now() + TOMBSTONE_PRUNE_INTERVAL,
            )),
        }
    }

    pub fn contains_id(&self, id: u32) -> bool {
        self.inner.read().contains_key(&id)
    }

    pub fn is_reserved(&self, id: u32) -> bool {
        self.prune_expired_tombstones();
        self.contains_id(id) || self.is_tombstoned(id)
    }

    pub fn create_entry(&self, stream_id: u32) -> BufferReceive {
        let (send_incoming, recv_incoming) = async_channel::unbounded::<(Frame, Instant)>();
        let send_more = SharedSemaphore::new(false, INIT_WINDOW);
        self.tombstones.write().remove(&stream_id);
        self.inner
            .write()
            .insert(stream_id, (send_incoming, send_more));
        BufferReceive {
            id: stream_id,
            recv: recv_incoming,

            inner: self.inner.clone(),
            tombstones: self.tombstones.clone(),

            queue_delay: None,
        }
    }

    pub fn send_to(&self, stream_id: u32, frame: Frame) {
        let sender = self
            .inner
            .read()
            .get(&stream_id)
            .map(|entry| entry.0.clone());
        if let Some(sender) = sender {
            if sender.len() > MAX_WINDOW * 2 {
                tracing::warn!(
                    stream_id,
                    frame = debug(frame.header),
                    "individual buffer is full, so dropping message"
                );
            } else {
                let _ = sender.try_send((frame, Instant::now()));
            }
        }
    }

    /// Waits until the send window for the given stream is at least 1, then decrement it by 1.
    pub async fn wait_send_window(&self, stream_id: u32) {
        // Release the table lock before either await (including the missing-ID path).
        let semaph = self
            .inner
            .read()
            .get(&stream_id)
            .map(|entry| entry.1.clone());
        let semaph = if let Some(semaph) = semaph {
            let before = semaph.permits();
            tracing::debug!(stream_id, before, "decrementing send window");
            semaph
        } else {
            futures_util::future::pending().await
        };
        semaph.acquire(1).await.disarm();
    }

    /// Increases the send window for the given stream.
    pub fn incr_send_window(&self, stream_id: u32, amount: u16) {
        let semaph = self
            .inner
            .read()
            .get(&stream_id)
            .map(|entry| entry.1.clone());
        if let Some(semaph) = semaph {
            let before = semaph.permits();
            tracing::debug!(
                stream_id,
                before,
                after = display(amount as usize + before),
                "increasing send window"
            );
            semaph.release(amount as _);
        }
    }

    fn is_tombstoned(&self, id: u32) -> bool {
        let mut tombstones = self.tombstones.write();
        if let Some(expiry) = tombstones.get(&id) {
            if *expiry > Instant::now() {
                return true;
            }
            tombstones.remove(&id);
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
        self.tombstones.write().retain(|_, expiry| *expiry > now);
    }
}

/// The receiving end for a stream-specific buffer.
pub struct BufferReceive {
    id: u32,
    recv: async_channel::Receiver<(Frame, Instant)>,
    inner: Arc<Inner>,
    tombstones: Arc<Tombstones>,

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
        self.inner.write().remove(&self.id);
        self.tombstones
            .write()
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
        assert!(table.tombstones.read().get(&123).is_none());
    }

    #[test]
    fn pending_window_wait_does_not_lock_table() {
        use std::{future::Future, task::Context};

        let table = BufferTable::new();
        let _recv = table.create_entry(123);
        table.inner.write().get_mut(&123).unwrap().1 = SharedSemaphore::new(false, 0);
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());

        // Both an exhausted window and an unknown stream must release the lock.
        for id in [123, 456] {
            let mut wait = Box::pin(table.wait_send_window(id));
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            assert!(table.inner.try_write().is_some());
            if id == 123 {
                table.incr_send_window(id, 1);
                assert!(wait.as_mut().poll(&mut cx).is_ready());
            }
        }
    }

    #[test]
    fn expired_tombstones_are_removed() {
        let table = BufferTable::new();
        let expired = Instant::now() - Duration::from_secs(1);
        table.tombstones.write().insert(123, expired);
        assert!(!table.is_reserved(123));
        assert!(!table.tombstones.read().contains_key(&123));

        table.tombstones.write().insert(456, expired);
        table
            .tombstones
            .write()
            .insert(789, Instant::now() + STREAM_ID_TOMBSTONE_TTL);
        *table.next_prune.lock() = expired;
        table.prune_expired_tombstones();
        assert!(!table.tombstones.read().contains_key(&456));
        assert!(table.is_reserved(789));
    }
}
