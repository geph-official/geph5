#![windows_subsystem = "windows"]

use std::time::Duration;

use daemon::{DAEMON_HANDLE, TOTAL_BYTES_TIMESERIES};
use egui::{FontData, FontDefinitions, FontFamily, Visuals};
use l10n::l10n;

#[cfg(target_os = "android")]
use egui_winit::winit::platform::android::EventLoopBuilderExtAndroid;

use once_cell::sync::OnceCell;
use refresh_cell::RefreshCell;
use settings::USERNAME;
use tabs::{dashboard::Dashboard, login::Login, logs::Logs, settings::render_settings};
pub mod daemon;
pub mod l10n;
pub mod logs;
pub mod pac;
pub mod prefs;
pub mod refresh_cell;
pub mod settings;
pub mod store_cell;
pub mod tabs;
pub mod timeseries;

pub static SHOW_KEYBOARD_CALLBACK: OnceCell<Box<dyn Fn(bool) + Send + Sync + 'static>> =
    OnceCell::new();

pub(crate) fn show_keyboard(show: bool) {
    if let Some(callback) = SHOW_KEYBOARD_CALLBACK.get() {
        callback(show);
    }
}

#[cfg(target_os = "android")]
pub fn show_soft_input(show: bool) -> anyhow::Result<bool> {
    use jni::objects::JValue;

    let ctx = ndk_context::android_context();
    let vm = match unsafe { jni::JavaVM::from_raw(ctx.vm() as _) } {
        Ok(value) => value,
        Err(e) => {
            anyhow::bail!("virtual_kbd : no vm !! : {:?}", e);
        }
    };
    let activity = unsafe { jni::objects::JObject::from_raw(ctx.context() as _) };
    let mut env = match vm.attach_current_thread() {
        Ok(value) => value,
        Err(e) => {
            anyhow::bail!("virtual_kbd : no env !! : {:?}", e);
        }
    };

    let class_ctxt = match env.find_class("android/content/Context") {
        Ok(value) => value,
        Err(e) => {
            anyhow::bail!("virtual_kbd : no class_ctxt !! : {:?}", e);
        }
    };
    let ims = match env.get_static_field(class_ctxt, "INPUT_METHOD_SERVICE", "Ljava/lang/String;") {
        Ok(value) => value,
        Err(e) => {
            anyhow::bail!("virtual_kbd : no ims !! : {:?}", e);
        }
    };

    let im_manager = match env
        .call_method(
            &activity,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[ims.borrow()],
        )
        .unwrap()
        .l()
    {
        Ok(value) => value,
        Err(e) => {
            anyhow::bail!("virtual_kbd : no im_manager !! : {:?}", e);
        }
    };

    let jni_window = match env
        .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])
        .unwrap()
        .l()
    {
        Ok(value) => value,
        Err(e) => {
            anyhow::bail!("virtual_kbd : no jni_window !! : {:?}", e);
        }
    };
    let view = match env
        .call_method(jni_window, "getDecorView", "()Landroid/view/View;", &[])
        .unwrap()
        .l()
    {
        Ok(value) => value,
        Err(e) => {
            anyhow::bail!("virtual_kbd : no view !! : {:?}", e);
        }
    };

    if show {
        let result = env
            .call_method(
                im_manager,
                "showSoftInput",
                "(Landroid/view/View;I)Z",
                &[JValue::Object(&view), 0i32.into()],
            )?
            .z()?;
        Ok(result)
    } else {
        let window_token = env
            .call_method(view, "getWindowToken", "()Landroid/os/IBinder;", &[])?
            .l()?;
        let jvalue_window_token = jni::objects::JValueGen::Object(&window_token);

        let result = env
            .call_method(
                im_manager,
                "hideSoftInputFromWindow",
                "(Landroid/os/IBinder;I)Z",
                &[jvalue_window_token, 0i32.into()],
            )?
            .z()?;
        Ok(result)
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: egui_winit::winit::platform::android::activity::AndroidApp) {
    use egui_winit::winit::platform::android::EventLoopBuilderExtAndroid;
    // android_logger::init_once(
    //     android_logger::Config::default()
    //         .with_max_level(tracing::metadata::LevelFilter::TRACE)
    //         .with_tag(env!("CARGO_PKG_NAME")),
    // );

    crate::SHOW_KEYBOARD_CALLBACK
        .set(Box::new(|b| {
            show_soft_input(b).unwrap();
        }))
        .ok()
        .unwrap();
    eframe::run_native(
        "My egui App",
        eframe::NativeOptions {
            event_loop_builder: Some(Box::new(|builder| {
                builder.with_android_app(app);
            })),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .unwrap();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TabName {
    Dashboard,
    Logs,
    Settings,
}

pub struct App {
    total_bytes: RefreshCell<f64>,
    selected_tab: TabName,
    login: Login,

    dashboard: Dashboard,
    logs: Logs,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
}

impl App {
    /// Constructs the app.
    pub fn new(ctx: &eframe::CreationContext) -> Self {
        egui_extras::install_image_loaders(&ctx.egui_ctx);
        // set up fonts. currently this uses SC for CJK, but this can be autodetected instead.
        let mut fonts = FontDefinitions::default();
        fonts.font_data.insert(
            "normal".into(),
            FontData::from_static(include_bytes!("assets/normal.otf")),
        );
        fonts.font_data.insert(
            "chinese".into(),
            FontData::from_static(include_bytes!("assets/chinese.ttf")),
        );

        {
            let fonts = fonts.families.get_mut(&FontFamily::Proportional).unwrap();
            fonts.insert(0, "chinese".into());
            fonts.insert(0, "normal".into());
        }

        ctx.egui_ctx.set_fonts(fonts);
        ctx.egui_ctx.style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);

            style.visuals = Visuals::light();
        });

        Self {
            total_bytes: RefreshCell::new(),
            selected_tab: TabName::Dashboard,
            login: Login::new(),

            dashboard: Dashboard::new(),
            logs: Logs::new(),
        }
    }
}

