mod keyboard;

use std::sync::Arc;

use egui::ViewportId;
use egui_wgpu::wgpu::{self, rwh::HasDisplayHandle};
use egui_winit::winit::{
    self,
    event::{Event, WindowEvent},
};

use geph5_client_gui::App;
use jni::{objects::JObject, JNIEnv, JavaVM};
use keyboard::show_soft_input;
use once_cell::sync::OnceCell;
use winit::event_loop::{EventLoop, EventLoopBuilder, EventLoopWindowTarget};

#[cfg(target_os = "android")]
use winit::platform::android::activity::AndroidApp;

use egui_wgpu::winit::Painter;
use egui_winit::State;
//use egui_winit_platform::{Platform, PlatformDescriptor};

const INITIAL_WIDTH: u32 = 1920;
const INITIAL_HEIGHT: u32 = 1080;

pub static JVM: OnceCell<JavaVM> = OnceCell::new();

#[no_mangle]
pub extern "system" fn Java_io_geph_geph5_NativeBridge_setjvm(env: JNIEnv, _: JObject) {
    let jvm = env.get_java_vm().expect("Failed to get JVM");
    JVM.set(jvm).unwrap();
    // JVM.set(Mutex::new(jvm)).expect("Failed to set JVM");
    println!("SETTING JVM");
}

/// A custom event type for the winit app.
enum CustomEvent {
    RequestRedraw,
}

/// Enable egui to request redraws via a custom Winit event...
#[derive(Clone)]
struct RepaintSignal(
    std::sync::Arc<std::sync::Mutex<winit::event_loop::EventLoopProxy<CustomEvent>>>,
);

fn create_window<T>(
    event_loop: &EventLoopWindowTarget<T>,
    state: &mut State,
    painter: &mut Painter,
) -> Option<Arc<winit::window::Window>> {
    let window = Arc::new(
        winit::window::WindowBuilder::new()
            .with_decorations(true)
            .with_resizable(true)
            .with_transparent(false)
            .with_title("egui winit + wgpu example")
            .with_inner_size(winit::dpi::PhysicalSize {
                width: INITIAL_WIDTH,
                height: INITIAL_HEIGHT,
            })
            .build(event_loop)
            .unwrap(),
    );

    if let Err(err) = pollster::block_on(painter.set_window(ViewportId::ROOT, Some(window.clone())))
    {
        log::error!("Failed to associate new Window with Painter: {err:?}");
        return None;
    }

    // NB: calling set_window will lazily initialize render state which
    // means we will be able to query the maximum supported texture
    // dimensions
    if let Some(max_size) = painter.max_texture_side() {
        state.set_max_texture_side(max_size);
    }

    // let pixels_per_point = window.scale_factor() as f32;
    // state.set_pixels_per_point(pixels_per_point);

    window.request_redraw();

    Some(window)
}

