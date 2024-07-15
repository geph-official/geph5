#![windows_subsystem = "windows"]

use std::time::Duration;

use daemon::{DAEMON_HANDLE, TOTAL_BYTES_TIMESERIES};
use egui::{FontData, FontDefinitions, FontFamily, Visuals};
use l10n::l10n;

use refresh_cell::RefreshCell;
use settings::USERNAME;
use tabs::{dashboard::Dashboard, login::Login, logs::Logs, settings::render_settings};
pub mod daemon;
pub mod l10n;
pub mod logs;
pub mod pac;
pub mod prefs;
pub mod refresh_cell;
pub mod settings;
pub mod store_cell;
pub mod tabs;
pub mod timeseries;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TabName {
    Dashboard,
    Logs,
    Settings,
}

pub struct App {
    total_bytes: RefreshCell<f64>,
    selected_tab: TabName,
    login: Login,

    dashboard: Dashboard,
    logs: Logs,
}

impl App {
    /// Constructs the app.
    pub fn new(ctx: &egui::Context) -> Self {
        egui_extras::install_image_loaders(ctx);
        // set up fonts. currently this uses SC for CJK, but this can be autodetected instead.
        let mut fonts = FontDefinitions::default();
        fonts.font_data.insert(
            "normal".into(),
            FontData::from_static(include_bytes!("assets/normal.otf")),
        );
        fonts.font_data.insert(
            "chinese".into(),
            FontData::from_static(include_bytes!("assets/chinese.ttf")),
        );

        {
            let fonts = fonts.families.get_mut(&FontFamily::Proportional).unwrap();
            fonts.insert(0, "chinese".into());
            fonts.insert(0, "normal".into());
        }

        ctx.set_fonts(fonts);
        ctx.style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);

            style.visuals = Visuals::light();
        });

        Self {
            total_bytes: RefreshCell::new(),
            selected_tab: TabName::Dashboard,
            login: Login::new(),

            dashboard: Dashboard::new(),
            logs: Logs::new(),
        }
    }
}

impl App {
    pub fn render(&mut self, ctx: &egui::Context) {
        ctx.set_zoom_factor(1.1);
        ctx.request_repaint_after(Duration::from_millis(200));

        {
            let count = self
                .total_bytes
                .get_or_refresh(Duration::from_millis(200), || {
                    smol::future::block_on(
                        DAEMON_HANDLE
                            .control_client()
                            .stat_num("total_rx_bytes".to_string()),
                    )
                    .unwrap_or_default()
                })
                .copied()
                .unwrap_or_default();
            TOTAL_BYTES_TIMESERIES.record(count);
        }

        if USERNAME.get().is_empty() {
            egui::CentralPanel::default().show(ctx, |ui| {
                self.login.render(ui).unwrap();
            });

            return;
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.selected_tab,
                    TabName::Dashboard,
                    l10n("dashboard"),
                );
                ui.selectable_value(&mut self.selected_tab, TabName::Logs, l10n("logs"));
                ui.selectable_value(&mut self.selected_tab, TabName::Settings, l10n("settings"));
            });
        });

        let result = egui::CentralPanel::default().show(ctx, |ui| match self.selected_tab {
            TabName::Dashboard => self.dashboard.render(ui),
            TabName::Logs => self.logs.render(ui),
            TabName::Settings => {
                egui::ScrollArea::vertical()
                    .show(ui, |ui| render_settings(ctx, ui))
                    .inner
            }
        });

        #[cfg(not(target_os = "android"))]
        if let Err(err) = result.inner {
            use native_dialog::MessageType;
            let _ = native_dialog::MessageDialog::new()
                .set_title("Fatal error")
                .set_text(&format!(
                    "Unfortunately, a fatal error occurred:\n\n{:?}",
                    err
                ))
                .set_type(MessageType::Error)
                .show_alert();
        }
    }
}
