use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{
    Action, AnyWindowHandle, BackgroundExecutor, Bounds, Capslock, ClipboardItem, CursorStyle,
    DevicePixels, DispatchEventResult, DisplayId, DummyKeyboardMapper, FileDialogFilter,
    ForegroundExecutor, GpuSpecs, Keymap, Menu, MenuItem, Modifiers, MouseButton, MouseUpEvent,
    PathPromptOptions, Pixels, Platform, PlatformAtlas, PlatformDisplay, PlatformDispatcher,
    PlatformInput, PlatformInputHandler, PlatformKeyboardLayout, PlatformKeyboardMapper,
    PlatformTextSystem, PlatformWindow, Point, Priority, PromptButton, PromptLevel,
    RequestFrameOptions, RunnableVariant, Scene, ScrollDelta, ScrollWheelEvent, Size, Task,
    ThermalState, WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea,
    WindowParams, point, px, size,
};
use gpui_wgpu::CosmicTextSystem;
use image::RgbaImage;
use parking_lot::Mutex;
use raw_window_handle::{HandleError, HasDisplayHandle, HasWindowHandle};
use uuid::Uuid;

use crate::{
    ButtonEvent, DeviceRefreshProfile, FbInkPresenter, FrameUpdate, Gesture, HardwareButton,
    KoboRenderer, KoboRuntime, PixelRect, RepaintScheduler, RuntimeEvent, TouchPhase,
    changed_pixel_bounds,
};

/// Configuration for GPUI's single fullscreen Kobo application runtime.
#[derive(Clone, Copy, Debug)]
pub struct KoboPlatformOptions {
    /// Logical GPUI window size. The default produces the proven 600x800 framebuffer at 2x.
    pub logical_size: Size<Pixels>,
    /// Logical-to-framebuffer scale factor.
    pub scale_factor: f32,
    /// Present frames to FBInk. Disable for host tests and PGM capture.
    pub display: bool,
    /// Read evdev input and stay in the event loop until quit or timeout.
    pub interactive: bool,
    /// Recovery deadline for the foreground application.
    pub timeout: Duration,
}

impl Default for KoboPlatformOptions {
    fn default() -> Self {
        Self {
            logical_size: size(px(300.0), px(400.0)),
            scale_factor: 2.0,
            display: true,
            interactive: true,
            timeout: Duration::from_secs(45),
        }
    }
}

struct KoboDispatcher {
    main_thread: ThreadId,
    main_tx: mpsc::Sender<RunnableVariant>,
    main_rx: Mutex<mpsc::Receiver<RunnableVariant>>,
    background_tx: mpsc::Sender<RunnableVariant>,
}

impl KoboDispatcher {
    fn new() -> Arc<Self> {
        let (main_tx, main_rx) = mpsc::channel();
        let (background_tx, background_rx) = mpsc::channel::<RunnableVariant>();
        let background_rx = Arc::new(Mutex::new(background_rx));
        for worker in 0..2 {
            let background_rx = background_rx.clone();
            thread::Builder::new()
                .name(format!("gpui-kobo-{worker}"))
                .spawn(move || loop {
                    let runnable = background_rx.lock().recv();
                    let Ok(runnable) = runnable else { break; };
                    runnable.run();
                })
                .expect("failed to start Kobo GPUI worker");
        }
        Arc::new(Self {
            main_thread: thread::current().id(),
            main_tx,
            main_rx: Mutex::new(main_rx),
            background_tx,
        })
    }

    fn run_pending(&self) -> usize {
        let mut completed = 0;
        while let Ok(runnable) = self.main_rx.lock().try_recv() {
            runnable.run();
            completed += 1;
        }
        completed
    }
}

impl PlatformDispatcher for KoboDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread
    }

    fn dispatch(&self, runnable: RunnableVariant, _priority: Priority) {
        let _ = self.background_tx.send(runnable);
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        let _ = self.main_tx.send(runnable);
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let tx = self.main_tx.clone();
        thread::spawn(move || {
            thread::sleep(duration);
            let _ = tx.send(runnable);
        });
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        thread::spawn(f);
    }
}

