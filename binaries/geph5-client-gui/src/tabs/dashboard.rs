use std::time::{Duration, Instant};

use egui_plot::{Line, Plot, PlotPoints};
use once_cell::sync::Lazy;

use crate::{
    daemon::{start_daemon, stop_daemon, DAEMON, TOTAL_BYTES_TIMESERIES},
    l10n::l10n,
};

pub struct Dashboard {}

impl Dashboard {
    pub fn new() -> Self {
        Self {}
    }
    pub fn render(&mut self, ui: &mut egui::Ui) -> anyhow::Result<()> {
        ui.columns(2, |columns| {
            columns[0].label(l10n("status"));
            let daemon = DAEMON.lock();
            match daemon.as_ref() {
                Some(daemon) => {
                    columns[1].colored_label(egui::Color32::DARK_GREEN, l10n("connected"));
                    let start_time = daemon.start_time().elapsed().as_secs() + 1;
                    let start_time = Duration::from_secs(1) * start_time as _;
                    columns[1].label(format!("{:?}", start_time));
                    let rx_mb = daemon.total_rx_bytes() / 1_000_000.0;
                    columns[1].label(format!("{:.2} MB", rx_mb));
                }
                None => {
                    columns[1].colored_label(egui::Color32::DARK_RED, l10n("disconnected"));
                }
            }
            columns[0].label(l10n("connection_time"));
            columns[0].label(l10n("data_used"));
        });
        ui.add_space(10.);
        ui.vertical_centered(|ui| {
            if DAEMON.lock().is_none() {
                if ui.button(l10n("connect")).clicked() {
                    tracing::warn!("connect clicked");
                    start_daemon()?;
                }
            } else if ui.button(l10n("disconnect")).clicked() {
                tracing::warn!("disconnect clicked");
                stop_daemon()?;
            }
            anyhow::Ok(())
        })
        .inner?;

        let daemon = DAEMON.lock();
        if let Some(daemon) = daemon.as_ref() {
            if let Err(err) = daemon.check_dead() {
                ui.colored_label(egui::Color32::RED, format!("{:?}", err));
            }
        }

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
            .y_axis_width(2)
            .y_axis_label("Mbps")
            .include_y(0.0)
            .include_y(1.0)
            .show_x(false)
            .show_axes(egui::Vec2b { x: false, y: true })
            .show(ui, |plot| plot.line(line));

        Ok(())
    }
}
