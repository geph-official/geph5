use std::collections::VecDeque;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use smol::Task;

static TASK_POOL: Lazy<Mutex<VecDeque<Task<anyhow::Result<()>>>>> =
    Lazy::new(|| Mutex::new(VecDeque::new()));

const TASK_POOL_CAPACITY: usize = 100;

/// Add a task to the task pool. If the pool is full, the oldest task will be removed.
pub fn add_task(task: Task<anyhow::Result<()>>) {
    let mut pool = TASK_POOL.lock();
    if pool.len() >= TASK_POOL_CAPACITY {
        pool.pop_front();
    }
    TASK_POOL.lock().push_back(task);
}
