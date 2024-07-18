#![windows_subsystem = "windows"]

use egui::IconData;
use geph5_client_gui::daemon::DAEMON_HANDLE;
use geph5_client_gui::l10n::l10n;

use native_dialog::MessageType;

use geph5_client_gui::pac::unset_http_proxy;

use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt, EnvFilter};

// 0123456789

#[cfg(not(target_os = "android"))]
fn main() {
    let ((_, _), _) = binary_search::binary_search((1, ()), (65536, ()), |lim| {
        if rlimit::increase_nofile_limit(lim).unwrap_or_default() >= lim {
            binary_search::Direction::Low(())
        } else {
            binary_search::Direction::High(())
        }
    });

    use geph5_client_gui::logs::LOGS;
    use single_instance::SingleInstance;

    let instance = SingleInstance::new("geph5-client-gui");
    if let Ok(instance) = instance {
        if !instance.is_single() {
            native_dialog::MessageDialog::new()
                .set_type(MessageType::Error)
                .set_text(l10n("geph_already_running"))
                .set_title("Error")
                .show_alert()
                .unwrap();
            std::process::exit(-1)
        }
    }

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_writer(|| &*LOGS),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_writer(std::io::stderr),
        )
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5=debug".parse().unwrap())
                .from_env_lossy(),
        )
        .init();

    let (icon_rgba, icon_width, icon_height) = {
        let icon = include_bytes!("../icon.ico");
        let image = image::load_from_memory(icon)
            .expect("Failed to open icon path")
            .into_rgba8();
        let (width, height) = image.dimensions();
        let rgba = image.into_raw();
        (rgba, width, height)
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([320.0, 320.0])
            .with_min_inner_size([320.0, 320.0])
            .with_icon(IconData {
                rgba: icon_rgba,
                width: icon_width,
                height: icon_height,
            }),
        ..Default::default()
    };

    let mut cell = None;
    eframe::run_simple_native(l10n("geph"), native_options, move |ctx, _frame| {
        let app = cell.get_or_insert_with(|| geph5_client_gui::App::new(ctx));
        app.render(ctx)
    })
    .unwrap();

    eprintln!("****** STOPPED ******");
    let _ = smol::future::block_on(DAEMON_HANDLE.control_client().stop());
    unset_http_proxy().unwrap();
}
