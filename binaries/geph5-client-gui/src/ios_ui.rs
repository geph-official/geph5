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

pub fn ios_ui(ui: &mut Ui) {

    if ui.button("buy c").clicked() {
        unsafe {
            dummyFn();
        }
    }

    if ui.button("buy objc").clicked() {
        unsafe {
            let ns_string = NSString::from_str("I was called via rust and can have a custom message!");
            GlobalFunctionsObjc::objcDummyFn(&ns_string);
        }
    }

}