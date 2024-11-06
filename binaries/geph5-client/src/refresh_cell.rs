use std::{future::Future, sync::Arc, time::Duration};

use parking_lot::Mutex;
use smol::future::FutureExt;
use smolscale::immortal::Immortal;

pub struct RefreshCell<T: Clone> {
    inner: Arc<Mutex<T>>,
    _task: Immortal,
}

impl<T: Clone + Send + 'static> RefreshCell<T> {
    pub async fn create<Fut: Future<Output = T> + Send>(
        interval: Duration,
        mut refresh: impl FnMut() -> Fut + Send + 'static,
    ) -> Self {
        let inner = Arc::new(Mutex::new(refresh().await));
        let inner2 = inner.clone();
        let task = Immortal::spawn(async move {
            let mut heartbeat = smol::Timer::interval(interval);
            loop {
                let fresh = async {
                    let new_value = refresh().await;
                    tracing::debug!(interval = debug(interval), "RefreshCell refreshed properly");
                    let mut inner = inner2.lock();
                    *inner = new_value;
                };
                let timeout = async {
                    (&mut heartbeat).await;
                    tracing::debug!(
                        interval = debug(interval),
                        "RefreshCell timed out this time"
                    );
                };
                fresh.race(timeout).await;
            }
        });
        Self { inner, _task: task }
    }

    pub fn get(&self) -> T {
        self.inner.lock().clone()
    }
}
