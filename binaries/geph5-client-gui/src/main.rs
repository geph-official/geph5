mod l10n;
mod prefs;
use std::time::Duration;

use egui::{Color32, FontData, FontDefinitions, FontFamily, Visuals};
use l10n::l10n;
use prefs::pref_write;
use tap::Tap as _;

fn main() {
    // default prefs
    for (key, value) in [("lang", "en")] {
        pref_write(key, value).unwrap();
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

pub struct App {}

impl App {
    /// Constructs the app.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // light mode
        cc.egui_ctx.set_visuals(
            Visuals::light()
                .tap_mut(|vis| vis.widgets.noninteractive.fg_stroke.color = Color32::BLACK),
        );

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
        Self {}
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_secs(1));

        egui::TopBottomPanel::top("top").show(ctx, |ui| {});
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {});
        egui::CentralPanel::default().show(ctx, |ui| ui.label("hello world 你好你好"));
    }
}
