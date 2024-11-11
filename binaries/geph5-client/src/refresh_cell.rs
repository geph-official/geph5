use std::{
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use smol::future::FutureExt;
use smolscale::immortal::Immortal;

pub struct RefreshCell<T: Clone> {
    inner: Arc<smol::lock::Mutex<(T, SystemTime)>>,
    _task: Immortal,
    interval: Duration,
    refresh: Arc<dyn Fn() -> BoxFuture<'static, T> + Send + Sync + 'static>,
}

impl<T: Clone + Send + 'static> RefreshCell<T> {
    pub async fn create<Fut: Future<Output = T> + Send + 'static>(
        interval: Duration,
        refresh: impl Fn() -> Fut + Send + Sync + 'static,
    ) -> Self {
        let inner = Arc::new(smol::lock::Mutex::new((refresh().await, SystemTime::now())));
        let inner2 = inner.clone();
        let refresh = Arc::new(move || refresh().boxed());
        let task = {
            let refresh = refresh.clone();
            Immortal::spawn(async move {
                let mut heartbeat = smol::Timer::interval(interval);
                (&mut heartbeat).await;
                loop {
                    let refresh = refresh.clone();
                    let fresh = async {
                        let new_value = refresh().await;
                        tracing::debug!(
                            interval = debug(interval),
                            "RefreshCell refreshed properly"
                        );
                        let mut inner = inner2.lock().await;
                        *inner = (new_value, SystemTime::now());
                        true
                    };
                    let timeout = async {
                        (&mut heartbeat).await;
                        tracing::debug!(
                            interval = debug(interval),
                            "RefreshCell timed out this time"
                        );
                        false
                    };
                    let refreshed = fresh.race(timeout).await;
                    if refreshed {
                        (&mut heartbeat).await;
                    }
                }
            })
        };
        Self {
            inner,
            _task: task,
            interval,
            refresh,
        }
    }

    pub async fn get(&self) -> T {
        let mut inner = self.inner.lock().await;
        if let Ok(old) = inner.1.elapsed() {
            if old > self.interval + Duration::from_secs(30) {
                tracing::warn!(
                    interval = debug(self.interval),
                    "forcing refresh of RefreshCell due to out of date"
                );
                let val = (self.refresh)().await;
                *inner = (val.clone(), SystemTime::now());
                return val;
            }
        }
        inner.0.clone()
    }
}
