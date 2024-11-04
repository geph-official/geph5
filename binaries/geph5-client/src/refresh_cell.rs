use std::{future::Future, sync::Arc, time::Duration};

use parking_lot::Mutex;
use smolscale::immortal::Immortal;

pub struct RefreshCell<T: Clone> {
    inner: Arc<Mutex<T>>,
    _task: Immortal,
}

impl<T: Clone + Send + 'static> RefreshCell<T> {
    pub async fn create<Fut: Future<Output = T> + Send>(
        interval: Duration,
        refresh: impl Fn() -> Fut + Send + 'static,
    ) -> Self {
        let inner = Arc::new(Mutex::new(refresh().await));
        let inner2 = inner.clone();
        let task = Immortal::spawn(async move {
            loop {
                smol::Timer::after(interval).await;
                let new_value = refresh().await;
                let mut inner = inner2.lock();
                *inner = new_value;
            }
        });
        Self { inner, _task: task }
    }

    pub fn get(&self) -> T {
        self.inner.lock().clone()
    }
}
