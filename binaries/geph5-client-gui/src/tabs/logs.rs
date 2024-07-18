use std::{
    fmt::Write as _,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use itertools::Itertools;

use crate::{daemon::DAEMON_HANDLE, logs::LOGS, refresh_cell::RefreshCell};

pub struct Logs {
    log_cache: RefreshCell<anyhow::Result<String>>,
}

impl Default for Logs {
    fn default() -> Self {
        Self::new()
    }
}

impl Logs {
    pub fn new() -> Self {
        Logs {
            log_cache: RefreshCell::new(),
        }
    }

    pub fn render(&mut self, ui: &mut egui::Ui) -> anyhow::Result<()> {
        let logs = self
            .log_cache
            .get_or_refresh(Duration::from_millis(500), || {
                smol::future::block_on(async {
                    let mut remote_logs = DAEMON_HANDLE.control_client().recent_logs().await?;
                    {
                        let raw_logs = LOGS.lock();
                        let raw_logs = String::from_utf8_lossy(&raw_logs);
                        for log in raw_logs.split('\n') {
                            remote_logs.push(log.to_string());
                        }
                    }

                    Ok(remote_logs.into_iter().join("\n"))
                })
            });

        if let Some(Ok(logs)) = logs {
            let logs = strip_ansi_escapes::strip_str(logs);
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

                        ui.add(egui::TextEdit::multiline(&mut logs.as_str()).code_editor())
                    })
            });
        }
        Ok(())
    }
}

fn chrono_to_system_time(dt: chrono::DateTime<chrono::Utc>) -> SystemTime {
    let duration_since_epoch = dt.timestamp_nanos_opt().unwrap();
    let std_duration = Duration::from_nanos(duration_since_epoch as u64);
    UNIX_EPOCH + std_duration
}
