use geph5_client::Config;

use crate::prefs::{pref_read, pref_write};

pub fn get_config() -> anyhow::Result<Config> {
    let settings = pref_read("settings")?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&settings)?;
    let json: serde_json::Value = serde_json::to_value(&yaml)?;
    Ok(serde_json::from_value(json)?)
}

pub fn render_settings(ui: &mut egui::Ui) -> anyhow::Result<()> {
    let mut settings_yaml = pref_read("settings").unwrap_or_default().to_string();
    ui.centered_and_justified(|ui| {
        egui::ScrollArea::vertical().show(ui, |ui| ui.code_editor(&mut settings_yaml))
    });
    pref_write("settings", &settings_yaml)?;
    Ok(())
}
