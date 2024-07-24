use eframe::egui::Window;

#[no_mangle]
pub extern "C" fn start_app() {
  start()
}

fn start() {

    eframe::run_simple_native("geph5", Default::default(), |ctx, frame| {

        Window::new("Hello, world!").show(ctx, |ui| {
            ui.label("Hello, world!");
        });

    });

}