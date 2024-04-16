use std::fmt::Write as _;

use crate::logs::LOGS;

pub struct Logs {
    log_cache: String,
}

impl Logs {
    pub fn new() -> Self {
        Logs {
            log_cache: String::new(),
        }
    }

    pub fn render(&mut self, ui: &mut egui::Ui) -> anyhow::Result<()> {
        self.log_cache.clear();
        {
            let raw_logs = LOGS.read();
            for log in raw_logs.iter() {
                write!(
                    &mut self.log_cache,
                    "{} {} ",
                    log.timestamp
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    log.level
                )?;
                for (k, v) in log.fields.iter() {
                    write!(&mut self.log_cache, "{k}={v} ")?;
                }
                writeln!(&mut self.log_cache)?;
            }
        }
        // let mut to_disp = self.log_cache.as_str();
        ui.centered_and_justified(|ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    let style = ui.style_mut(); // Clone the current style
                    style
                        .text_styles
                        .get_mut(&egui::TextStyle::Monospace)
                        .unwrap()
                        .size = 8.0; // Change font size

                    ui.add(egui::TextEdit::multiline(&mut self.log_cache).code_editor())
                })
        });
        Ok(())
    }
}