#[derive(Debug)]
struct KoboDisplay {
    bounds: Bounds<Pixels>,
}

impl PlatformDisplay for KoboDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(1)
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(Uuid::from_u128(0x4b4f424f_4750_5549_0000_000000000001))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}

struct KoboKeyboardLayout;

impl PlatformKeyboardLayout for KoboKeyboardLayout {
    fn id(&self) -> &str {
        "kobo-touch"
    }

    fn name(&self) -> &str {
        "Kobo Touch"
    }
}

struct KoboWindowState {
    handle: AnyWindowHandle,
    bounds: Bounds<Pixels>,
    scale_factor: f32,
    display: Rc<dyn PlatformDisplay>,
    renderer: KoboRenderer,
    title: String,
    mouse_position: Point<Pixels>,
    input_handler: Option<PlatformInputHandler>,
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    active_changed: Option<Box<dyn FnMut(bool)>>,
    hover_changed: Option<Box<dyn FnMut(bool)>>,
    resized: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved: Option<Box<dyn FnMut()>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    hit_test: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
    closed: Option<Box<dyn FnOnce()>>,
    previous_frame: Option<RgbaImage>,
    pending_update: Option<FrameUpdate>,
    render_error: Option<String>,
    render_count: u64,
}

/// GPUI's sole fullscreen Kobo window.
#[derive(Clone)]
pub struct KoboWindow(Rc<RefCell<KoboWindowState>>);

impl KoboWindow {
    fn new(
        handle: AnyWindowHandle,
        params: WindowParams,
        display: Rc<dyn PlatformDisplay>,
        scale_factor: f32,
    ) -> Self {
        Self(Rc::new(RefCell::new(KoboWindowState {
            handle,
            bounds: params.bounds,
            scale_factor,
            display,
            renderer: KoboRenderer::new(),
            title: String::new(),
            mouse_position: Point::default(),
            input_handler: None,
            request_frame: None,
            input: None,
            active_changed: None,
            hover_changed: None,
            resized: None,
            moved: None,
            should_close: None,
            hit_test: None,
            closed: None,
            previous_frame: None,
            pending_update: None,
            render_error: None,
            render_count: 0,
        })))
    }

    fn request_frame(&self, force: bool) {
        let callback = self.0.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            callback(RequestFrameOptions {
                require_presentation: true,
                force_render: force,
            });
            self.0.borrow_mut().request_frame = Some(callback);
        }
    }

    fn activate_callbacks(&self) {
        let active = self.0.borrow_mut().active_changed.take();
        if let Some(mut active) = active {
            active(true);
            self.0.borrow_mut().active_changed = Some(active);
        }
        let hovered = self.0.borrow_mut().hover_changed.take();
        if let Some(mut hovered) = hovered {
            hovered(true);
            self.0.borrow_mut().hover_changed = Some(hovered);
        }
    }

    fn close(&self) {
        let should_close = self.0.borrow_mut().should_close.take();
        let allowed = if let Some(mut should_close) = should_close {
            let allowed = should_close();
            self.0.borrow_mut().should_close = Some(should_close);
            allowed
        } else {
            true
        };
        if allowed {
            if let Some(closed) = self.0.borrow_mut().closed.take() {
                closed();
            }
        }
    }

    fn dispatch_input(&self, input: PlatformInput, position: Point<Pixels>) -> bool {
        let callback = {
            let mut state = self.0.borrow_mut();
            state.mouse_position = position;
            state.input.take()
        };
        let Some(mut callback) = callback else {
            return false;
        };
        let result = callback(input);
        self.0.borrow_mut().input = Some(callback);
        !result.propagate
    }

    fn take_update(&self) -> Option<FrameUpdate> {
        self.0.borrow_mut().pending_update.take()
    }

    fn last_frame(&self) -> Option<RgbaImage> {
        self.0.borrow().previous_frame.clone()
    }

    fn take_render_error(&self) -> Option<String> {
        self.0.borrow_mut().render_error.take()
    }

    fn render_count(&self) -> u64 {
        self.0.borrow().render_count
    }
}

