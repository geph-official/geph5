use std::{sync::LazyLock, time::Duration};

use egui::{mutex::Mutex, Id};
use geph5_broker_protocol::{BrokerClient, ExitList, UserInfo};
use geph5_client::{BridgeMode, Client};
use itertools::Itertools as _;
use smol_str::format_smolstr;

use crate::{
    daemon::DAEMON_HANDLE,
    l10n::{l10n, l10n_country},
    refresh_cell::RefreshCell,
    settings::{
        get_config, BRIDGE_MODE, HTTP_PROXY_PORT, LANG_CODE, PASSTHROUGH_CHINA, PASSWORD,
        PROXY_AUTOCONF, SELECTED_CITY, SELECTED_COUNTRY, SOCKS5_PORT, USERNAME, VPN_MODE,
    },
};

pub static LOCATION_LIST: LazyLock<Mutex<RefreshCell<ExitList>>> =
    LazyLock::new(|| Mutex::new(RefreshCell::new()));

pub struct Settings {
    user_info: RefreshCell<anyhow::Result<UserInfo>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

impl Settings {
    pub fn new() -> Self {
        Settings {
            user_info: RefreshCell::new(),
        }
    }

    pub fn render(&mut self, ui: &mut egui::Ui) -> anyhow::Result<()> {
        let inert_config = get_config()?.inert();

        let user_info = self.user_info.get_or_refresh(Duration::from_secs(10), || {
            let client = Client::start(inert_config);
            smolscale::block_on(async move { client.user_info().await })
        });
        if let Some(user_info) = user_info {
            match user_info {
                Ok(info) => {
                    let user_info_str = format_smolstr!(
                        "{} / expires {}",
                        info.user_id,
                        info.plus_expires_unix.unwrap_or_default()
                    );
                    ui.label(user_info_str.as_str());
                }
                Err(err) => {
                    ui.colored_label(egui::Color32::DARK_RED, err.to_string());
                }
            }
        } else {
            ui.colored_label(egui::Color32::DARK_GRAY, "Loading user info...");
        }

        if ui.button(l10n("logout")).clicked() {
            let _ = DAEMON_HANDLE.stop();
            USERNAME.set("".into());
            PASSWORD.set("".into());
        }

        // Preferences
        ui.separator();

        ui.columns(2, |columns| {
            columns[0].label(l10n("language"));
            render_language_settings(&mut columns[1])
        })?;

        // Network settings
        ui.separator();

        #[cfg(any(target_os = "linux", target_os = "windows"))]
        VPN_MODE.modify(|vpn_mode| {
            ui.columns(2, |columns| {
                columns[0].label(l10n("vpn_mode"));
                columns[1].add(egui::Checkbox::new(vpn_mode, ""));
            })
        });

        PASSTHROUGH_CHINA.modify(|china_passthrough| {
            ui.columns(2, |columns| {
                columns[0].label(l10n("china_passthrough"));
                columns[1].add(egui::Checkbox::new(china_passthrough, ""));
            })
        });

        ui.columns(2, |columns| {
            columns[0].label(l10n("exit_location"));
            let is_plus = match user_info {
                Some(Ok(user_info)) => user_info.plus_expires_unix.is_some(),
                _ => false,
            };
            let mut location_list = LOCATION_LIST.lock();
            let locations = location_list.get_or_refresh(Duration::from_secs(10), move || {
                smolscale::block_on(async move {
                    let rpc_transport = get_config().unwrap().broker.unwrap().rpc_transport();
                    let client = BrokerClient::from(rpc_transport);
                    loop {
                        let fallible = async {
                            let all_exits =
                                client.get_exits().await?.map_err(|e| anyhow::anyhow!(e))?;
                            let all_free_exits = client
                                .get_free_exits()
                                .await?
                                .map_err(|e| anyhow::anyhow!(e))?;

                            let mut exits = if is_plus {
                                all_exits.inner
                            } else {
                                all_free_exits.inner
                            };
                            exits
                                .all_exits
                                .sort_unstable_by_key(|s| (s.1.country, s.1.city.clone()));
                            anyhow::Ok(exits)
                        };
                        match fallible.await {
                            Ok(v) => return v,
                            Err(err) => tracing::warn!("Failed to get country list: {}", err),
                        }
                    }
                })
            });

            columns[1].vertical(|ui| {
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

                                for country in
                                    locations.all_exits.iter().map(|s| s.1.country).unique()
                                {
                                    ui.selectable_value(
                                        selected,
                                        Some(country),
                                        l10n_country(country),
                                    );
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
        });

        ui.collapsing(l10n("advanced_settings"), |ui| {
            BRIDGE_MODE.modify(|bridge_mode| {
                let mode_label = |bm: BridgeMode| match bm {
                    BridgeMode::Auto => "Auto",
                    BridgeMode::ForceBridges => "Force bridges",
                    BridgeMode::ForceDirect => "Force direct",
                };
                ui.horizontal(|ui| {
                    ui.label("Bridge mode");

                    egui::ComboBox::from_id_source("bridge")
                        .selected_text(mode_label(*bridge_mode))
                        .show_ui(ui, |ui| {
                            for this_mode in [
                                BridgeMode::Auto,
                                BridgeMode::ForceBridges,
                                BridgeMode::ForceDirect,
                            ] {
                                ui.selectable_value(bridge_mode, this_mode, mode_label(this_mode));
                            }
                        });
                });
            });

            PROXY_AUTOCONF.modify(|proxy_autoconf| {
                ui.horizontal(|ui| {
                    ui.label(l10n("proxy_autoconf"));
                    ui.add(egui::Checkbox::new(proxy_autoconf, ""));
                })
            });
            SOCKS5_PORT.modify(|socks5_port| {
                ui.horizontal(|ui| {
                    ui.label(l10n("socks5_port"));
                    ui.add(egui::DragValue::new(socks5_port));
                });
            });

            HTTP_PROXY_PORT.modify(|http_proxy_port| {
                ui.horizontal(|ui| {
                    ui.label(l10n("http_proxy_port"));
                    ui.add(egui::DragValue::new(http_proxy_port));
                })
            });
        });

        Ok(())
    }
}

pub fn render_language_settings(ui: &mut egui::Ui) -> anyhow::Result<()> {
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
    Ok(())
}
