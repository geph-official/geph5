use std::time::Duration;

use crate::{
    daemon::DAEMON,
    l10n::l10n,
    pac::{set_http_proxy, unset_http_proxy},
    settings::{get_config, PROXY_AUTOCONF},
};

pub struct Dashboard {}

impl Dashboard {
    pub fn new() -> Self {
        Self {}
    }
    pub fn render(&mut self, ui: &mut egui::Ui) -> anyhow::Result<()> {
        let mut daemon = DAEMON.lock();

        ui.columns(2, |columns| {
            columns[0].label(l10n("status"));
            match daemon.as_ref() {
                Some(daemon) => {
                    columns[1].colored_label(egui::Color32::DARK_GREEN, l10n("connected"));
                    let start_time = daemon.start_time().elapsed().as_secs() + 1;
                    let start_time = Duration::from_secs(1) * start_time as _;
                    columns[1].label(format!("{:?}", start_time));
                    let mb_used = daemon.bytes_used() / 1_000_000.0;
                    columns[1].label(format!("{:.2} MB", mb_used));
                }
                None => {
                    columns[1].colored_label(egui::Color32::DARK_RED, l10n("disconnected"));
                }
            }
            columns[0].label(l10n("connection_time"));
            columns[0].label(l10n("data_used"));
        });
        ui.add_space(10.);
        ui.vertical_centered(|ui| {
            if daemon.is_none() {
                if ui.button(l10n("connect")).clicked() {
                    tracing::warn!("connect clicked");
                    if PROXY_AUTOCONF.get() {
                        set_http_proxy(get_config()?.http_proxy_listen.unwrap())?;
                    }
                    *daemon = Some(geph5_client::Client::start(get_config()?));
                }
            } else if ui.button(l10n("disconnect")).clicked() {
                tracing::warn!("disconnect clicked");
                if PROXY_AUTOCONF.get() {
                    unset_http_proxy()?;
                }
                *daemon = None;
            }
            anyhow::Ok(())
        })
        .inner?;

        if let Some(daemon) = daemon.as_ref() {
            if let Err(err) = daemon.check_dead() {
                ui.colored_label(egui::Color32::RED, format!("{:?}", err));
            }
        }
        Ok(())
    }
}