impl HasWindowHandle for KoboWindow {
    fn window_handle(&self) -> std::result::Result<raw_window_handle::WindowHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

impl HasDisplayHandle for KoboWindow {
    fn display_handle(&self) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

impl PlatformWindow for KoboWindow {
    fn bounds(&self) -> Bounds<Pixels> { self.0.borrow().bounds }
    fn is_maximized(&self) -> bool { true }
    fn window_bounds(&self) -> WindowBounds { WindowBounds::Maximized(self.bounds()) }
    fn content_size(&self) -> Size<Pixels> { self.bounds().size }
    fn resize(&mut self, size: Size<Pixels>) { self.0.borrow_mut().bounds.size = size; }
    fn scale_factor(&self) -> f32 { self.0.borrow().scale_factor }
    fn appearance(&self) -> WindowAppearance { WindowAppearance::Light }
    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> { Some(self.0.borrow().display.clone()) }
    fn mouse_position(&self) -> Point<Pixels> { self.0.borrow().mouse_position }
    fn modifiers(&self) -> Modifiers { Modifiers::default() }
    fn capslock(&self) -> Capslock { Capslock::default() }
    fn set_input_handler(&mut self, handler: PlatformInputHandler) { self.0.borrow_mut().input_handler = Some(handler); }
    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> { self.0.borrow_mut().input_handler.take() }
    fn prompt(&self, _level: PromptLevel, _msg: &str, _detail: Option<&str>, _answers: &[PromptButton]) -> Option<oneshot::Receiver<usize>> { None }
    fn activate(&self) { self.activate_callbacks(); }
    fn is_active(&self) -> bool { true }
    fn is_hovered(&self) -> bool { true }
    fn background_appearance(&self) -> WindowBackgroundAppearance { WindowBackgroundAppearance::Opaque }
    fn set_title(&mut self, title: &str) { self.0.borrow_mut().title = title.to_owned(); }
    fn set_background_appearance(&self, _appearance: WindowBackgroundAppearance) {}
    fn minimize(&self) {}
    fn zoom(&self) {}
    fn toggle_fullscreen(&self) {}
    fn is_fullscreen(&self) -> bool { true }
    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) { self.0.borrow_mut().request_frame = Some(callback); }
    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) { self.0.borrow_mut().input = Some(callback); }
    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) { self.0.borrow_mut().active_changed = Some(callback); }
    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) { self.0.borrow_mut().hover_changed = Some(callback); }
    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) { self.0.borrow_mut().resized = Some(callback); }
    fn on_moved(&self, callback: Box<dyn FnMut()>) { self.0.borrow_mut().moved = Some(callback); }
    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) { self.0.borrow_mut().should_close = Some(callback); }
    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) { self.0.borrow_mut().hit_test = Some(callback); }
    fn on_close(&self, callback: Box<dyn FnOnce()>) { self.0.borrow_mut().closed = Some(callback); }
    fn on_appearance_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn draw(&self, scene: &Scene) {
        let mut state = self.0.borrow_mut();
        state.render_count = state.render_count.saturating_add(1);
        let device_size: Size<DevicePixels> = state.bounds.size.to_device_pixels(state.scale_factor);
        match state.renderer.render_scene_to_image(scene, device_size) {
            Ok(image) => {
                let damage = state.previous_frame.as_ref()
                    .and_then(|previous| changed_pixel_bounds(previous, &image))
                    .or_else(|| state.previous_frame.is_none().then_some(PixelRect {
                        x: 0,
                        y: 0,
                        width: image.width(),
                        height: image.height(),
                    }));
                state.previous_frame = Some(image.clone());
                state.pending_update = damage.map(|damage| FrameUpdate { image, damage });
            }
            Err(error) => state.render_error = Some(error.to_string()),
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> { self.0.borrow().renderer.sprite_atlas() }
    fn is_subpixel_rendering_supported(&self) -> bool { false }
    fn gpu_specs(&self) -> Option<GpuSpecs> { None }
    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}
}

#[derive(Default)]
struct TouchAccumulator {
    last_y: Option<f32>,
    scroll_y: f32,
}

