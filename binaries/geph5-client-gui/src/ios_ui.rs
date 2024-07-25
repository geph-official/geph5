use egui::Ui;

use objc2::{extern_class, extern_methods, mutability, ClassType};
use objc2::rc::Id;
use objc2::runtime::NSObject;
use objc2_foundation::NSString;

extern_class!(
  #[derive(Debug)]
  pub struct GlobalFunctionsObjc;

  unsafe impl ClassType for GlobalFunctionsObjc {
    type Super = NSObject;
    type Mutability = mutability::InteriorMutable;
  }
);


extern_methods!(
  unsafe impl GlobalFunctionsObjc {
    #[method(objcDummyFnWithData:)]
    pub fn objcDummyFn(data: &NSString);
  }
);


extern "C" {
    fn dummyFn();
}



swift_rs::swift!(fn swiftrs_dummy_fn(data: &swift_rs::SRString));

pub fn ios_ui(ui: &mut Ui) {
    ui.horizontal(|ui| {
        if ui.button("buy c").clicked() {
            unsafe {
                dummyFn();
            }
        }

        if ui.button("buy objc").clicked() {
            unsafe {
                let ns_string = NSString::from_str("I was called via rust / objc2 and can have a custom message!");
                GlobalFunctionsObjc::objcDummyFn(&ns_string);
            }
        }

        if ui.button("buy swift").clicked() {
            unsafe {
                let swift_string = swift_rs::SRString::from("I was called via rust / swiftrs and can have a custom message!");
                swiftrs_dummy_fn(&swift_string);
            }
        }
    });
}
