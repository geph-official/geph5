use egui::mutex::Mutex;
use once_cell::sync::Lazy;

use crate::{
    pac::{set_http_proxy, unset_http_proxy},
    settings::{get_config, PROXY_AUTOCONF},
};

pub static DAEMON: Lazy<Mutex<Option<geph5_client::Client>>> = Lazy::new(|| Mutex::new(None));

pub fn stop_daemon() -> anyhow::Result<()> {
    let mut daemon = DAEMON.lock();
    if PROXY_AUTOCONF.get() {
        unset_http_proxy()?;
    }
    *daemon = None;
    Ok(())
}

pub fn start_daemon() -> anyhow::Result<()> {
    let mut daemon = DAEMON.lock();
    if PROXY_AUTOCONF.get() {
        set_http_proxy(get_config()?.http_proxy_listen.unwrap())?;
    }
    *daemon = Some(geph5_client::Client::start(get_config()?));
    Ok(())
}
