use egui::mutex::Mutex;
use native_dialog::MessageType;
use once_cell::sync::Lazy;

use crate::{
    l10n::l10n,
    pac::{set_http_proxy, unset_http_proxy},
    settings::{get_config, PROXY_AUTOCONF},
    timeseries::TimeSeries,
};

pub static TOTAL_BYTES_TIMESERIES: TimeSeries = TimeSeries::new(60 * 600);

pub static DAEMON: Lazy<Mutex<Option<geph5_client::Client>>> = Lazy::new(|| Mutex::new(None));

pub fn stop_daemon() -> anyhow::Result<()> {
    let mut daemon = DAEMON.lock();
    if PROXY_AUTOCONF.get() && daemon.is_some() {
        unset_http_proxy()?;
    }
    *daemon = None;
    Ok(())
}

pub fn start_daemon() -> anyhow::Result<()> {
    let cfg = get_config()?;
    if cfg.vpn && !elevated_command::Command::is_elevated() {
        let _ = native_dialog::MessageDialog::new()
            .set_title("Fatal error")
            .set_text(l10n("vpn_admin_only"))
            .set_type(MessageType::Error)
            .show_alert();
        Ok(())
    } else {
        let mut daemon = DAEMON.lock();
        if PROXY_AUTOCONF.get() {
            set_http_proxy(cfg.http_proxy_listen.unwrap())?;
        }
        *daemon = Some(geph5_client::Client::start(cfg));
        Ok(())
    }
}
