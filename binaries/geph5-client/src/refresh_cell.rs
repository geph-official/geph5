use std::{
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};

use parking_lot::Mutex;
use smol::{channel::Sender, future::FutureExt};
use smolscale::immortal::Immortal;

pub struct RefreshCell<T: Clone> {
    inner: Arc<Mutex<T>>,
    last_refresh_start: Arc<Mutex<SystemTime>>,
    _task: Immortal,
    interval: Duration,
    force_refresh: Sender<()>,
}

impl<T: Clone + Send + 'static> RefreshCell<T> {
    pub async fn create<Fut: Future<Output = T> + Send + 'static>(
        interval: Duration,
        refresh: impl Fn() -> Fut + Send + Sync + 'static,
    ) -> Self {
        let inner = Arc::new(Mutex::new(refresh().await));
        let last_refresh_start = Arc::new(Mutex::new(SystemTime::now()));
        let inner2 = inner.clone();
        let refresh = Arc::new(move || refresh().boxed());
        let (force_refresh, recv_force_refresh) = smol::channel::unbounded();
        let task = {
            let refresh = refresh.clone();
            let recv_force_refresh = recv_force_refresh.clone();
            let last_refresh_start = last_refresh_start.clone();
            Immortal::spawn(async move {
                smol::Timer::after(interval).await;
                loop {
                    *last_refresh_start.lock() = SystemTime::now();
                    let refresh = refresh.clone();
                    let fresh = async {
                        tracing::debug!("about to refresh RefreshCell...");
                        let new_value = refresh().await;
                        tracing::debug!(
                            interval = debug(interval),
                            "RefreshCell refreshed properly"
                        );
                        let mut inner = inner2.lock();
                        *inner = new_value;
                        true
                    };
                    let timeout = async {
                        smol::Timer::after(interval).await;
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
                        smol::Timer::after(interval).await;
                    }
                }
            })
        };
        Self {
            inner,
            _task: task,
            interval,
            force_refresh,
            last_refresh_start,
        }
    }

    /// Obtains the latest value. If it is out of date, immediately schedule a refresh.
    pub fn get(&self) -> T {
        let mut last_refresh_start = self.last_refresh_start.lock();
        if let Ok(old) = last_refresh_start.elapsed() {
            if old > self.interval + Duration::from_secs(30) {
                tracing::warn!(
                    interval = debug(self.interval),
                    "forcing refresh of RefreshCell due to out of date"
                );
                let _ = self.force_refresh.try_send(());
                *last_refresh_start = SystemTime::now();
            }
        }
        self.inner.lock().clone()
    }
}
