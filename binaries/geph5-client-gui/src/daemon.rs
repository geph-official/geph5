mod inline;
mod subproc;

use std::sync::Arc;

use geph5_client::{Config, ControlClient};

use once_cell::sync::Lazy;

use crate::timeseries::TimeSeries;

pub static TOTAL_BYTES_TIMESERIES: TimeSeries = TimeSeries::new(60 * 600);

#[cfg(unix)]
pub static DAEMON_HANDLE: Lazy<Arc<dyn Daemon>> =
    Lazy::new(|| Arc::new(inline::InlineDaemon::default()));

#[cfg(windows)]
pub static DAEMON_HANDLE: Lazy<Arc<dyn Daemon>> = Lazy::new(|| Arc::new(subproc::SubprocDaemon));

pub trait Daemon: Sync + Send + 'static {
    fn start(&self, cfg: Config) -> anyhow::Result<()>;

    fn stop(&self) -> anyhow::Result<()>;
    fn control_client(&self) -> ControlClient;

    fn check_dead(&self) -> anyhow::Result<()>;
}
