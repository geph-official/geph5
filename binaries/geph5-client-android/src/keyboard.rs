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
