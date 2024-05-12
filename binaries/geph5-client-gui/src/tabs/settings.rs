use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
    time::Duration,
};

use egui::mutex::Mutex;
use geph5_broker_protocol::{BrokerClient, ExitList};
use geph5_client::BrokerSource;
use itertools::Itertools as _;
use once_cell::sync::Lazy;

use crate::{
    daemon::stop_daemon,
    l10n::{l10n, l10n_country},
    refresh_cell::RefreshCell,
    settings::{
        get_config, CUSTOM_BROKER, LANG_CODE, PASSWORD, PROXY_AUTOCONF, SELECTED_CITY,
        SELECTED_COUNTRY, USERNAME,
    },
};

pub static LOCATION_LIST: Lazy<Mutex<RefreshCell<ExitList>>> =
    Lazy::new(|| Mutex::new(RefreshCell::new()));

pub static BROKER_DIRECT_TCP_TEXT: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

pub fn render_settings(_ctx: &egui::Context, ui: &mut egui::Ui) -> anyhow::Result<()> {
    if ui.button(l10n("logout")).clicked() {
        stop_daemon()?;
        USERNAME.set("".into());
        PASSWORD.set("".into());
    }

    // Preferences
    ui.separator();
    // ui.label(l10n("preferences"));

    ui.horizontal(|ui| {
        ui.label(l10n("language"));
        render_language_settings(ui)
    })
    .inner?;

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

        ui.vertical(|ui| {
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

                            for country in locations.all_exits.iter().map(|s| s.1.country).unique()
                            {
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
    });

    ui.horizontal(|ui| {
        ui.label(l10n("broker"));
        render_broker_settings(ui)
    })
    .inner?;

    Ok(())
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

pub fn render_broker_settings(ui: &mut egui::Ui) -> anyhow::Result<()> {
    CUSTOM_BROKER.modify(|custom_broker| {
        let mut broker_type = match custom_broker {
            None => 1,
            Some(BrokerSource::Direct(_)) => 2,
            Some(BrokerSource::Fronted { front: _, host: _ }) => 3,
            Some(BrokerSource::DirectTcp(_)) => 4,
        };
        ui.vertical(|ui| {
            egui::ComboBox::from_id_source("custombroker")
                .selected_text(match broker_type {
                    1 => l10n("broker_none"),
                    2 => l10n("broker_direct"),
                    3 => l10n("broker_fronted"),
                    4 => l10n("broker_direct_tcp"),
                    _ => unreachable!(),
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut broker_type, 1, l10n("broker_none"));
                    ui.selectable_value(&mut broker_type, 2, l10n("broker_direct"));
                    ui.selectable_value(&mut broker_type, 3, l10n("broker_fronted"));
                    ui.selectable_value(&mut broker_type, 4, l10n("broker_direct_tcp"));
                });
            match broker_type {
                1 => {
                    *custom_broker = None;
                }
                2 => {
                    let mut addr = if let Some(BrokerSource::Direct(addr)) = custom_broker {
                        addr.to_owned()
                    } else {
                        "".into()
                    };
                    ui.text_edit_singleline(&mut addr);
                    *custom_broker = Some(BrokerSource::Direct(addr));
                }
                3 => {
                    let (mut front, mut host) =
                        if let Some(BrokerSource::Fronted { front, host }) = custom_broker {
                            (front.to_owned(), host.to_owned())
                        } else {
                            ("".into(), "".into())
                        };
                    ui.horizontal(|ui| {
                        ui.label(l10n("broker_fronted_front"));
                        ui.text_edit_singleline(&mut front);
                    });
                    ui.horizontal(|ui| {
                        ui.label(l10n("broker_fronted_host"));
                        ui.text_edit_singleline(&mut host);
                    });
                    *custom_broker = Some(BrokerSource::Fronted { front, host });
                }
                4 => {
                    let mut text = BROKER_DIRECT_TCP_TEXT.lock();
                    if text.is_none() {
                        if let Some(BrokerSource::DirectTcp(addr)) = custom_broker {
                            *text = Some(addr.to_owned().to_string());
                        } else {
                            *text = Some("".into());
                        }
                    }
                    ui.text_edit_singleline(text.as_mut().unwrap());
                    if let Ok(addr) = SocketAddr::from_str(text.clone().unwrap().as_str()) {
                        *custom_broker = Some(BrokerSource::DirectTcp(addr));
                    } else {
                        *custom_broker = Some(BrokerSource::DirectTcp(SocketAddr::V4(
                            SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0),
                        )));
                    }
                }
                _ => unreachable!(),
            }
        });
    });
    Ok(())
}
