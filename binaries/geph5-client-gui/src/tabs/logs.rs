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
                        let raw_logs = LOGS.read();
                        for log in raw_logs.iter() {
                            let msg = log.fields.get("message").map(|s| s.as_str()).unwrap_or("");
                            remote_logs.push((
                                chrono_to_system_time(log.timestamp),
                                msg.to_string(),
                                log.fields.clone(),
                            ));
                        }
                    }
                    remote_logs.sort_unstable_by_key(|rl| rl.0);

                    Ok(remote_logs
                        .into_iter()
                        .map(|log| {
                            let datetime: DateTime<Utc> = log.0.into();
                            let mut fields = log.2.clone();
                            fields.retain(|k, _| k != "message");

                            // Format the DateTime as a string
                            let formatted_string = datetime.format("%Y-%m-%d %H:%M:%S").to_string();
                            format!("{} {} {:?}", formatted_string, log.1, fields)
                        })
                        .join("\n"))
                })
            });

        if let Some(Ok(logs)) = logs {
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
