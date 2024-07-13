use egui::{Align, Image, Key, Layout, TextEdit, Widget};
use geph5_broker_protocol::{BrokerClient, Credential};
use poll_promise::Promise;

use crate::{
    l10n::l10n,
    settings::{get_config, PASSWORD, USERNAME},
};



pub struct Login {
    username: String,
    password: String,

    check_login: Option<Promise<anyhow::Result<()>>>,
}

impl Default for Login {
    fn default() -> Self {
        Self::new()
    }
}

impl Login {
    pub fn new() -> Self {
        Self {
            username: "".to_string(),
            password: "".to_string(),

            check_login: None,
        }
    }

    pub fn render(&mut self, ui: &mut egui::Ui) -> anyhow::Result<()> {
        if let Some(promise) = self.check_login.as_ref() {
            ui.add_space(30.0);
            match promise.poll() {
                std::task::Poll::Ready(ready) => match ready {
                    Ok(_) => {
                        USERNAME.set(self.username.clone());
                        PASSWORD.set(self.password.clone());
                    }
                    Err(err) => {
                        let err = format!("{:?}", err);
                        ui.vertical_centered(|ui| {
                            ui.colored_label(egui::Color32::DARK_RED, err);
                            if ui.button(l10n("ok")).clicked() {
                                self.check_login = None;
                            }
                        });
                    }
                },
                std::task::Poll::Pending => {
                    ui.vertical_centered(|ui| {
                        ui.label(l10n("logging_in"));
                        ui.spinner();
                    });
                }
            }
        } else {
            let (rect, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click());
            let rect = rect.shrink2(egui::vec2(40., 0.));
            ui.allocate_ui_at_rect(rect, |ui| {
                ui.with_layout(Layout::top_down_justified(Align::Center), |ui| {
                    ui.add_space(10.);
                    Image::new(egui::include_image!("../../icon.png"))
                        .fit_to_exact_size(egui::vec2(140., 140.))
                        .ui(ui);
                    ui.add_space(10.);
                    TextEdit::singleline(&mut self.username)
                        .hint_text(l10n("username"))
                        .ui(ui);
                    TextEdit::singleline(&mut self.password)
                        .hint_text(l10n("password"))
                        .password(true)
                        .ui(ui);
                    anyhow::Ok(())
                })
                .inner?;

                if ui.button(l10n("login")).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    let username = self.username.clone();
                    let password = self.password.clone();
                    self.check_login = Some(Promise::spawn_thread("check_login", move || {
                        smolscale::block_on(check_login(username, password))
                    }));
                }
                anyhow::Ok(())
            })
            .inner?;
        }

        Ok(())
    }
}

async fn check_login(username: String, password: String) -> anyhow::Result<()> {
    let mut config = get_config()?;
    config.credentials = Credential::LegacyUsernamePassword { username, password };
    let rpc_transport = config.broker.unwrap().rpc_transport();
    let client = BrokerClient::from(rpc_transport);
    client.get_auth_token(config.credentials.clone()).await??;
    Ok(())
}
