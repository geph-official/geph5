use std::time::{Duration, Instant};

use egui_plot::{Line, Plot, PlotPoints};
use geph5_client::ConnInfo;
use once_cell::sync::Lazy;
use smol_timeout2::TimeoutExt;

use crate::{
    daemon::{DAEMON_HANDLE, TOTAL_BYTES_TIMESERIES},
    l10n::{l10n, l10n_country},
    pac::{set_http_proxy, unset_http_proxy},
    refresh_cell::RefreshCell,
    settings::{get_config, PROXY_AUTOCONF},
};

pub struct Dashboard {
    conn_info: RefreshCell<Option<ConnInfo>>,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Dashboard {
    pub fn new() -> Self {
        Self {
            conn_info: RefreshCell::new(),
        }
    }
    pub fn render(&mut self, ui: &mut egui::Ui) -> anyhow::Result<()> {
        let conn_info = self
            .conn_info
            .get_or_refresh(Duration::from_millis(200), || {
                smol::future::block_on(
                    DAEMON_HANDLE
                        .control_client()
                        .conn_info()
                        .timeout(Duration::from_millis(100)),
                )
                .and_then(|s| s.ok())
            })
            .cloned()
            .flatten();
        let style = ui.style().clone();
        let font_id = style.text_styles.get(&egui::TextStyle::Body).unwrap();
        let font_color = style.visuals.text_color();
        ui.columns(2, |columns| {
            columns[0].label(l10n("status"));

            match &conn_info {
                Some(ConnInfo::Connecting) => {
                    columns[1].colored_label(egui::Color32::DARK_BLUE, l10n("connecting"));
                }
                Some(ConnInfo::Connected(info)) => {
                    columns[1].colored_label(egui::Color32::DARK_GREEN, l10n("connected"));

                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        &info.exit.b2e_listen.ip().to_string(),
                        0.0,
                        egui::TextFormat {
                            font_id: font_id.clone(),
                            color: font_color,
                            ..Default::default()
                        },
                    );
                    job.append("\n", 0.0, egui::TextFormat::default());
                    job.append(
                        &format!("{}\n{}", l10n_country(info.exit.country), info.exit.city),
                        0.0,
                        egui::TextFormat {
                            font_id: font_id.clone(),
                            color: egui::Color32::GRAY,
                            ..Default::default()
                        },
                    );
                    columns[1].label(job);

                    columns[1].label(info.bridge.split(':').next().unwrap());
                }
                _ => {
                    columns[1].colored_label(egui::Color32::DARK_RED, l10n("disconnected"));
                }
            }
            columns[0].label(l10n("exit_location").to_string() + "\n\n");
            columns[0].label(l10n("via"));
        });
        ui.add_space(10.);
        ui.vertical_centered(|ui| {
            if conn_info.is_none() {
                if ui.button(l10n("connect")).clicked() {
                    tracing::warn!("connect clicked");
                    DAEMON_HANDLE.start(get_config()?)?;
                    if PROXY_AUTOCONF.get() {
                        set_http_proxy(get_config()?.http_proxy_listen.unwrap())?;
                    }
                }
            } else if ui.button(l10n("disconnect")).clicked() {
                tracing::warn!("disconnect clicked");
                DAEMON_HANDLE.stop()?;
                unset_http_proxy()?;
            }
            anyhow::Ok(())
        })
        .inner?;

        static START: Lazy<Instant> = Lazy::new(Instant::now);
        let now = Instant::now();
        let quantum_ms = 200;
        let now = *START
            + Duration::from_millis(
                (now.saturating_duration_since(*START).as_millis() / quantum_ms * quantum_ms) as _,
            );
        let range = 1000;

        let line = Line::new(
            (0..range)
                .map(|i| {
                    let x = i as f64;
                    [
                        (range as f64) - x,
                        ((TOTAL_BYTES_TIMESERIES
                            .get_at(now - Duration::from_millis(i * (quantum_ms as u64)))
                            - TOTAL_BYTES_TIMESERIES.get_at(
                                now - Duration::from_millis(i * (quantum_ms as u64) + 3000),
                            ))
                        .max(0.0)
                            / 1000.0
                            / 1000.0
                            * 8.0)
                            / 3.0,
                    ]
                })
                .collect::<PlotPoints>(),
        );

        Plot::new("my_plot")
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .y_axis_position(egui_plot::HPlacement::Right)
            .y_axis_min_width(24.0)
            .y_axis_label("Mbps")
            .include_y(0.0)
            .include_y(1.0)
            .show_x(false)
            .show_axes(egui::Vec2b { x: false, y: true })
            .show(ui, |plot| plot.line(line));

        Ok(())
    }
}
