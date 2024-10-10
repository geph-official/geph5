use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use atomic_float::AtomicF64;
use quanta::Instant;
use smol::Timer;

pub static SCHEDULER_LAG_SECS: LazyLock<Arc<AtomicF64>> = LazyLock::new(|| {
    let shared_lag = Arc::new(AtomicF64::new(0.0));
    smolscale::spawn({
        let shared_lag = shared_lag.clone();
        async move {
            loop {
                let start = Instant::now();
                Timer::after(Duration::from_secs(1)).await;
                let true_elapsed = start.elapsed();
                let lag = (true_elapsed.as_secs_f64() - 1.0).abs();
                shared_lag.store(lag, std::sync::atomic::Ordering::Relaxed);
            }
        }
    })
    .detach();
    shared_lag
});
