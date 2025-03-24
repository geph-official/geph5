use std::collections::VecDeque;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use smol::Task;

static TASK_POOL: Lazy<Mutex<VecDeque<Task<anyhow::Result<()>>>>> =
    Lazy::new(|| Mutex::new(VecDeque::new()));

/// Add a task to the task pool. If the pool is full, the oldest task will be removed.
pub fn add_task(task_limit: u32, task: Task<anyhow::Result<()>>) {
    let mut pool = TASK_POOL.lock();
    if pool.len() >= task_limit as usize {
        pool.pop_front();
    }
    pool.push_back(task);
}
