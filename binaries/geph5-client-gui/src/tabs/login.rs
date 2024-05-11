use egui::{Align, Layout};
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
                        ui.spinner();
                    });
                }
            }
        } else {
            ui.with_layout(Layout::top_down_justified(Align::LEFT), |ui| {
                ui.label(l10n("username"));
                ui.text_edit_singleline(&mut self.username);

                ui.label(l10n("password"));
                ui.text_edit_singleline(&mut self.password);
                anyhow::Ok(())
            })
            .inner?;

            if ui.button(l10n("login")).clicked() {
                let username = self.username.clone();
                let password = self.password.clone();
                self.check_login = Some(Promise::spawn_thread("check_login", move || {
                    smol::future::block_on(check_login(username, password))
                }));
            }
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
