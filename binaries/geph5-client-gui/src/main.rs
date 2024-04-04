mod dashboard;
mod l10n;
mod prefs;
mod settings;
use std::time::Duration;

use dashboard::Dashboard;

use egui::{Color32, FontData, FontDefinitions, FontFamily, Visuals};
use l10n::l10n;
use native_dialog::MessageType;
use prefs::{pref_read, pref_write};
use settings::render_settings;
use tap::Tap as _;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt, EnvFilter};

fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_writer(std::io::stderr),
        )
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5=debug".parse().unwrap())
                .from_env_lossy(),
        )
        .init();
    // default prefs
    for (key, value) in [("lang", "en")] {
        if pref_read(key).is_err() {
            pref_write(key, value).unwrap();
        }
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 300.0])
            .with_min_inner_size([400.0, 300.0]),

        ..Default::default()
    };
    eframe::run_native(
        l10n("geph"),
        native_options,
        Box::new(|cc| Box::new(App::new(cc))),
    )
    .unwrap();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TabName {
    Dashboard,
    Logs,
    Settings,
}

pub struct App {
    selected_tab: TabName,

    dashboard: Dashboard,
}

impl App {
    /// Constructs the app.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // light mode
        cc.egui_ctx.set_visuals(
            Visuals::light()
                .tap_mut(|vis| vis.widgets.noninteractive.fg_stroke.color = Color32::BLACK),
        );

        cc.egui_ctx.set_zoom_factor(1.1);

        // set up fonts. currently this uses SC for CJK, but this can be autodetected instead.
        let mut fonts = FontDefinitions::default();
        fonts.font_data.insert(
            "sarasa_sc".into(),
            FontData::from_static(include_bytes!("assets/subset.ttf")),
        );
        fonts
            .families
            .get_mut(&FontFamily::Proportional)
            .unwrap()
            .insert(0, "sarasa_sc".into());

        cc.egui_ctx.set_fonts(fonts);
        Self {
            selected_tab: TabName::Dashboard,

            dashboard: Dashboard::new(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_secs(1));

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.selected_tab,
                    TabName::Dashboard,
                    l10n("dashboard"),
                );
                ui.selectable_value(&mut self.selected_tab, TabName::Logs, l10n("logs"));
                ui.selectable_value(&mut self.selected_tab, TabName::Settings, l10n("settings"));
            })
        });

        let result = egui::CentralPanel::default().show(ctx, |ui| match self.selected_tab {
            TabName::Dashboard => self.dashboard.render(ui),
            TabName::Logs => Ok(()),
            TabName::Settings => render_settings(ui),
        });

        if let Err(err) = result.inner {
            let _ = native_dialog::MessageDialog::new()
                .set_title("Fatal error")
                .set_text(&format!(
                    "Unfortunately, a fatal error occurred, so Geph must die:\n\n{:?}",
                    err
                ))
                .set_type(MessageType::Error)
                .show_alert();
            std::process::exit(-1);
        }
    }
}