impl App {
    pub fn render(&mut self, ctx: &egui::Context) {
        ctx.set_zoom_factor(1.1);
        ctx.request_repaint_after(Duration::from_millis(200));

        {
            let count = self
                .total_bytes
                .get_or_refresh(Duration::from_millis(200), || {
                    smol::future::block_on(
                        DAEMON_HANDLE
                            .control_client()
                            .stat_num("total_rx_bytes".to_string()),
                    )
                    .unwrap_or_default()
                        + smol::future::block_on(
                            DAEMON_HANDLE
                                .control_client()
                                .stat_num("total_tx_bytes".to_string()),
                        )
                        .unwrap_or_default()
                })
                .copied()
                .unwrap_or_default();
            TOTAL_BYTES_TIMESERIES.record(count);
        }

        if USERNAME.get().is_empty() {
            egui::CentralPanel::default().show(ctx, |ui| {
                self.login.render(ui).unwrap();
            });

            return;
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.selected_tab,
                    TabName::Dashboard,
                    l10n("dashboard"),
                );
                ui.selectable_value(&mut self.selected_tab, TabName::Logs, l10n("logs"));
                ui.selectable_value(&mut self.selected_tab, TabName::Settings, l10n("settings"));
            });
        });

        let result = egui::CentralPanel::default().show(ctx, |ui| match self.selected_tab {
            TabName::Dashboard => self.dashboard.render(ui),
            TabName::Logs => self.logs.render(ui),
            TabName::Settings => {
                egui::ScrollArea::vertical()
                    .show(ui, |ui| render_settings(ctx, ui))
                    .inner
            }
        });

        #[cfg(not(target_os = "android"))]
        if let Err(err) = result.inner {
            use native_dialog::MessageType;
            let _ = native_dialog::MessageDialog::new()
                .set_title("Fatal error")
                .set_text(&format!(
                    "Unfortunately, a fatal error occurred:\n\n{:?}",
                    err
                ))
                .set_type(MessageType::Error)
                .show_alert();
        }
    }
}
