use eframe::egui::Window;

#[no_mangle]
pub extern "C" fn start_app() {
  start()
}

fn start() {

    let mut text = String::new();

    eframe::run_simple_native("geph5", Default::default(), move |ctx, frame| {

        Window::new("Hello, world!").show(ctx, |ui| {
            ui.label("Hello, world!");

            ui.text_edit_singleline(&mut text);
        });

    });

}