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
                write!(&mut self.log_cache, "{} {} ", log.timestamp, log.level)?;
                for (k, v) in log.fields.iter() {
                    write!(&mut self.log_cache, "{k}={v} ")?;
                }
                writeln!(&mut self.log_cache)?;
            }
        }
        let mut to_disp = self.log_cache.as_str();
        ui.centered_and_justified(|ui| ui.code_editor(&mut to_disp));
        Ok(())
    }
}
