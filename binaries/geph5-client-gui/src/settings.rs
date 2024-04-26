use geph5_broker_protocol::Credential;
use geph5_client::Config;
use once_cell::sync::Lazy;
use smol_str::{SmolStr, ToSmolStr};

use crate::{l10n, store_cell::StoreCell};

pub fn get_config() -> anyhow::Result<Config> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(include_str!("settings_default.yaml"))?;
    let json: serde_json::Value = serde_json::to_value(&yaml)?;
    let mut cfg: Config = serde_json::from_value(json)?;
    cfg.credentials = Credential::LegacyUsernamePassword {
        username: USERNAME.get(),
        password: PASSWORD.get(),
    };
    Ok(cfg)
}

static USERNAME: Lazy<StoreCell<String>> =
    Lazy::new(|| StoreCell::new_persistent("username", || "".to_string()));

static PASSWORD: Lazy<StoreCell<String>> =
    Lazy::new(|| StoreCell::new_persistent("password", || "".to_string()));

pub static ZOOM_FACTOR: Lazy<StoreCell<f32>> =
    Lazy::new(|| StoreCell::new_persistent("zoom_factor", || 1.0));

pub static LANG_CODE: Lazy<StoreCell<SmolStr>> =
    Lazy::new(|| StoreCell::new_persistent("lang_code", || "en".to_smolstr()));

pub static PROXY_AUTOCONF: Lazy<StoreCell<bool>> =
    Lazy::new(|| StoreCell::new_persistent("proxy_autoconf", || false));

pub fn render_settings(_ctx: &egui::Context, ui: &mut egui::Ui) -> anyhow::Result<()> {
    // Account settings
    // ui.heading(l10n("account_info"));
    USERNAME.modify(|username| {
        ui.horizontal(|ui| {
            ui.label(l10n("username"));
            ui.text_edit_singleline(username);
        })
    });
    PASSWORD.modify(|password| {
        ui.horizontal(|ui| {
            ui.label(l10n("password"));
            ui.add(egui::TextEdit::singleline(password).password(true));
        })
    });

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
    PROXY_AUTOCONF.modify(|proxy_autoconf| {
        ui.horizontal(|ui| {
            ui.label(l10n("proxy_autoconf"));
            ui.add(egui::Checkbox::new(proxy_autoconf, ""));
        })
    });

    // // Configuration file
    // ui.separator();
    // // ui.heading(l10n("Configuration File"));
    // let config = get_config()?;
    // let config_json = serde_json::to_value(config)?;
    // let config_yaml = serde_yaml::to_string(&config_json)?;

    // egui::ScrollArea::vertical().show(ui, |ui| ui.code_editor(&mut config_yaml.as_str()));

    Ok(())
}
