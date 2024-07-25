use egui::Ui;


extern "C" {
    fn dummyFn();
}

pub fn ios_ui(ui: &mut Ui) {

    if ui.button("buy").clicked() {
        unsafe {
            dummyFn();
        }
    }

}