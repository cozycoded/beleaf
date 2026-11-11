//! beleaf native: the Android app. This file is the Android side: the window, the S Pen and finger
//! input (with every in-between pen sample), and the app's life (pause, resume, rotate, close).

mod app;
mod doc;
mod gfx;

#[cfg(target_os = "android")]
mod android {
    use crate::app::{App, Files, PEv, Ptr, Tool};
    use crate::gfx::Gfx;
    use android_activity::input::{Axis, InputEvent, KeyAction, Keycode, MotionAction, ToolType};
    use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
    use raw_window_handle::{AndroidDisplayHandle, AndroidNdkWindowHandle, DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle, WindowHandle};
    use std::sync::Arc;
    use std::time::Duration;

    /// The Android window, in the shape the graphics library wants.
    struct Win(android_activity::ndk::native_window::NativeWindow);
    unsafe impl Send for Win {}
    unsafe impl Sync for Win {}
    impl HasWindowHandle for Win {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let h = AndroidNdkWindowHandle::new(self.0.ptr().cast());
            Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::AndroidNdk(h)) })
        }
    }
    impl HasDisplayHandle for Win {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Android(AndroidDisplayHandle::new())) })
        }
    }

    struct Gpu {
        instance: wgpu::Instance,
        adapter: Option<wgpu::Adapter>,
        surface: Option<(wgpu::Surface<'static>, wgpu::SurfaceConfiguration)>,
    }

    fn now_ms() -> f64 {
        use std::sync::OnceLock;
        static T0: OnceLock<std::time::Instant> = OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
    }

    fn make_surface(gpu: &mut Gpu, appx: &mut App, win: android_activity::ndk::native_window::NativeWindow) {
        let (w, h) = (win.width().max(1) as u32, win.height().max(1) as u32);
        let surface = match gpu.instance.create_surface(Arc::new(Win(win))) {
            Ok(s) => s,
            Err(e) => {
                log::error!("no surface: {e}");
                return;
            }
        };
        if gpu.adapter.is_none() {
            let adapter = pollster::block_on(gpu.instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: Some(&surface), force_fallback_adapter: false }));
            let Some(adapter) = adapter else {
                log::error!("no graphics adapter");
                return;
            };
            log::info!("graphics: {:?}", adapter.get_info());
            let limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
            let (device, queue) = match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: None, required_features: wgpu::Features::empty(), required_limits: limits, memory_hints: wgpu::MemoryHints::Performance }, None)) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("no device: {e}");
                    return;
                }
            };
            let caps = surface.get_capabilities(&adapter);
            let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
            appx.surface_format = format;
            appx.renderer = Some(egui_wgpu::Renderer::new(&device, format, None, 1, false));
            appx.gfx = Some(Gfx::new(device, queue, format, 2048, 2048, &crate::app::tip_images()));
            // pick up where we left off
            let p = appx.autosave_path();
            if p.exists() {
                appx.open_path(&p);
            }
            if appx.gfx.as_ref().map(|g| g.layers.is_empty()).unwrap_or(true) {
                appx.new_doc(2048, 2048);
            } else {
                appx.say("Welcome back");
            }
            gpu.adapter = Some(adapter);
        }
        let adapter = gpu.adapter.as_ref().unwrap();
        let caps = surface.get_capabilities(adapter);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) { wgpu::PresentMode::Mailbox } else { wgpu::PresentMode::Fifo };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: appx.surface_format,
            width: w,
            height: h,
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&appx.gfx.as_ref().unwrap().device, &config);
        appx.screen = (w, h);
        appx.dirty = true;
        gpu.surface = Some((surface, config));
    }

    /// Keep the controls out from under the phone's own bars (navigation buttons, gesture bar).
    fn update_safe(app: &AndroidApp, appx: &mut App) {
        let r = app.content_rect();
        let (w, h) = (appx.screen.0 as f32, appx.screen.1 as f32);
        let mut s = if r.right > r.left && r.bottom > r.top { [r.left as f32, r.top as f32, r.right as f32, r.bottom as f32] } else { [0.0, 0.0, w, h] };
        // if Android says the whole screen is ours, the navigation bar still sits on top at the bottom: leave room for it
        if s[3] >= h - 1.0 {
            s[3] = h - 48.0 * appx.ppp;
        }
        if s != appx.safe {
            appx.safe = s;
            appx.dirty = true;
        }
    }

    fn resize(gpu: &mut Gpu, appx: &mut App, w: u32, h: u32) {
        if let (Some((s, c)), Some(g)) = (gpu.surface.as_mut(), appx.gfx.as_ref()) {
            if (c.width, c.height) != (w, h) && w > 0 && h > 0 {
                c.width = w;
                c.height = h;
                s.configure(&g.device, c);
                // keep the same spot of the canvas in the middle when the phone turns
                let (ow, oh) = appx.screen;
                appx.view.tx += (w as f32 - ow as f32) / 2.0;
                appx.view.ty += (h as f32 - oh as f32) / 2.0;
                appx.screen = (w, h);
                appx.dirty = true;
            }
        }
    }

    fn draw(gpu: &mut Gpu, appx: &mut App) {
        let Some((surface, config)) = gpu.surface.as_ref() else { return };
        if appx.gfx.is_none() {
            return;
        }
        let frame = match surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => {
                surface.configure(&appx.gfx.as_ref().unwrap().device, config);
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let cmds = appx.frame(&view);
        appx.gfx.as_ref().unwrap().queue.submit(cmds);
        frame.present();
    }

    fn tool_of(t: ToolType) -> Tool {
        match t {
            ToolType::Stylus => Tool::Pen,
            ToolType::Eraser => Tool::PenEraser,
            ToolType::Mouse => Tool::Mouse,
            _ => Tool::Finger,
        }
    }
    /// Android's tilt is the angle from upright in radians; the brushes want 0 (flat) .. 1 (upright).
    fn tilt01(t: f32) -> f32 {
        (1.0 - t / std::f32::consts::FRAC_PI_2).clamp(0.0, 1.0)
    }

    fn handle_input(app: &AndroidApp, appx: &mut App) {
        let mut evs: Vec<PEv> = Vec::new();
        let mut back = false;
        if let Ok(mut it) = app.input_events_iter() {
            loop {
                let more = it.next(|ev| match ev {
                    InputEvent::MotionEvent(me) => {
                        let button = me.button_state().stylus_primary();
                        let t_ev = me.event_time() as f64 / 1.0e6;
                        let mk = |p: &android_activity::input::Pointer, t: f64| {
                            let tool = tool_of(p.tool_type());
                            let pen = tool == Tool::Pen || tool == Tool::PenEraser;
                            Ptr {
                                id: p.pointer_id(),
                                x: p.x(),
                                y: p.y(),
                                pressure: if pen { p.pressure() } else { 0.6 },
                                tilt: if pen { Some(tilt01(p.axis_value(Axis::Tilt))) } else { None },
                                az: if pen { Some(p.axis_value(Axis::Orientation)) } else { None },
                                tool,
                                t_ms: t,
                                button,
                            }
                        };
                        match me.action() {
                            MotionAction::Down | MotionAction::PointerDown => {
                                let p = me.pointer_at_index(me.pointer_index());
                                evs.push(PEv::Down(mk(&p, t_ev)));
                            }
                            MotionAction::Up | MotionAction::PointerUp => {
                                let p = me.pointer_at_index(me.pointer_index());
                                evs.push(PEv::Up(mk(&p, t_ev)));
                            }
                            MotionAction::Move => {
                                for p in me.pointers() {
                                    // every pen sample since the last frame, then the newest one
                                    let tool = tool_of(p.tool_type());
                                    let pen = tool == Tool::Pen || tool == Tool::PenEraser;
                                    let mut v: Vec<Ptr> = p
                                        .history()
                                        .map(|h| Ptr {
                                            id: p.pointer_id(),
                                            x: h.x(),
                                            y: h.y(),
                                            pressure: if pen { h.pressure() } else { 0.6 },
                                            tilt: if pen { Some(tilt01(h.axis_value(Axis::Tilt))) } else { None },
                                            az: if pen { Some(h.axis_value(Axis::Orientation)) } else { None },
                                            tool,
                                            t_ms: h.event_time() as f64 / 1.0e6,
                                            button,
                                        })
                                        .collect();
                                    v.push(mk(&p, t_ev));
                                    evs.push(PEv::Move(v));
                                }
                            }
                            MotionAction::HoverMove | MotionAction::HoverEnter => {
                                let p = me.pointer_at_index(0);
                                evs.push(PEv::Hover(mk(&p, t_ev)));
                            }
                            MotionAction::HoverExit => evs.push(PEv::HoverEnd),
                            MotionAction::Cancel => evs.push(PEv::Cancel),
                            _ => {}
                        }
                        InputStatus::Handled
                    }
                    InputEvent::KeyEvent(ke) => {
                        if ke.key_code() == Keycode::Back {
                            if ke.action() == KeyAction::Up {
                                back = true;
                            }
                            InputStatus::Handled
                        } else {
                            InputStatus::Unhandled
                        }
                    }
                    _ => InputStatus::Unhandled,
                });
                if !more {
                    break;
                }
            }
        }
        for e in evs {
            appx.input(e);
        }
        if back {
            appx.back();
        }
    }

    #[no_mangle]
    fn android_main(app: AndroidApp) {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("beleaf"));
        let internal = app.internal_data_path().unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp"));
        let files = Files { internal, shared: std::path::PathBuf::from("/storage/emulated/0/Download/beleaf") };
        let mut appx = App::new(files);
        if let Some(d) = app.config().density() {
            appx.ppp = (d as f32 / 160.0).max(1.0);
        }
        let mut gpu = Gpu { instance: wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN | wgpu::Backends::GL, ..Default::default() }), adapter: None, surface: None };
        let mut quit = false;
        let mut last_autosave = now_ms();
        while !quit {
            let timeout = if appx.dirty { Some(Duration::ZERO) } else { Some(Duration::from_millis(500)) };
            let mut need_input = false;
            let mut save_now = false;
            app.poll_events(timeout, |ev| {
                if let PollEvent::Main(m) = ev {
                    match m {
                        MainEvent::InitWindow { .. } => {
                            if let Some(w) = app.native_window() {
                                make_surface(&mut gpu, &mut appx, w);
                                update_safe(&app, &mut appx);
                                appx.fit_view();
                            }
                        }
                        MainEvent::TerminateWindow { .. } => {
                            gpu.surface = None;
                            save_now = true;
                        }
                        MainEvent::WindowResized { .. } | MainEvent::ContentRectChanged { .. } | MainEvent::ConfigChanged { .. } => {
                            if let Some(w) = app.native_window() {
                                resize(&mut gpu, &mut appx, w.width() as u32, w.height() as u32);
                            }
                            update_safe(&app, &mut appx);
                        }
                        MainEvent::RedrawNeeded { .. } => appx.dirty = true,
                        MainEvent::InputAvailable => need_input = true,
                        MainEvent::Pause | MainEvent::SaveState { .. } | MainEvent::Stop => save_now = true,
                        MainEvent::Destroy => {
                            save_now = true;
                            quit = true;
                        }
                        _ => {}
                    }
                }
            });
            if need_input || appx.gfx.is_some() {
                handle_input(&app, &mut appx);
            }
            if appx.want_save {
                appx.want_save = false;
                appx.save_shared();
            }
            // keep a copy safe: when the app goes to the background, and every couple of minutes after a pause in drawing
            let idle = now_ms() - appx.last_change > 4000.0;
            if appx.gfx.is_some() && appx.changed && (save_now || (idle && now_ms() - last_autosave > 120_000.0)) {
                let p = appx.autosave_path();
                appx.save_to(p, true);
                last_autosave = now_ms();
            }
            if appx.dirty && gpu.surface.is_some() {
                appx.dirty = false;
                draw(&mut gpu, &mut appx);
            }
        }
    }
}
