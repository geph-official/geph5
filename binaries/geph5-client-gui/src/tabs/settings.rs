use std::time::Duration;

use egui::mutex::Mutex;
use geph5_broker_protocol::{BrokerClient, ExitList};
use itertools::Itertools as _;
use once_cell::sync::Lazy;

use crate::{
    daemon::stop_daemon,
    l10n::{l10n, l10n_country},
    refresh_cell::RefreshCell,
    settings::{
        get_config, LANG_CODE, PASSWORD, PROXY_AUTOCONF, SELECTED_CITY, SELECTED_COUNTRY, USERNAME,
        ZOOM_FACTOR,
    },
};

pub static LOCATION_LIST: Lazy<Mutex<RefreshCell<ExitList>>> =
    Lazy::new(|| Mutex::new(RefreshCell::new()));

pub fn render_settings(_ctx: &egui::Context, ui: &mut egui::Ui) -> anyhow::Result<()> {
    if ui.button(l10n("logout")).clicked() {
        stop_daemon()?;
        USERNAME.set("".into());
        PASSWORD.set("".into());
    }

    // Preferences
    ui.separator();
    // ui.label(l10n("preferences"));
    ZOOM_FACTOR.modify(|zoom_factor| {
        ui.horizontal(|ui| {
            ui.label(l10n("zoom_factor"));
            egui::ComboBox::from_id_source("zoom_factor_cmbx")
                .selected_text(format!("{:.2}", zoom_factor))
                .show_ui(ui, |ui| {
                    ui.selectable_value(zoom_factor, 1.0, "1.0");
                    ui.selectable_value(zoom_factor, 1.25, "1.25");
                    ui.selectable_value(zoom_factor, 1.5, "1.5");
                    ui.selectable_value(zoom_factor, 1.75, "1.75");
                    ui.selectable_value(zoom_factor, 2.0, "2.0");
                });
        })
    });

    ui.horizontal(|ui| {
        ui.label(l10n("language"));
        LANG_CODE.modify(|lang_code| {
            egui::ComboBox::from_id_source("lcmbx")
                .selected_text(match lang_code.as_str() {
                    "en" => "English",
                    "zh" => "中文",
                    "fa" => "Fārsī",
                    "ru" => "Русский",
                    _ => lang_code,
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(lang_code, "en".into(), "English");
                    ui.selectable_value(lang_code, "zh".into(), "中文");
                    ui.selectable_value(lang_code, "fa".into(), "Fārsī");
                    ui.selectable_value(lang_code, "ru".into(), "Русский");
                });
        });
    });

    // Network settings
    ui.separator();
    // ui.heading(l10n("network_settings"));

    #[cfg(not(target_os = "macos"))]
    PROXY_AUTOCONF.modify(|proxy_autoconf| {
        ui.horizontal(|ui| {
            ui.label(l10n("proxy_autoconf"));
            ui.add(egui::Checkbox::new(proxy_autoconf, ""));
        })
    });

    ui.horizontal(|ui| {
        ui.label(l10n("exit_location"));
        let mut location_list = LOCATION_LIST.lock();
        let locations = location_list.get_or_refresh(Duration::from_secs(10), || {
            smol::future::block_on(async {
                let rpc_transport = get_config().unwrap().broker.unwrap().rpc_transport();
                let client = BrokerClient::from(rpc_transport);
                loop {
                    let fallible = async {
                        let exits = client.get_exits().await?.map_err(|e| anyhow::anyhow!(e))?;
                        let mut inner = exits.inner;
                        inner
                            .all_exits
                            .sort_unstable_by_key(|s| (s.1.country, s.1.city.clone()));
                        anyhow::Ok(inner)
                    };
                    match fallible.await {
                        Ok(v) => return v,
                        Err(err) => tracing::warn!("Failed to get country list: {}", err),
                    }
                }
            })
        });

        egui::ComboBox::from_id_source("country")
            .selected_text(
                SELECTED_COUNTRY
                    .get()
                    .map(l10n_country)
                    .unwrap_or_else(|| l10n("auto")),
            )
            .show_ui(ui, |ui| {
                let former = SELECTED_COUNTRY.get();
                SELECTED_COUNTRY.modify(|selected| {
                    if let Some(locations) = locations {
                        ui.selectable_value(selected, None, l10n("auto"));

                        for country in locations.all_exits.iter().map(|s| s.1.country).unique() {
                            ui.selectable_value(selected, Some(country), l10n_country(country));
                        }
                    } else {
                        ui.spinner();
                    }
                });
                if SELECTED_COUNTRY.get() != former {
                    SELECTED_CITY.set(None);
                }
            });
        if let Some(country) = SELECTED_COUNTRY.get() {
            egui::ComboBox::from_id_source("city")
                .selected_text(
                    SELECTED_CITY
                        .get()
                        .unwrap_or_else(|| l10n("auto").to_string()),
                )
                .show_ui(ui, |ui| {
                    if let Some(locations) = locations {
                        SELECTED_CITY.modify(|selected| {
                            ui.selectable_value(selected, None, l10n("auto"));
                            for city in locations
                                .all_exits
                                .iter()
                                .filter(|s| s.1.country == country)
                                .map(|s| &s.1.city)
                                .unique()
                            {
                                ui.selectable_value(
                                    selected,
                                    Some(city.to_string()),
                                    city.to_string(),
                                );
                            }
                        })
                    } else {
                        ui.spinner();
                    }
                });
        }
    });

    Ok(())
}
