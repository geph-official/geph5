use std::time::Duration;

use itertools::Itertools;

use crate::{daemon::DAEMON_HANDLE, l10n, logs::LOGS, refresh_cell::RefreshCell};

pub struct Logs {
    log_cache: RefreshCell<anyhow::Result<Vec<String>>>,
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

                    Ok(remote_logs)
                })
            });

        if let Some(Ok(logs)) = logs {
            let logs = strip_ansi_escapes::strip_str(logs.join("\n"));

            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            if ui.button(l10n("export_logs")).clicked() {
                use native_dialog::FileDialog;
                let path = FileDialog::new()
                    .add_filter("Text Files", &["txt"])
                    .set_filename("geph-logs.txt")
                    .show_save_single_file()
                    .unwrap();

                if let Some(path) = path {
                    let _ = std::fs::write(path, logs.as_bytes());
                }
            }

            let last_1000_lines: String = logs
                .lines()
                .rev()
                .take(1000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .join("\n");
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

                        ui.add(
                            egui::TextEdit::multiline(&mut last_1000_lines.as_str()).code_editor(),
                        )
                    })
            });
        }
        Ok(())
    }
}