fn _main(event_loop: EventLoop<CustomEvent>) {
    geph5_client_gui::SHOW_KEYBOARD_CALLBACK
        .set(Box::new(|b| {
            show_soft_input(b).unwrap();
        }))
        .ok()
        .unwrap();
    let ctx = egui::Context::default();
    let repaint_signal = RepaintSignal(std::sync::Arc::new(std::sync::Mutex::new(
        event_loop.create_proxy(),
    )));
    ctx.set_request_repaint_callback(move |_info| {
        log::debug!("Request Repaint Callback");
        repaint_signal
            .0
            .lock()
            .unwrap()
            .send_event(CustomEvent::RequestRedraw)
            .ok();
    });

    let mut state = State::new(
        ctx.clone(),
        ViewportId::ROOT,
        &event_loop.display_handle().unwrap(),
        None,
        None,
    );
    let mut painter = Painter::new(
        egui_wgpu::WgpuConfiguration {
            supported_backends: wgpu::Backends::all(),
            power_preference: wgpu::PowerPreference::LowPower,
            device_descriptor: std::sync::Arc::new(|_adapter| wgpu::DeviceDescriptor {
                label: None,
                required_features: Default::default(),
                required_limits: Default::default(),
            }),
            present_mode: wgpu::PresentMode::Fifo,
            ..Default::default()
        },
        1, // msaa samples
        Some(wgpu::TextureFormat::Depth24Plus),
        false,
    );
    let mut window: Option<Arc<winit::window::Window>> = None;

    let mut app = None;

    event_loop
        .run(move |event, event_loop| match event {
            Event::Resumed => match window.clone() {
                None => {
                    window = create_window(event_loop, &mut state, &mut painter);
                }
                Some(window) => {
                    pollster::block_on(painter.set_window(ViewportId::ROOT, Some(window.clone())))
                        .unwrap_or_else(|err| {
                            log::error!(
                            "Failed to associate window with painter after resume event: {err:?}"
                        )
                        });
                    window.request_redraw();
                }
            },
            Event::Suspended => {
                window = None;
            }
            Event::WindowEvent {
                window_id: _,
                event: WindowEvent::RedrawRequested,
            } => {
                if let Some(window) = window.as_ref() {
                    log::debug!("RedrawRequested, with window set");
                    let raw_input = state.take_egui_input(window);

                    log::debug!("RedrawRequested: calling ctx.run()");
                    let full_output = ctx.run(raw_input, |ctx| {
                        let app = app.get_or_insert_with(|| App::new(ctx));
                        app.render(ctx);
                    });
                    log::debug!("RedrawRequested: called ctx.run()");
                    state.handle_platform_output(window, full_output.platform_output);

                    log::debug!("RedrawRequested: calling paint_and_update_textures()");
                    painter.paint_and_update_textures(
                        ViewportId::ROOT,
                        state.egui_ctx().pixels_per_point(),
                        [0.0, 0.0, 0.0, 0.0],
                        &ctx.tessellate(full_output.shapes, state.egui_ctx().pixels_per_point()),
                        &full_output.textures_delta,
                        false, // capture
                    );

                    // if full_output.repaint_after.is_zero() {
                    window.request_redraw();
                    // }
                } else {
                    log::debug!("RedrawRequested, with no window set");
                }
            }

            Event::WindowEvent { event, .. } => {
                log::debug!("Window Event: {event:?}");
                match &event {
                    winit::event::WindowEvent::Resized(size) => {
                        painter.on_window_resized(
                            ViewportId::ROOT,
                            size.width.try_into().unwrap(),
                            size.height.try_into().unwrap(),
                        );
                    }
                    winit::event::WindowEvent::KeyboardInput { event, .. } => {
                        let evt = egui::Event::Key {
                            key: egui::Key::from_name(event.logical_key.to_text().unwrap())
                                .unwrap(),
                            physical_key: None,
                            pressed: event.state.is_pressed(),
                            repeat: event.repeat,
                            modifiers: Default::default(),
                        };
                        log::debug!("input key {:?}", evt);
                        state.egui_input_mut().events.push(evt);

                        window.as_ref().unwrap().request_redraw();
                    }
                    winit::event::WindowEvent::CloseRequested => {
                        todo!()
                    }
                    _ => {}
                }

                let response = state.on_window_event(window.as_ref().unwrap(), &event);
                if response.repaint {
                    if let Some(window) = window.as_ref() {
                        window.request_redraw();
                    }
                }
            }

            _ => (),
        })
        .unwrap();
}

#[allow(dead_code)]
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;

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

    let event_loop = EventLoopBuilder::with_user_event()
        .with_android_app(app)
        .build()
        .unwrap();
    _main(event_loop);
}

#[allow(dead_code)]
#[cfg(not(target_os = "android"))]
fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn) // Default Log Level
        .parse_default_env()
        .init();

    let event_loop = EventLoopBuilder::with_user_event().build().unwrap();
    _main(event_loop);
}
