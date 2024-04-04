use futures_util::{future::Shared, FutureExt};
use geph5_broker_protocol::ExitList;
use poll_promise::Promise;
use smol::Task;

use crate::l10n::l10n;

pub struct Dashboard {
    selected_server: Option<(String, String)>,
    server_list: Shared<Task<ExitList>>,
}

impl Dashboard {
    pub fn new() -> Self {
        Self {
            selected_server: None,
            server_list: smolscale::spawn(get_server_list()).shared(),
        }
    }
    pub fn render(&mut self, ui: &mut egui::Ui) -> anyhow::Result<()> {
        ui.label(l10n("selected_server"));
        egui::ComboBox::from_label("")
            .selected_text(render_exit_selection(&self.selected_server))
            .show_ui(ui, |ui| {
                if let Some(list) = self.server_list.peek() {
                } else {
                    ui.label(l10n("loading_exit_list"));
                }
                // ui.selectable_value(&mut self.selected_server, "Apple".to_string(), "Apple");
                // ui.selectable_value(&mut self.selected_server, "Pear".to_string(), "Pear");
                // ui.selectable_value(&mut self.selected_server, "Orange".to_string(), "Orange");
            });
        anyhow::Ok(())
    }
}

fn render_exit_selection(selection: &Option<(String, String)>) -> String {
    if let Some((country, city)) = selection.as_ref() {
        format!("{country}/{city}")
    } else {
        "Auto".into()
    }
}

async fn get_server_list() -> ExitList {
    smol::future::pending().await
}
