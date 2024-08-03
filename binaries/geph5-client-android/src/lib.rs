#[cfg(target_os = "android")]
mod keyboard;

#[allow(dead_code)]
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: egui_winit::winit::platform::android::activity::AndroidApp) {
    use egui_winit::winit::platform::android::EventLoopBuilderExtAndroid;
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Trace) // Default comes from `log::max_level`, i.e. Off
            .with_filter(
                android_logger::FilterBuilder::new()
                    .filter_level(log::LevelFilter::Debug)
                    .filter_module("geph5", log::LevelFilter::Trace)
                    //.filter_module("winit", log::LevelFilter::Trace)
                    .build(),
            ),
    );
    let mut cell = None;
    {
        let app = app.clone();
        geph5_client_gui::SHOW_KEYBOARD_CALLBACK
            .set(Box::new(move |s| {
                crate::keyboard::show_hide_keyboard_infal(app.clone(), s)
            }))
            .ok()
            .unwrap();
    }
    eframe::run_simple_native(
        "geph",
        eframe::NativeOptions {
            event_loop_builder: Some(Box::new(|builder| {
                builder.with_android_app(app);
            })),
            ..Default::default()
        },
        move |ctx, _frame| {
            let app = cell.get_or_insert_with(|| geph5_client_gui::App::new(ctx));
            app.render(ctx)
        },
    )
    .unwrap();
}

fn main() {}
