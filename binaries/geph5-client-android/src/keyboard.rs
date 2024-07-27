use egui_winit::winit::platform::android::activity::AndroidApp;
use jni::{objects::JObject, JavaVM};

pub fn show_hide_keyboard_infal(app: AndroidApp, show: bool) {
    show_hide_keyboard(app, show).unwrap();
}

pub fn show_hide_keyboard(app: AndroidApp, show: bool) -> anyhow::Result<()> {
    // After Android R, it is no longer possible to show the soft keyboard
    // with `showSoftInput` alone.
    // Here we use `WindowInsetsController`, which is the other way.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr() as _)? };
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr() as _) };
    let mut env = vm.attach_current_thread()?;
    let window = env
        .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])?
        .l()?;
    let wic = env
        .call_method(
            window,
            "getInsetsController",
            "()Landroid/view/WindowInsetsController;",
            &[],
        )?
        .l()?;
    let window_insets_types = env.find_class("android/view/WindowInsets$Type")?;
    let ime_type = env
        .call_static_method(&window_insets_types, "ime", "()I", &[])?
        .i()?;
    env.call_method(
        &wic,
        if show { "show" } else { "hide" },
        "(I)V",
        &[ime_type.into()],
    )?
    .v()?;
    Ok(())
}
