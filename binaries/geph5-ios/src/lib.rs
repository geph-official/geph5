use eframe::egui::Window;

#[no_mangle]
pub extern "C" fn start_app() {
    start()
}

fn start() {
    let mut text = String::new();

    let mut app = None;

    eframe::run_simple_native("geph5", Default::default(), move |ctx, frame| {
        let app = app.get_or_insert_with(|| {
            geph5_client_gui::App::new(ctx)
        });

        app.render(ctx);
    }).unwrap();
}