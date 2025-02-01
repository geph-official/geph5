use std::{
    sync::{atomic::AtomicUsize, LazyLock},
    time::Duration,
};

use crate::CONFIG_FILE;

static TASK_COUNT: AtomicUsize = AtomicUsize::new(0);
static TASK_KILLER: LazyLock<async_event::Event> = LazyLock::new(async_event::Event::new);

/// Obtains the current task count.
pub fn get_task_count() -> usize {
    TASK_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Adds a task to the limited task pool, then waits for the death signal.
pub async fn new_task_until_death(protected_period: Duration) -> anyhow::Result<()> {
    let task_limit = CONFIG_FILE.wait().task_limit;
    let count = TASK_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if rand::random::<f32>() < 0.01 {
        tracing::debug!("** current task count: {count} **");
    }
    // tracing::debug!(count, task_limit, "making a task death handle");
    scopeguard::defer!({
        TASK_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    });
    TASK_KILLER.notify_one();

    smol::Timer::after(protected_period).await;

    // wait until something horrible happens
    TASK_KILLER
        .wait_until(|| {
            if TASK_COUNT.load(std::sync::atomic::Ordering::Relaxed) > task_limit {
                Some(())
            } else {
                None
            }
        })
        .await;
    // tracing::warn!(task_limit, "a task is killed due to overflow");
    anyhow::bail!("too many tasks")
}