/// Production, single-window GPUI platform for Kobo e-readers.
pub struct KoboPlatform {
    options: KoboPlatformOptions,
    dispatcher: Arc<KoboDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    display: Rc<dyn PlatformDisplay>,
    active_window: RefCell<Option<KoboWindow>>,
    quit: Cell<bool>,
    error: RefCell<Option<String>>,
    clipboard: Mutex<Option<ClipboardItem>>,
    presenter: RefCell<Option<FbInkPresenter>>,
    runtime: RefCell<Option<KoboRuntime>>,
    scheduler: RefCell<RepaintScheduler>,
    touch: RefCell<TouchAccumulator>,
    queued_events: RefCell<VecDeque<RuntimeEvent>>,
    quit_callback: RefCell<Option<Box<dyn FnMut()>>>,
    wake_callback: RefCell<Option<Box<dyn FnMut()>>>,
    wake_probe_requested: Cell<bool>,
}

impl KoboPlatform {
    /// Create the platform and, when requested, initialize FBInk and evdev before GPUI starts.
    pub fn new(options: KoboPlatformOptions) -> Result<Rc<Self>> {
        let dispatcher = KoboDispatcher::new();
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        let text_system = Arc::new(CosmicTextSystem::new_without_system_fonts("Lilex"));
        text_system.add_fonts(vec![
            std::borrow::Cow::Borrowed(include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf")),
            std::borrow::Cow::Borrowed(include_bytes!("../../../assets/fonts/lilex/Lilex-Bold.ttf")),
        ])?;

        let mut presenter = options.display.then(FbInkPresenter::open).transpose()?;
        let (runtime, profile) = if let Some(presenter) = presenter.as_ref() {
            let geometry = presenter.geometry();
            println!("display: {}", geometry.description());
            let runtime = options.interactive.then(|| KoboRuntime::open(geometry)).transpose()?;
            (runtime, DeviceRefreshProfile::for_device(&geometry.device_codename))
        } else {
            (None, DeviceRefreshProfile::default())
        };
        if !options.display {
            presenter = None;
        }

        let bounds = Bounds { origin: point(px(0.0), px(0.0)), size: options.logical_size };
        Ok(Rc::new(Self {
            options,
            dispatcher,
            background_executor,
            foreground_executor,
            text_system,
            display: Rc::new(KoboDisplay { bounds }),
            active_window: RefCell::new(None),
            quit: Cell::new(false),
            error: RefCell::new(None),
            clipboard: Mutex::new(None),
            presenter: RefCell::new(presenter),
            runtime: RefCell::new(runtime),
            scheduler: RefCell::new(RepaintScheduler::new(profile)),
            touch: RefCell::new(TouchAccumulator::default()),
            queued_events: RefCell::new(VecDeque::new()),
            quit_callback: RefCell::new(None),
            wake_callback: RefCell::new(None),
            wake_probe_requested: Cell::new(false),
        }))
    }

    /// Return the last CPU-rendered framebuffer, including in no-display mode.
    pub fn last_frame(&self) -> Option<RgbaImage> {
        self.active_window.borrow().as_ref().and_then(KoboWindow::last_frame)
    }

    /// Return and clear a backend error captured by GPUI's non-fallible platform callbacks.
    pub fn take_error(&self) -> Option<anyhow::Error> {
        self.error.borrow_mut().take().map(|message| anyhow!(message))
    }

    /// Queue a native event before `Application::run`, primarily for deterministic host checks.
    pub fn queue_event(&self, event: RuntimeEvent) {
        self.queued_events.borrow_mut().push_back(event);
    }

    /// Number of actual GPUI scene draws performed by the active window.
    pub fn render_count(&self) -> u64 {
        self.window().map_or(0, |window| window.render_count())
    }

    /// Whether GPUI or a Kobo lifecycle event requested application termination.
    pub fn quit_requested(&self) -> bool {
        self.quit.get()
    }

    fn fail(&self, error: impl std::fmt::Display) {
        *self.error.borrow_mut() = Some(error.to_string());
        self.quit.set(true);
    }

    fn window(&self) -> Option<KoboWindow> {
        self.active_window.borrow().clone()
    }

    fn request_and_present(&self, scrolling: bool, force: bool) -> Result<()> {
        let Some(window) = self.window() else { return Ok(()); };
        let pipeline_started = Instant::now();
        let render_started = Instant::now();
        window.request_frame(force);
        self.dispatcher.run_pending();
        let render_ms = render_started.elapsed().as_millis();
        if let Some(error) = window.take_render_error() {
            return Err(anyhow!(error));
        }
        let Some(update) = window.take_update() else {
            println!(
                "GPUI timing: force={force} scrolling={scrolling} render_ms={render_ms} changed=false total_ms={}",
                pipeline_started.elapsed().as_millis()
            );
            return Ok(());
        };
        if force {
            let present_started = Instant::now();
            if let Some(presenter) = self.presenter.borrow_mut().as_mut() {
                presenter.present_full(&update.image)?;
            }
            println!(
                "GPUI timing: force=true scrolling={scrolling} render_ms={render_ms} changed=true present_ms={} total_ms={}",
                present_started.elapsed().as_millis(),
                pipeline_started.elapsed().as_millis()
            );
            return Ok(());
        }
        let schedule_started = Instant::now();
        self.scheduler.borrow_mut().enqueue(update, scrolling);
        if let Some(refresh) = self.scheduler.borrow_mut().flush(Instant::now()) {
            let schedule_ms = schedule_started.elapsed().as_millis();
            println!("GPUI refresh: mode={:?} reason={:?} damage=x:{},y:{},w:{},h:{}",
                refresh.mode, refresh.reason, refresh.update.damage.x, refresh.update.damage.y,
                refresh.update.damage.width, refresh.update.damage.height);
            let present_started = Instant::now();
            if let Some(presenter) = self.presenter.borrow_mut().as_mut() {
                presenter.present_update(&refresh.update, refresh.mode)?;
            }
            println!(
                "GPUI timing: force=false scrolling={scrolling} render_ms={render_ms} changed=true schedule_ms={schedule_ms} present_ms={} total_ms={}",
                present_started.elapsed().as_millis(),
                pipeline_started.elapsed().as_millis()
            );
        }
        Ok(())
    }

    fn dispatch_scroll(&self, canvas_delta_y: f32) -> Result<()> {
        let Some(window) = self.window() else { return Ok(()); };
        let dispatch_started = Instant::now();
        let position = point(self.options.logical_size.width / 2.0, self.options.logical_size.height / 2.0);
        window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.0), px(canvas_delta_y / self.options.scale_factor))),
            touch_phase: gpui::TouchPhase::Ended,
            ..Default::default()
        }), position);
        let dispatch_ms = dispatch_started.elapsed().as_millis();
        let frame_started = Instant::now();
        let result = self.request_and_present(true, false);
        println!(
            "GPUI scroll timing: delta={canvas_delta_y:.1} dispatch_ms={dispatch_ms} frame_ms={} total_ms={}",
            frame_started.elapsed().as_millis(),
            dispatch_started.elapsed().as_millis()
        );
        result
    }

    fn dispatch_touch(&self, mapped: crate::MappedTouch, gesture: Option<Gesture>) -> Result<()> {
        let position = point(px(mapped.x / self.options.scale_factor), px(mapped.y / self.options.scale_factor));
        let logical_y = mapped.y / self.options.scale_factor;
        match mapped.phase {
            TouchPhase::Down => {
                *self.touch.borrow_mut() = TouchAccumulator { last_y: Some(logical_y), scroll_y: 0.0 };
                return Ok(());
            }
            TouchPhase::Move => {
                let mut touch = self.touch.borrow_mut();
                if let Some(last_y) = touch.last_y { touch.scroll_y += logical_y - last_y; }
                touch.last_y = Some(logical_y);
                return Ok(());
            }
            TouchPhase::Cancel => {
                *self.touch.borrow_mut() = TouchAccumulator::default();
                return Ok(());
            }
            TouchPhase::Up => {}
        }

        let mut touch = self.touch.borrow_mut();
        if let Some(last_y) = touch.last_y { touch.scroll_y += logical_y - last_y; }
        let scroll_y = touch.scroll_y;
        *touch = TouchAccumulator::default();
        drop(touch);
        let Some(window) = self.window() else { return Ok(()); };
        let scrolling = matches!(gesture, Some(Gesture::Swipe { .. })) || scroll_y.abs() >= 1.0;
        if scrolling {
            window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.0), px(scroll_y))),
                touch_phase: gpui::TouchPhase::Ended,
                ..Default::default()
            }), position);
        } else {
            let button = if matches!(gesture, Some(Gesture::LongPress { .. })) { MouseButton::Right } else { MouseButton::Left };
            window.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                button,
                position,
                click_count: 1,
                ..Default::default()
            }), position);
        }
        if self.quit.get() { Ok(()) } else { self.request_and_present(scrolling, false) }
    }

    fn dispatch_button(&self, event: ButtonEvent) -> Result<()> {
        let button_started = Instant::now();
        println!("hardware button received: {:?} pressed={} repeated={}", event.button, event.pressed, event.repeated);
        if matches!(event.button, HardwareButton::Power | HardwareButton::Sleep) && !event.pressed {
            self.wake_probe_requested.set(true);
        }
        if !event.pressed || event.repeated { return Ok(()); }
        let result = match event.button {
            HardwareButton::PreviousPage => self.dispatch_scroll(300.0),
            HardwareButton::NextPage => self.dispatch_scroll(-300.0),
            HardwareButton::Home => {
                let callback = self.quit_callback.borrow_mut().take();
                if let Some(mut callback) = callback {
                    callback();
                    *self.quit_callback.borrow_mut() = Some(callback);
                }
                self.quit.set(true);
                Ok(())
            }
            HardwareButton::Power | HardwareButton::Sleep | HardwareButton::Unknown(_) => Ok(()),
        };
        println!(
            "hardware button complete: {:?} total_ms={}",
            event.button,
            button_started.elapsed().as_millis()
        );
        result
    }

    fn process_event(&self, event: RuntimeEvent) -> Result<()> {
        match event {
            RuntimeEvent::Touch { mapped, gesture } => {
                if let Some(gesture) = gesture { println!("input gesture: {gesture:?}"); }
                self.dispatch_touch(mapped, gesture)
            }
            RuntimeEvent::Button(event) => self.dispatch_button(event),
            RuntimeEvent::Idle => {
                if let Some(refresh) = self.scheduler.borrow_mut().idle_cleanup(Instant::now()) {
                    if let Some(presenter) = self.presenter.borrow_mut().as_mut() {
                        presenter.present_update(&refresh.update, refresh.mode)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn run_event_loop(&self) -> Result<()> {
        self.request_and_present(false, true)?;
        loop {
            let event = self.queued_events.borrow_mut().pop_front();
            let Some(event) = event else { break; };
            self.process_event(event)?;
        }
        if !self.options.interactive || self.quit.get() { return Ok(()); }
        let deadline = Instant::now() + self.options.timeout;
        let mut next_display_probe = Instant::now() + Duration::from_secs(1);
        while !self.quit.get() {
            if self.dispatcher.run_pending() > 0 {
                self.request_and_present(false, false)?;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { println!("interactive timeout reached"); break; }
            let event = self.runtime.borrow_mut().as_mut()
                .ok_or_else(|| anyhow!("interactive Kobo platform has no input runtime"))?
                .next_event(remaining.min(Duration::from_millis(50)))?;
            self.process_event(event)?;
            if self.wake_probe_requested.replace(false) || Instant::now() >= next_display_probe {
                let changed = if let Some(presenter) = self.presenter.borrow_mut().as_mut() {
                    presenter.reinitialize()?
                } else {
                    false
                };
                if changed {
                    if let (Some(presenter), Some(runtime)) = (self.presenter.borrow().as_ref(), self.runtime.borrow_mut().as_mut()) {
                        runtime.update_geometry(presenter.geometry());
                    }
                    let callback = self.wake_callback.borrow_mut().take();
                    if let Some(mut callback) = callback {
                        callback();
                        *self.wake_callback.borrow_mut() = Some(callback);
                    }
                    if let (Some(frame), Some(presenter)) = (self.last_frame(), self.presenter.borrow_mut().as_mut()) {
                        presenter.present_full(&frame)?;
                    }
                }
                next_display_probe = Instant::now() + Duration::from_secs(1);
            }
        }
        if let Some(window) = self.window() {
            window.close();
        }
        Ok(())
    }
}

impl Platform for KoboPlatform {
    fn background_executor(&self) -> BackgroundExecutor { self.background_executor.clone() }
    fn foreground_executor(&self) -> ForegroundExecutor { self.foreground_executor.clone() }
    fn text_system(&self) -> Arc<dyn PlatformTextSystem> { self.text_system.clone() }
    fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
        on_finish_launching();
        if let Err(error) = self.run_event_loop() { self.fail(error); }
    }
    fn quit(&self) { self.quit.set(true); }
    fn restart(&self, _binary_path: Option<PathBuf>) { self.quit(); }
    fn activate(&self, _ignoring_other_apps: bool) {}
    fn hide(&self) {}
    fn hide_other_apps(&self) {}
    fn unhide_other_apps(&self) {}
    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> { vec![self.display.clone()] }
    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> { Some(self.display.clone()) }
    fn active_window(&self) -> Option<AnyWindowHandle> { self.window().map(|window| window.0.borrow().handle) }
    fn open_window(&self, handle: AnyWindowHandle, mut params: WindowParams) -> Result<Box<dyn PlatformWindow>> {
        if self.active_window.borrow().is_some() {
            return Err(anyhow!("Kobo supports exactly one fullscreen GPUI window"));
        }
        params.bounds = self.display.bounds();
        let window = KoboWindow::new(handle, params, self.display.clone(), self.options.scale_factor);
        *self.active_window.borrow_mut() = Some(window.clone());
        Ok(Box::new(window))
    }
    fn window_appearance(&self) -> WindowAppearance { WindowAppearance::Light }
    fn open_url(&self, url: &str) { eprintln!("Kobo cannot open URL: {url}"); }
    fn on_open_urls(&self, _callback: Box<dyn FnMut(Vec<String>)>) {}
    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> { Task::ready(Ok(())) }
    fn prompt_for_paths(&self, _options: PathPromptOptions, _filters: Vec<FileDialogFilter>) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel(); let _ = tx.send(Ok(None)); rx
    }
    fn prompt_for_new_path(&self, _directory: &Path, _suggested_name: Option<&str>, _filters: Vec<FileDialogFilter>) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel(); let _ = tx.send(Ok(None)); rx
    }
    fn can_select_mixed_files_and_dirs(&self) -> bool { false }
    fn reveal_path(&self, _path: &Path) {}
    fn open_with_system(&self, _path: &Path) {}
    fn on_quit(&self, callback: Box<dyn FnMut()>) { *self.quit_callback.borrow_mut() = Some(callback); }
    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {}
    fn on_system_wake(&self, callback: Box<dyn FnMut()>) { *self.wake_callback.borrow_mut() = Some(callback); }
    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}
    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}
    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}
    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}
    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}
    fn thermal_state(&self) -> ThermalState { ThermalState::Nominal }
    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}
    fn compositor_name(&self) -> &'static str { "FBInk" }
    fn app_path(&self) -> Result<PathBuf> { Ok(std::env::current_exe()?) }
    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        Ok(std::env::current_exe()?.parent().ok_or_else(|| anyhow!("executable has no parent"))?.join(name))
    }
    fn set_cursor_style(&self, _style: CursorStyle) {}
    fn hide_cursor_until_mouse_moves(&self) {}
    fn is_cursor_visible(&self) -> bool { false }
    fn should_auto_hide_scrollbars(&self) -> bool { true }
    fn read_from_clipboard(&self) -> Option<ClipboardItem> { self.clipboard.lock().clone() }
    fn write_to_clipboard(&self, item: ClipboardItem) { *self.clipboard.lock() = Some(item); }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn read_from_primary(&self) -> Option<ClipboardItem> { self.read_from_clipboard() }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn write_to_primary(&self, item: ClipboardItem) { self.write_to_clipboard(item); }
    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> { Task::ready(Ok(())) }
    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> { Task::ready(Ok(None)) }
    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> { Task::ready(Ok(())) }
    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> { Box::new(KoboKeyboardLayout) }
    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> { Rc::new(DummyKeyboardMapper) }
    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}
