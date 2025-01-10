use std::{
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};

use parking_lot::Mutex;
use smol::{channel::Sender, future::FutureExt};
use smolscale::immortal::Immortal;

pub struct RefreshCell<T: Clone> {
    inner: Arc<Mutex<(T, SystemTime)>>,
    _task: Immortal,
    interval: Duration,
    force_refresh: Sender<()>,
}

impl<T: Clone + Send + 'static> RefreshCell<T> {
    pub async fn create<Fut: Future<Output = T> + Send + 'static>(
        interval: Duration,
        refresh: impl Fn() -> Fut + Send + Sync + 'static,
    ) -> Self {
        let inner = Arc::new(Mutex::new((refresh().await, SystemTime::now())));
        let inner2 = inner.clone();
        let refresh = Arc::new(move || refresh().boxed());
        let (force_refresh, recv_force_refresh) = smol::channel::unbounded();
        let task = {
            let refresh = refresh.clone();
            let recv_force_refresh = recv_force_refresh.clone();
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
                        let mut inner = inner2.lock();
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
                    let force = async {
                        let v = recv_force_refresh.recv().await;
                        if v.is_ok() {
                            tracing::debug!(
                                interval = debug(interval),
                                "RefreshCell force refresh received"
                            );
                            false
                        } else {
                            smol::future::pending().await
                        }
                    };
                    let refreshed = fresh.race(timeout).race(force).await;
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
            force_refresh,
        }
    }

    pub async fn get(&self) -> T {
        let inner = self.inner.lock();
        if let Ok(old) = inner.1.elapsed() {
            if old > self.interval + Duration::from_secs(30) {
                tracing::warn!(
                    interval = debug(self.interval),
                    "forcing refresh of RefreshCell due to out of date"
                );
                let _ = self.force_refresh.try_send(());
            }
        }
        inner.0.clone()
    }
}
