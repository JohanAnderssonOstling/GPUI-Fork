use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{
    Action, AnyWindowHandle, BackgroundExecutor, Bounds, Capslock, ClipboardItem, CursorStyle,
    DevicePixels, DispatchEventResult, DisplayId, DummyKeyboardMapper, FileDialogFilter,
    ForegroundExecutor, GpuSpecs, KeyDownEvent, KeyLocation, Keymap, Keystroke, Menu, MenuItem,
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, PathPromptOptions, Pixels, Platform,
    PlatformAtlas, PlatformDispatcher, PlatformDisplay, PlatformInput, PlatformInputHandler,
    PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, Point,
    Priority, PromptButton, PromptLevel, RequestFrameOptions, RunnableVariant, Scene, ScrollDelta,
    ScrollWheelEvent, Size, Task, ThermalState, TouchEvent, TouchId, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams, point, px, size,
};
use gpui_wgpu::CosmicTextSystem;
use image::{GrayImage, Luma};
use parking_lot::{Condvar, Mutex};
use raw_window_handle::{HandleError, HasDisplayHandle, HasWindowHandle};
use uuid::Uuid;

use crate::{
    ButtonEvent, DeviceRefreshProfile, FbInkPresenter, FrameUpdate,
    Gesture, HardwareButton, KoboRenderMode, KoboRenderer, KoboRuntime, PixelRect, RefreshMode,
    RepaintScheduler, RuntimeEvent, TouchPhase, changed_pixel_bounds_in, render_profiling_enabled,
};

thread_local! {
    static ACTIVE_KOBO_PLATFORM: Cell<Weak<KoboPlatform>> = Cell::new(Weak::new());
}

const RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct ProcessMemorySample {
    rss_kib: u64,
    peak_rss_kib: u64,
    virtual_kib: u64,
}

struct KoboResourceMonitor {
    next_sample_at: Instant,
    previous_cpu: Option<(Instant, u64)>,
    clock_ticks_per_second: f64,
}

struct KoboRunState {
    presenter: Option<FbInkPresenter>,
    runtime: Option<KoboRuntime>,
    scheduler: RepaintScheduler,
    resource_monitor: KoboResourceMonitor,
    touch: TouchAccumulator,
    suppress_next_power_release: bool,
    wake_frame_pending: bool,
}

impl KoboResourceMonitor {
    fn new(now: Instant) -> Self {
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        Self {
            next_sample_at: now,
            previous_cpu: read_process_cpu_ticks().map(|cpu| (now, cpu)),
            clock_ticks_per_second: if ticks > 0 { ticks as f64 } else { 100.0 },
        }
    }

    fn sample_if_due(&mut self, now: Instant) {
        if now < self.next_sample_at {
            return;
        }
        self.next_sample_at = now + RESOURCE_SAMPLE_INTERVAL;

        let cpu_ticks = read_process_cpu_ticks();
        let cpu_percent = match (self.previous_cpu, cpu_ticks) {
            (Some((previous_at, previous_ticks)), Some(current_ticks)) => {
                let elapsed = now.saturating_duration_since(previous_at).as_secs_f64();
                if elapsed > 0.0 {
                    current_ticks.saturating_sub(previous_ticks) as f64
                        / self.clock_ticks_per_second
                        / elapsed
                        * 100.0
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        if let Some(cpu_ticks) = cpu_ticks {
            self.previous_cpu = Some((now, cpu_ticks));
        }

        let memory = read_process_memory().unwrap_or(ProcessMemorySample {
            rss_kib: 0,
            peak_rss_kib: 0,
            virtual_kib: 0,
        });
        println!(
            "KOBO_RUNTIME_RESOURCE cpu_percent={cpu_percent:.1} rss_kib={} peak_rss_kib={} virtual_kib={}",
            memory.rss_kib, memory.peak_rss_kib, memory.virtual_kib,
        );
    }
}

fn read_process_cpu_ticks() -> Option<u64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let fields = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let user_ticks = fields.get(11)?.parse::<u64>().ok()?;
    let system_ticks = fields.get(12)?.parse::<u64>().ok()?;
    Some(user_ticks.saturating_add(system_ticks))
}

fn read_process_memory() -> Option<ProcessMemorySample> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let value = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    Some(ProcessMemorySample {
        rss_kib: value("VmRSS:"),
        peak_rss_kib: value("VmHWM:"),
        virtual_kib: value("VmSize:"),
    })
}

fn active_kobo_platform() -> Result<Rc<KoboPlatform>> {
    ACTIVE_KOBO_PLATFORM.with(|active| {
        let weak = active.take();
        let platform = weak.upgrade();
        active.set(weak);
        platform.ok_or_else(|| anyhow!("Kobo platform is not active"))
    })
}

pub fn begin_application_startup_profile(started_at: Instant) -> Result<()> {
    active_kobo_platform()?
        .application_startup_started_at
        .set(Some(started_at));
    Ok(())
}

pub fn begin_book_launch_profile(started_at: Instant) -> Result<()> {
    active_kobo_platform()?
        .book_launch_started_at
        .set(Some(started_at));
    Ok(())
}

/// Queue a complete GC16 refresh of the active Kobo window. The request is
/// consumed by the platform event loop after the current GPUI callback ends.
pub fn request_full_repaint() -> Result<()> {
    let platform = active_kobo_platform()?;
    platform.full_repaint_requested.set(true);
    platform.full_gc16_requested.set(true);
    Ok(())
}

thread_local! {
    static POWER_MENU_PENDING: Cell<bool> = const { Cell::new(false) };
    static SLEEP_PENDING: Cell<bool> = const { Cell::new(false) };
}

pub fn take_power_menu_request() -> bool { POWER_MENU_PENDING.with(|pending| pending.replace(false)) }
pub fn request_sleep() { SLEEP_PENDING.with(|pending| pending.set(true)); }

pub fn current_render_mode() -> Result<KoboRenderMode> {
    let platform = active_kobo_platform()?;
    if let Some(experience_mode) = platform.requested_experience_mode.get() {
        return Ok(experience_mode.render_mode());
    }
    Ok(platform
        .requested_render_mode
        .get()
        .unwrap_or_else(|| platform.render_mode.get()))
}

pub fn request_render_mode(render_mode: KoboRenderMode) -> Result<()> {
    let platform = active_kobo_platform()?;
    if platform.render_mode.get() != render_mode {
        platform.requested_render_mode.set(Some(render_mode));
        platform.full_repaint_requested.set(true);
    }
    Ok(())
}

/// Mark the next rendered frames as a reader page turn. The document below
/// `content_top` and changed chrome above it use the clean monochrome DU
/// waveform, while remaining separate damage regions.
pub fn request_reader_page_repaint(content_top: Pixels) -> Result<()> {
    let platform = active_kobo_platform()?;
    platform.reader_page_content_top.set(Some(content_top));
    platform
        .reader_page_fast_until
        .set(Some(Instant::now() + Duration::from_millis(750)));
    Ok(())
}

pub fn request_auto_rotation(enabled: bool) -> Result<()> {
    active_kobo_platform()?.set_auto_rotation(enabled);
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KoboExperienceMode {
    LibraryFast,
    ReaderQuality,
}

impl KoboExperienceMode {
    fn render_mode(self) -> KoboRenderMode {
        match self {
            Self::LibraryFast => KoboRenderMode::QualityGrayscale,
            Self::ReaderQuality => KoboRenderMode::QualityGrayscale,
        }
    }

    fn scale_factor(self, native_scale_factor: f32) -> f32 {
        match self {
            Self::LibraryFast => native_scale_factor,
            Self::ReaderQuality => native_scale_factor,
        }
    }
}

pub fn current_experience_mode() -> Result<KoboExperienceMode> {
    let platform = active_kobo_platform()?;
    Ok(platform
        .requested_experience_mode
        .get()
        .unwrap_or_else(|| platform.experience_mode.get()))
}

pub fn request_experience_mode(experience_mode: KoboExperienceMode) -> Result<()> {
    let platform = active_kobo_platform()?;
    if platform.experience_mode.get() != experience_mode {
        platform
            .requested_experience_mode
            .set(Some(experience_mode));
        platform.requested_render_mode.set(None);
        platform.full_repaint_requested.set(true);
        platform.full_gc16_requested.set(true);
    }
    Ok(())
}

fn inferred_logical_size(geometry: &crate::ScreenGeometry) -> Size<Pixels> {
    let divisor = if geometry.view_width % 2 == 0 && geometry.view_height % 2 == 0 {
        2
    } else {
        1
    };
    size(
        px((geometry.view_width / divisor).max(1) as f32),
        px((geometry.view_height / divisor).max(1) as f32),
    )
}

fn display_bounds(size: Size<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: point(px(0.0), px(0.0)),
        size,
    }
}

fn canvas_dimensions(size: Size<Pixels>, scale_factor: f32) -> (u32, u32) {
    let canvas_size: Size<DevicePixels> = size.to_device_pixels(scale_factor);
    (
        canvas_size.width.0.max(1) as u32,
        canvas_size.height.0.max(1) as u32,
    )
}

/// Configuration for GPUI's single fullscreen Kobo application runtime.
#[derive(Clone, Copy, Debug)]
pub struct KoboPlatformOptions {
    /// Logical GPUI window size. The default produces the proven 600x800 framebuffer at 2x.
    pub logical_size: Size<Pixels>,
    /// Logical-to-framebuffer scale factor.
    pub scale_factor: f32,
    /// Honor reported Kobo display geometry rotation changes automatically.
    pub auto_rotate: bool,
    /// Present frames to FBInk. Disable for host tests and PGM capture.
    pub display: bool,
    /// Read evdev input and stay in the event loop until quit or timeout.
    pub interactive: bool,
    /// Recovery deadline for the foreground application.
    pub timeout: Duration,
    /// How physical page buttons are exposed to the shared GPUI application.
    pub page_buttons: PageButtonBehavior,
    /// Application-owned Kobo suspend operation invoked on power-button release.
    pub power_button_handler: Option<fn() -> std::result::Result<(), String>>,
    pub power_button_opens_menu: bool,
    /// Called after the restored application frame is presented following suspend.
    /// Must only enqueue background work, without blocking the event loop.
    pub wake_ui_ready_handler: Option<fn()>,
    /// Draw and handle a persistent control that exits to the Kobo shell.
    pub show_exit_button: bool,
    pub experience_mode: KoboExperienceMode,
    pub render_mode: KoboRenderMode,
    pub monochrome_threshold: u8,
}

/// Input behavior for Kobo's physical page-turn buttons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageButtonBehavior {
    /// Emit a scroll gesture, useful for generic scrolling views and the smoke test.
    Scroll,
    /// Emit left/right keystrokes, reusing the desktop reader's existing key bindings.
    ///
    /// Note these move a *selection* in list and grid views rather than turning
    /// a page: the library browser binds left/right to its card cursor.
    ArrowKeys,
    /// Emit pageup/pagedown keystrokes.
    ///
    /// The reader binds these alongside the arrow keys, so it behaves
    /// identically either way, while views that paginate get a page turn
    /// instead of a cursor step. This matches what Android's volume buttons
    /// already dispatch.
    PageKeys,
}

impl Default for KoboPlatformOptions {
    fn default() -> Self {
        Self {
            logical_size: size(px(300.0), px(400.0)),
            scale_factor: 2.0,
            auto_rotate: true,
            display: true,
            interactive: true,
            timeout: Duration::from_secs(45),
            page_buttons: PageButtonBehavior::Scroll,
            power_button_handler: None,
            power_button_opens_menu: false,
            wake_ui_ready_handler: None,
            show_exit_button: false,
            experience_mode: KoboExperienceMode::ReaderQuality,
            render_mode: KoboRenderMode::QualityGrayscale,
            monochrome_threshold: 160,
        }
    }
}

struct KoboDispatcher {
    main_thread: ThreadId,
    main_tx: mpsc::Sender<RunnableVariant>,
    main_rx: Mutex<mpsc::Receiver<RunnableVariant>>,
    background_tx: mpsc::Sender<RunnableVariant>,
    timers: Arc<KoboTimerQueue>,
    timer_thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct KoboTimerQueue {
    state: Mutex<KoboTimerState>,
    changed: Condvar,
}

#[derive(Default)]
struct KoboTimerState {
    tasks: BinaryHeap<KoboTimerEntry>,
    next_sequence: u64,
    shutdown: bool,
}

struct KoboTimerEntry {
    deadline: Instant,
    sequence: u64,
    runnable: RunnableVariant,
}

impl PartialEq for KoboTimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.sequence == other.sequence
    }
}

impl Eq for KoboTimerEntry {}

impl PartialOrd for KoboTimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for KoboTimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl KoboTimerQueue {
    fn schedule(&self, duration: Duration, runnable: RunnableVariant) {
        let mut state = self.state.lock();
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state.tasks.push(KoboTimerEntry {
            deadline: Instant::now() + duration,
            sequence,
            runnable,
        });
        self.changed.notify_one();
    }

    fn shutdown(&self) {
        self.state.lock().shutdown = true;
        self.changed.notify_one();
    }

    fn run(&self, main_tx: mpsc::Sender<RunnableVariant>) {
        let mut state = self.state.lock();
        loop {
            if state.shutdown {
                return;
            }
            let Some(deadline) = state.tasks.peek().map(|entry| entry.deadline) else {
                self.changed.wait(&mut state);
                continue;
            };
            let now = Instant::now();
            if deadline > now {
                self.changed.wait_for(&mut state, deadline - now);
                continue;
            }
            let entry = state.tasks.pop().expect("timer entry was present");
            drop(state);
            if main_tx.send(entry.runnable).is_err() {
                return;
            }
            state = self.state.lock();
        }
    }
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
                .spawn(move || {
                    loop {
                        let runnable = background_rx.lock().recv();
                        let Ok(runnable) = runnable else {
                            break;
                        };
                        runnable.run();
                    }
                })
                .expect("failed to start Kobo GPUI worker");
        }
        let timers = Arc::new(KoboTimerQueue::default());
        let timer_thread = {
            let timers = Arc::clone(&timers);
            let main_tx = main_tx.clone();
            thread::Builder::new()
                .name("gpui-kobo-timer".into())
                .spawn(move || timers.run(main_tx))
                .expect("failed to start Kobo GPUI timer worker")
        };
        Arc::new(Self {
            main_thread: thread::current().id(),
            main_tx,
            main_rx: Mutex::new(main_rx),
            background_tx,
            timers,
            timer_thread: Some(timer_thread),
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
        self.timers.schedule(duration, runnable);
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        thread::spawn(f);
    }
}

impl Drop for KoboDispatcher {
    fn drop(&mut self) {
        self.timers.shutdown();
        if let Some(timer_thread) = self.timer_thread.take() {
            let _ = timer_thread.join();
        }
    }
}

#[derive(Debug)]
struct KoboDisplay {
    bounds: Cell<Bounds<Pixels>>,
}

impl KoboDisplay {
    fn set_bounds(&self, bounds: Bounds<Pixels>) {
        self.bounds.set(bounds);
    }
}

impl PlatformDisplay for KoboDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(1)
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(Uuid::from_u128(0x4b4f424f_4750_5549_0000_000000000001))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
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

struct KoboWindowRenderState {
    renderer: KoboRenderer,
    previous_frame: Option<crate::GrayFrame>,
}

struct KoboWindowState {
    handle: AnyWindowHandle,
    bounds: Cell<Bounds<Pixels>>,
    scale_factor: Cell<f32>,
    display: Rc<dyn PlatformDisplay>,
    render: RefCell<KoboWindowRenderState>,
    mouse_position: Cell<Point<Pixels>>,
    input_handler: Cell<Option<PlatformInputHandler>>,
    text_input_active: Cell<bool>,
    request_frame: Cell<Option<Box<dyn FnMut(RequestFrameOptions)>>>,
    frame_requested: Rc<Cell<bool>>,
    activation_requested: Cell<bool>,
    input: Cell<Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>>,
    active: Cell<bool>,
    active_changed: Cell<Option<Box<dyn FnMut(bool)>>>,
    hover_changed: Cell<Option<Box<dyn FnMut(bool)>>>,
    resized: Cell<Option<Box<dyn FnMut(Size<Pixels>, f32)>>>,
    moved: Cell<Option<Box<dyn FnMut()>>>,
    should_close: Cell<Option<Box<dyn FnMut() -> bool>>>,
    hit_test: Cell<Option<Box<dyn FnMut() -> Option<WindowControlArea>>>>,
    closed: Cell<Option<Box<dyn FnOnce()>>>,
    pending_update: Cell<Option<FrameUpdate>>,
    render_error: Cell<Option<String>>,
    render_count: Cell<u64>,
    show_exit_button: bool,
}

/// GPUI's sole fullscreen Kobo window.
#[derive(Clone)]
pub struct KoboWindow(Rc<KoboWindowState>);

impl KoboWindow {
    fn new(
        handle: AnyWindowHandle,
        params: WindowParams,
        display: Rc<dyn PlatformDisplay>,
        scale_factor: f32,
        show_exit_button: bool,
    ) -> Self {
        Self(Rc::new(KoboWindowState {
            handle,
            bounds: Cell::new(params.bounds),
            scale_factor: Cell::new(scale_factor),
            display,
            render: RefCell::new(KoboWindowRenderState {
                renderer: KoboRenderer::new(),
                previous_frame: None,
            }),
            mouse_position: Cell::new(Point::default()),
            input_handler: Cell::new(None),
            text_input_active: Cell::new(false),
            request_frame: Cell::new(None),
            frame_requested: Rc::new(Cell::new(false)),
            activation_requested: Cell::new(false),
            input: Cell::new(None),
            active: Cell::new(true),
            active_changed: Cell::new(None),
            hover_changed: Cell::new(None),
            resized: Cell::new(None),
            moved: Cell::new(None),
            should_close: Cell::new(None),
            hit_test: Cell::new(None),
            closed: Cell::new(None),
            pending_update: Cell::new(None),
            render_error: Cell::new(None),
            render_count: Cell::new(0),
            show_exit_button,
        }))
    }

    fn handle(&self) -> AnyWindowHandle {
        self.0.handle
    }

    fn request_frame(&self, force: bool) {
        // Clear before dispatch so demand raised by this frame survives.
        self.0.frame_requested.set(false);
        let callback = self.0.request_frame.take();
        if let Some(mut callback) = callback {
            callback(RequestFrameOptions {
                require_presentation: force,
                force_render: force,
            });
            self.0.request_frame.set(Some(callback));
        }
    }

    fn reconfigure_scale_factor(&self, scale_factor: f32) {
        self.0.scale_factor.set(scale_factor);
        let mut render = self.0.render.borrow_mut();
        render.renderer.reset_surface();
        render.previous_frame = None;
        drop(render);
        self.0.pending_update.set(None);
        self.0.mouse_position.set(Point::default());
        let size = self.0.bounds.get().size;
        let resized = self.0.resized.take();
        if let Some(mut resized) = resized {
            resized(size, scale_factor);
            self.0.resized.set(Some(resized));
        }
    }

    fn reconfigure_logical_size(&self, size: Size<Pixels>) {
        self.0.bounds.set(Bounds {
            origin: point(px(0.0), px(0.0)),
            size,
        });
        let mut render = self.0.render.borrow_mut();
        render.renderer.reset_surface();
        render.previous_frame = None;
        drop(render);
        self.0.pending_update.set(None);
        self.0.mouse_position.set(Point::default());
        let resized = self.0.resized.take();
        let scale_factor = self.0.scale_factor.get();
        if let Some(mut resized) = resized {
            resized(size, scale_factor);
            self.0.resized.set(Some(resized));
        }
    }

    fn set_active(&self, value: bool) {
        self.0.active.set(value);
        if let Some(mut callback) = self.0.active_changed.take() {
            callback(value);
            self.0.active_changed.set(Some(callback));
        }
    }

    fn activate_callbacks(&self) {
        if !self.0.activation_requested.replace(false) {
            return;
        }
        self.set_active(true);
        let hovered = self.0.hover_changed.take();
        if let Some(mut hovered) = hovered {
            hovered(true);
            self.0.hover_changed.set(Some(hovered));
        }
    }

    fn close(&self) {
        let should_close = self.0.should_close.take();
        let allowed = if let Some(mut should_close) = should_close {
            let allowed = should_close();
            self.0.should_close.set(Some(should_close));
            allowed
        } else {
            true
        };
        if allowed {
            let closed = self.0.closed.take();
            if let Some(closed) = closed {
                closed();
            }
        }
    }

    fn dispatch_input(&self, input: PlatformInput, position: Point<Pixels>) -> bool {
        self.0.mouse_position.set(position);
        let callback = self.0.input.take();
        let Some(mut callback) = callback else {
            return false;
        };
        let result = callback(input);
        self.0.input.set(Some(callback));
        !result.propagate
    }

    fn take_update(&self) -> Option<FrameUpdate> {
        self.0.pending_update.take()
    }

    fn last_frame(&self) -> Option<GrayImage> {
        self.0
            .render
            .borrow()
            .previous_frame
            .as_ref()
            .map(|frame| frame.as_ref().clone())
    }

    fn take_render_error(&self) -> Option<String> {
        self.0.render_error.take()
    }

    fn render_count(&self) -> u64 {
        self.0.render_count.get()
    }
}

impl HasWindowHandle for KoboWindow {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

impl HasDisplayHandle for KoboWindow {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

impl PlatformWindow for KoboWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.bounds.get()
    }
    fn is_maximized(&self) -> bool {
        true
    }
    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Maximized(self.bounds())
    }
    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }
    fn resize(&mut self, size: Size<Pixels>) {
        let mut bounds = self.0.bounds.get();
        bounds.size = size;
        self.0.bounds.set(bounds);
    }
    fn scale_factor(&self) -> f32 {
        self.0.scale_factor.get()
    }
    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Light
    }
    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.display.clone())
    }
    fn mouse_position(&self) -> Point<Pixels> {
        self.0.mouse_position.get()
    }
    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }
    fn capslock(&self) -> Capslock {
        Capslock::default()
    }
    fn set_input_handler(&mut self, handler: PlatformInputHandler) {
        self.0.input_handler.set(Some(handler));
    }
    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.input_handler.take()
    }
    fn is_text_input_active(&self) -> bool {
        self.0.text_input_active.get()
    }
    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        None
    }
    fn activate(&self) {
        self.0.activation_requested.set(true);
    }
    fn is_active(&self) -> bool {
        self.0.active.get()
    }
    fn is_hovered(&self) -> bool {
        true
    }
    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }
    fn set_title(&mut self, _title: &str) {}
    fn set_background_appearance(&self, _appearance: WindowBackgroundAppearance) {}
    fn minimize(&self) {}
    fn zoom(&self) {}
    fn toggle_fullscreen(&self) {}
    fn is_fullscreen(&self) -> bool {
        true
    }
    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.request_frame.set(Some(callback));
    }
    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.input.set(Some(callback));
    }
    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.active_changed.set(Some(callback));
    }
    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.hover_changed.set(Some(callback));
    }
    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.resized.set(Some(callback));
    }
    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.moved.set(Some(callback));
    }
    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.should_close.set(Some(callback));
    }
    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.0.hit_test.set(Some(callback));
    }
    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.closed.set(Some(callback));
    }
    fn on_appearance_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn draw(&self, scene: &Scene) {
        self.0
            .render_count
            .set(self.0.render_count.get().saturating_add(1));
        let profile_enabled = render_profiling_enabled();
        let total_started = profile_enabled.then(Instant::now);
        let scale_factor = self.0.scale_factor.get();
        let device_size: Size<DevicePixels> =
            self.0.bounds.get().size.to_device_pixels(scale_factor);
        let mut render = self.0.render.borrow_mut();
        let previous_image = render.previous_frame.clone();
        let render_started = profile_enabled.then(Instant::now);
        match render
            .renderer
            .render_scene_to_image(scene, device_size, previous_image.as_deref())
        {
            Ok(Some((mut image, candidate_damage))) => {
                let render_us = render_started.map_or(0, |started| started.elapsed().as_micros());
                if self.0.show_exit_button {
                    draw_exit_button(image.image_mut(), scale_factor);
                }
                let damage_started = profile_enabled.then(Instant::now);
                let damage = previous_image
                    .as_deref()
                    .and_then(|previous| {
                        changed_pixel_bounds_in(previous, &image, candidate_damage)
                    })
                    .or_else(|| {
                        previous_image.is_none().then_some(PixelRect {
                            x: 0,
                            y: 0,
                            width: image.width(),
                            height: image.height(),
                        })
                    });
                let damage_scan_us =
                    damage_started.map_or(0, |started| started.elapsed().as_micros());
                if let Some(total_started) = total_started {
                    println!(
                        "GPUI_KOBO_DRAW changed={} render_us={render_us} damage_scan_us={damage_scan_us} damage_pixels={} total_us={}",
                        damage.is_some(),
                        damage.map_or(0, |damage| u64::from(damage.width)
                            * u64::from(damage.height)),
                        total_started.elapsed().as_micros(),
                    );
                }
                render.previous_frame = Some(image.clone());
                self.0.pending_update.set(damage.map(|damage| FrameUpdate {
                    image,
                    previous_image,
                    damage,
                }));
            }
            Ok(None) => {
                if let Some(total_started) = total_started {
                    println!(
                        "GPUI_KOBO_DRAW changed=false render_us={} damage_scan_us=0 damage_pixels=0 total_us={}",
                        render_started.map_or(0, |started| started.elapsed().as_micros()),
                        total_started.elapsed().as_micros(),
                    );
                }
                self.0.pending_update.set(None);
            }
            Err(error) => self.0.render_error.set(Some(error.to_string())),
        }
        drop(render);

        let input_handler = self.0.input_handler.take();
        let input_active = input_handler.is_some();
        self.0.input_handler.set(input_handler);
        if self.0.text_input_active.replace(input_active) != input_active {
            self.0.frame_requested.set(true);
        }
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        let requested = self.0.frame_requested.clone();
        Some(Rc::new(move || requested.set(true)))
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.render.borrow().renderer.sprite_atlas()
    }
    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }
    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }
    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}
}

#[derive(Clone, Copy, Default)]
struct TouchAccumulator {
    last_y: Option<f32>,
    scroll_y: f32,
    exit_candidate: bool,
}

const EXIT_BUTTON_WIDTH: f32 = 116.0;
const EXIT_BUTTON_HEIGHT: f32 = 48.0;
const EXIT_BUTTON_MARGIN: f32 = 8.0;

fn draw_exit_button(image: &mut GrayImage, scale_factor: f32) {
    let scale = scale_factor.max(1.0);
    let margin = (EXIT_BUTTON_MARGIN * scale).round() as u32;
    let width = (EXIT_BUTTON_WIDTH * scale).round() as u32;
    let height = (EXIT_BUTTON_HEIGHT * scale).round() as u32;
    if image.width() < width + margin || image.height() < height + margin {
        return;
    }
    let left = image.width() - width - margin;
    let top = margin;
    for y in top..top + height {
        for x in left..left + width {
            let border =
                x < left + 3 || x >= left + width - 3 || y < top + 3 || y >= top + height - 3;
            image.put_pixel(x, y, if border { Luma([0]) } else { Luma([255]) });
        }
    }

    let dot = (3.0 * scale).round() as u32;
    let text_height = 7 * dot;
    let text_top = top + (height - text_height) / 2;
    draw_bitmap_text(image, "EXIT", dot, text_top, left, width);
}

fn glyph_rows(character: char) -> [u8; 7] {
    match character {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'I' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'N' => [
            0b10001, 0b11001, 0b11001, 0b10101, 0b10011, 0b10011, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'X' => [
            0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b01010, 0b10001,
        ],
        _ => [0; 7],
    }
}

fn draw_bitmap_text(
    image: &mut GrayImage,
    text: &str,
    dot: u32,
    top: u32,
    region_left: u32,
    region_width: u32,
) {
    let glyph_width = 5 * dot;
    let gap = 2 * dot;
    let characters = text.chars().count() as u32;
    let text_width = characters * glyph_width + characters.saturating_sub(1) * gap;
    if text_width > region_width
        || region_left + region_width > image.width()
        || top + 7 * dot > image.height()
    {
        return;
    }
    let text_left = region_left + (region_width - text_width) / 2;
    for (glyph_index, character) in text.chars().enumerate() {
        let glyph = glyph_rows(character);
        let glyph_left = text_left + glyph_index as u32 * (glyph_width + gap);
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) == 0 {
                    continue;
                }
                let pixel_left = glyph_left + column * dot;
                let pixel_top = top + row as u32 * dot;
                for y in pixel_top..pixel_top + dot {
                    for x in pixel_left..pixel_left + dot {
                        image.put_pixel(x, y, Luma([0]));
                    }
                }
            }
        }
    }
}

fn draw_quitting_screen(image: &mut GrayImage) {
    for pixel in image.pixels_mut() {
        *pixel = Luma([255]);
    }
    let dot = (image.width() / 100).clamp(4, 8);
    let line_height = 7 * dot;
    let gap = 5 * dot;
    let top = image.height().saturating_sub(line_height * 2 + gap) / 2;
    draw_bitmap_text(image, "RETURNING", dot, top, 0, image.width());
    draw_bitmap_text(
        image,
        "TO KOBO",
        dot,
        top + line_height + gap,
        0,
        image.width(),
    );
}

fn draw_sleep_screen(image: &mut GrayImage) {
    for pixel in image.pixels_mut() {
        *pixel = Luma([255]);
    }
    let dot = (image.width() / 100).clamp(1, 8);
    let top = image.height().saturating_sub(7 * dot) / 2;
    draw_bitmap_text(image, "SLEEPING", dot, top, 0, image.width());
}

/// Production, single-window GPUI platform for Kobo e-readers.
pub struct KoboPlatform {
    options: KoboPlatformOptions,
    logical_size: Cell<Size<Pixels>>,
    dispatcher: Arc<KoboDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    display: Rc<KoboDisplay>,
    windows: Cell<Vec<Weak<KoboWindowState>>>,
    cached_frame: Cell<Option<GrayImage>>,
    cached_render_count: Cell<u64>,
    quit: Cell<bool>,
    error: Cell<Option<String>>,
    clipboard: Mutex<Option<ClipboardItem>>,
    run_state: Cell<Option<KoboRunState>>,
    auto_rotate: Cell<bool>,
    display_geometry_refresh_requested: Cell<bool>,
    queued_events: Cell<VecDeque<RuntimeEvent>>,
    quit_callback: Cell<Option<Box<dyn FnMut()>>>,
    wake_callback: Cell<Option<Box<dyn FnMut()>>>,
    full_repaint_requested: Cell<bool>,
    full_gc16_requested: Cell<bool>,
    scale_factor: Cell<f32>,
    native_scale_factor: f32,
    experience_mode: Cell<KoboExperienceMode>,
    requested_experience_mode: Cell<Option<KoboExperienceMode>>,
    render_mode: Cell<KoboRenderMode>,
    requested_render_mode: Cell<Option<KoboRenderMode>>,
    reader_page_content_top: Cell<Option<Pixels>>,
    reader_page_fast_until: Cell<Option<Instant>>,
    application_startup_started_at: Cell<Option<Instant>>,
    book_launch_started_at: Cell<Option<Instant>>,
    return_screen_requested: Cell<bool>,
}

impl KoboPlatform {
    fn with_windows<R>(&self, operation: impl FnOnce(&mut Vec<Weak<KoboWindowState>>) -> R) -> R {
        let mut windows = self.windows.take();
        let result = operation(&mut windows);
        self.windows.set(windows);
        result
    }

    fn logical_size(&self) -> Size<Pixels> {
        self.logical_size.get()
    }

    fn set_logical_size(&self, logical_size: Size<Pixels>) {
        if self.logical_size.get() == logical_size {
            return;
        }
        self.logical_size.set(logical_size);
        self.display.set_bounds(display_bounds(logical_size));
        if let Some(window) = self.window() {
            window.reconfigure_logical_size(logical_size);
            self.full_repaint_requested.set(true);
        }
    }

    fn set_auto_rotation(&self, auto_rotate: bool) {
        self.auto_rotate.set(auto_rotate);
        if auto_rotate {
            self.display_geometry_refresh_requested.set(true);
        }
    }

    /// Create the platform and, when requested, initialize FBInk and evdev before GPUI starts.
    pub fn new(options: KoboPlatformOptions) -> Result<Rc<Self>> {
        let mut options = options;
        options.render_mode = options.experience_mode.render_mode();
        let dispatcher = KoboDispatcher::new();
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        let text_system = Arc::new(CosmicTextSystem::new_without_system_fonts("Lilex"));
        text_system.add_fonts(vec![
            std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/fonts/lilex/Lilex-Regular.ttf"
            )),
            std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/fonts/lilex/Lilex-Bold.ttf"
            )),
        ])?;

        let presenter = options
            .display
            .then(|| {
                FbInkPresenter::open_with_render_mode(
                    options.render_mode,
                    options.monochrome_threshold,
                )
            })
            .transpose()?;
        let (profile, native_scale_factor) = if let Some(presenter) = presenter.as_ref() {
            let geometry = presenter.geometry();
            println!("display: {}", geometry.description());
            let divisor = if geometry.view_width % 2 == 0 && geometry.view_height % 2 == 0 {
                2
            } else {
                1
            };
            (
                DeviceRefreshProfile::for_device(&geometry.device_codename),
                divisor as f32,
            )
        } else {
            (DeviceRefreshProfile::default(), 2.0)
        };
        let logical_size = presenter
            .as_ref()
            .map_or(options.logical_size, |presenter| {
                inferred_logical_size(presenter.geometry())
            });
        options.logical_size = logical_size;
        options.scale_factor = options.experience_mode.scale_factor(native_scale_factor);
        let canvas_size: Size<DevicePixels> =
            options.logical_size.to_device_pixels(options.scale_factor);
        let canvas_width = canvas_size.width.0.max(1) as u32;
        let canvas_height = canvas_size.height.0.max(1) as u32;
        let runtime = if let Some(presenter) = presenter.as_ref() {
            options
                .interactive
                .then(|| KoboRuntime::open(presenter.geometry(), canvas_width, canvas_height))
                .transpose()?
        } else {
            None
        };

        let bounds = display_bounds(options.logical_size);
        let platform = Rc::new(Self {
            options,
            logical_size: Cell::new(logical_size),
            auto_rotate: Cell::new(options.auto_rotate),
            dispatcher,
            background_executor,
            foreground_executor,
            text_system,
            display: Rc::new(KoboDisplay {
                bounds: Cell::new(bounds),
            }),
            windows: Cell::new(Vec::new()),
            cached_frame: Cell::new(None),
            cached_render_count: Cell::new(0),
            quit: Cell::new(false),
            error: Cell::new(None),
            clipboard: Mutex::new(None),
            run_state: Cell::new(Some(KoboRunState {
                presenter,
                runtime,
                scheduler: RepaintScheduler::new_with_render_mode(profile, options.render_mode),
                resource_monitor: KoboResourceMonitor::new(Instant::now()),
                touch: TouchAccumulator::default(),
                suppress_next_power_release: false,
                wake_frame_pending: false,
            })),
            display_geometry_refresh_requested: Cell::new(false),
            queued_events: Cell::new(VecDeque::new()),
            quit_callback: Cell::new(None),
            wake_callback: Cell::new(None),
            full_repaint_requested: Cell::new(false),
            full_gc16_requested: Cell::new(false),
            scale_factor: Cell::new(options.scale_factor),
            native_scale_factor,
            experience_mode: Cell::new(options.experience_mode),
            requested_experience_mode: Cell::new(None),
            render_mode: Cell::new(options.render_mode),
            requested_render_mode: Cell::new(None),
            reader_page_content_top: Cell::new(None),
            reader_page_fast_until: Cell::new(None),
            application_startup_started_at: Cell::new(None),
            book_launch_started_at: Cell::new(None),
            return_screen_requested: Cell::new(false),
        });
        println!(
            "KOBO_EXPERIENCE initial={:?} logical={}x{} framebuffer={}x{} scale={} render={:?} threshold={}",
            options.experience_mode,
            f32::from(options.logical_size.width),
            f32::from(options.logical_size.height),
            canvas_width,
            canvas_height,
            options.scale_factor,
            options.render_mode,
            options.monochrome_threshold,
        );
        ACTIVE_KOBO_PLATFORM.with(|active| active.set(Rc::downgrade(&platform)));
        Ok(platform)
    }

    /// Return the last CPU-rendered framebuffer, including in no-display mode.
    pub fn last_frame(&self) -> Option<GrayImage> {
        if let Some(frame) = self.window().and_then(|window| window.last_frame()) {
            return Some(frame);
        }
        let frame = self.cached_frame.take();
        let result = frame.clone();
        self.cached_frame.set(frame);
        result
    }

    /// Return and clear a backend error captured by GPUI's non-fallible platform callbacks.
    pub fn take_error(&self) -> Option<anyhow::Error> {
        self.error.take().map(|message| anyhow!(message))
    }

    /// Queue a native event before `Application::run`, primarily for deterministic host checks.
    pub fn queue_event(&self, event: RuntimeEvent) {
        let mut events = self.queued_events.take();
        events.push_back(event);
        self.queued_events.set(events);
    }

    fn present_pending_full_repaint(&self, state: &mut KoboRunState) -> Result<bool> {
        if SLEEP_PENDING.with(|pending| pending.replace(false)) {
            if let Err(error) = self.request_suspend(state) { eprintln!("Kobo sleep failed: {error}"); }
        }
        if !self.full_repaint_requested.replace(false) {
            return Ok(false);
        }
        if let Some(experience_mode) = self.requested_experience_mode.take() {
            self.apply_experience_mode(state, experience_mode);
        } else if let Some(render_mode) = self.requested_render_mode.take() {
            self.render_mode.set(render_mode);
            state.scheduler.set_render_mode(render_mode);
            if let Some(presenter) = state.presenter.as_mut() {
                presenter.set_render_mode(render_mode);
            }
            println!("Kobo rendering mode changed: {render_mode:?}");
        }
        println!("GPUI full repaint request consumed at event-loop boundary");
        let force = self.full_gc16_requested.get();
        self.request_and_present(state, false, force)?;
        Ok(true)
    }

    fn apply_experience_mode(&self, state: &mut KoboRunState, experience_mode: KoboExperienceMode) {
        let started = Instant::now();
        let previous = self.experience_mode.replace(experience_mode);
        let scale_factor = experience_mode.scale_factor(self.native_scale_factor);
        let render_mode = experience_mode.render_mode();
        self.requested_render_mode.set(None);
        self.scale_factor.set(scale_factor);
        self.render_mode.set(render_mode);
        state.scheduler.set_render_mode(render_mode);
        if let Some(presenter) = state.presenter.as_mut() {
            presenter.set_render_mode(render_mode);
        }
        let logical_size = self.logical_size();
        let canvas_size: Size<DevicePixels> = logical_size.to_device_pixels(scale_factor);
        let canvas_width = canvas_size.width.0.max(1) as u32;
        let canvas_height = canvas_size.height.0.max(1) as u32;
        if let Some(runtime) = state.runtime.as_mut() {
            runtime.set_canvas_size(canvas_width, canvas_height);
        }
        let windows = self
            .with_windows(|windows| windows.iter().filter_map(Weak::upgrade).collect::<Vec<_>>());
        for window in windows {
            KoboWindow(window).reconfigure_scale_factor(scale_factor);
        }
        state.touch = TouchAccumulator::default();
        self.full_gc16_requested.set(true);
        println!(
            "KOBO_EXPERIENCE transition={previous:?}->{experience_mode:?} logical={}x{} framebuffer={}x{} scale={} render={render_mode:?} reset_windows=true elapsed_ms={}",
            f32::from(logical_size.width),
            f32::from(logical_size.height),
            canvas_width,
            canvas_height,
            scale_factor,
            started.elapsed().as_millis(),
        );
    }

    /// Number of actual GPUI scene draws performed by the active window.
    pub fn render_count(&self) -> u64 {
        self.window().map_or_else(
            || self.cached_render_count.get(),
            |window| window.render_count(),
        )
    }

    /// Whether GPUI or a Kobo lifecycle event requested application termination.
    pub fn quit_requested(&self) -> bool {
        self.quit.get()
    }

    fn fail(&self, error: impl std::fmt::Display) {
        self.error.set(Some(error.to_string()));
        self.quit.set(true);
    }

    fn exit_button_hit(&self, mapped_x: f32, mapped_y: f32) -> bool {
        if !self.options.show_exit_button {
            return false;
        }
        let x = px(mapped_x / self.scale_factor.get());
        let y = px(mapped_y / self.scale_factor.get());
        let logical_size = self.logical_size();
        x >= logical_size.width - px(EXIT_BUTTON_WIDTH + EXIT_BUTTON_MARGIN)
            && x <= logical_size.width - px(EXIT_BUTTON_MARGIN)
            && y >= px(EXIT_BUTTON_MARGIN)
            && y <= px(EXIT_BUTTON_MARGIN + EXIT_BUTTON_HEIGHT)
    }

    fn request_quit(&self) {
        if self.quit.replace(true) {
            return;
        }
        self.return_screen_requested.set(true);
    }

    fn present_return_screen(&self, state: &mut KoboRunState) -> Result<()> {
        if !self.return_screen_requested.replace(false) {
            return Ok(());
        }
        println!("Kobo exit requested; presenting return screen");
        if let Some(mut frame) = self.last_frame() {
            draw_quitting_screen(&mut frame);
            if let Some(presenter) = state.presenter.as_mut() {
                presenter.present_full(&frame)?;
            }
        }
        println!("Kobo return screen presented");
        Ok(())
    }

    fn request_suspend(&self, state: &mut KoboRunState) -> Result<()> {
        let handler = self
            .options
            .power_button_handler
            .ok_or_else(|| anyhow!("Kobo application did not install a power-button handler"))?;
        let mut frame = self
            .last_frame()
            .ok_or_else(|| anyhow!("Kobo sleep requested before the first frame"))?;
        draw_sleep_screen(&mut frame);
        // Also restore the app if presenting or suspending fails. The sleep
        // page must not replace the cached application frame.
        self.full_repaint_requested.set(true);
        self.full_gc16_requested.set(true);
        if let Some(presenter) = state.presenter.as_mut() {
            // Full refreshes wait for e-ink completion in the FBInk shim.
            presenter.present_full_gc16(&frame)?;
        }
        println!("Kobo sleep screen presented; suspending");
        let windows = self.with_windows(|windows| {
            windows.iter().filter_map(Weak::upgrade).map(KoboWindow).collect::<Vec<_>>()
        });
        for window in &windows { window.set_active(false); }
        let suspend_result = handler();
        for window in &windows { window.set_active(true); }
        state.wake_frame_pending = true;
        if let Some(runtime) = state.runtime.as_mut() {
            let (touch_events, button_events) = runtime.discard_pending_input()?;
            println!(
                "discarded input accumulated during Kobo suspend: touch_events={touch_events} button_events={button_events}"
            );
        }
        state.suppress_next_power_release = false;

        let geometry = if let Some(presenter) = state.presenter.as_mut() {
            presenter.reinitialize()?;
            Some(presenter.geometry().clone())
        } else {
            None
        };
        if let Some(geometry) = geometry {
            self.apply_display_geometry(state, &geometry);
        }
        let callback = self.wake_callback.take();
        if let Some(mut callback) = callback {
            callback();
            self.wake_callback.set(Some(callback));
        }
        self.full_repaint_requested.set(true);
        self.full_gc16_requested.set(true);
        suspend_result.map_err(|error| anyhow!("Kobo application suspend failed: {error}"))
    }

    fn apply_display_geometry(&self, state: &mut KoboRunState, geometry: &crate::ScreenGeometry) {
        let Some(runtime) = state.runtime.as_mut() else {
            return;
        };
        runtime.update_geometry(geometry);

        let logical_size = inferred_logical_size(geometry);
        self.full_repaint_requested.set(true);
        if logical_size != self.logical_size() {
            let (canvas_width, canvas_height) =
                canvas_dimensions(logical_size, self.scale_factor.get());
            runtime.set_canvas_size(canvas_width, canvas_height);
            self.set_logical_size(logical_size);
            println!(
                "KOBO display geometry changed logical={}x{} framebuffer={}x{}",
                f32::from(logical_size.width),
                f32::from(logical_size.height),
                canvas_width,
                canvas_height,
            );
        }
    }

    fn window(&self) -> Option<KoboWindow> {
        self.with_windows(|windows| {
            loop {
                let window = windows.last()?.upgrade();
                if let Some(window) = window {
                    return Some(KoboWindow(window));
                }
                windows.pop();
            }
        })
    }

    fn notify_wake_ui_ready(&self, state: &mut KoboRunState) {
        if std::mem::take(&mut state.wake_frame_pending) {
            if let Some(handler) = self.options.wake_ui_ready_handler {
                handler();
            }
        }
    }

    fn request_and_present(
        &self,
        state: &mut KoboRunState,
        scrolling: bool,
        force: bool,
    ) -> Result<()> {
        let Some(window) = self.window() else {
            return Ok(());
        };
        window.activate_callbacks();
        let pipeline_started = Instant::now();
        let render_started = Instant::now();
        window.request_frame(force);
        self.dispatcher.run_pending();
        let render_ms = render_started.elapsed().as_millis();
        if let Some(error) = window.take_render_error() {
            return Err(anyhow!(error));
        }
        if let Some(frame) = window.last_frame() {
            self.cached_frame.set(Some(frame));
        }
        self.cached_render_count.set(window.render_count());
        let update = window.take_update();
        if force {
            let image = update
                .as_ref()
                .map(|update| update.image.clone())
                .or_else(|| window.last_frame().map(crate::GrayFrame::unpooled));
            let Some(image) = image else {
                self.full_repaint_requested.set(true);
                println!(
                    "GPUI timing: force=true scrolling={scrolling} render_ms={render_ms} frame=false total_ms={}",
                    pipeline_started.elapsed().as_millis()
                );
                return Ok(());
            };
            let present_started = Instant::now();
            if let Some(presenter) = state.presenter.as_mut() {
                if self.full_gc16_requested.get() {
                    presenter.present_full_gc16(&image)?;
                } else {
                    presenter.present_full(&image)?;
                }
            }
            self.full_gc16_requested.set(false);
            state.scheduler.record_full_presentation(&image);
            self.notify_wake_ui_ready(state);
            println!(
                "GPUI timing: force=true scrolling={scrolling} render_ms={render_ms} changed={} present_ms={} total_ms={}",
                update.is_some(),
                present_started.elapsed().as_millis(),
                pipeline_started.elapsed().as_millis()
            );
            if self.experience_mode.get() == KoboExperienceMode::LibraryFast
                && let Some(started_at) = self.application_startup_started_at.take()
            {
                println!(
                    "KOBO_APP_STARTUP phase=first_frame_presented elapsed_ms={}",
                    started_at.elapsed().as_millis()
                );
            }
            if self.experience_mode.get() == KoboExperienceMode::ReaderQuality
                && let Some(started_at) = self.book_launch_started_at.take()
            {
                println!(
                    "KOBO_BOOK_STARTUP phase=first_frame_presented elapsed_ms={}",
                    started_at.elapsed().as_millis()
                );
            }
            return Ok(());
        }
        let Some(update) = update else {
            println!(
                "GPUI timing: force={force} scrolling={scrolling} render_ms={render_ms} changed=false total_ms={}",
                pipeline_started.elapsed().as_millis()
            );
            return Ok(());
        };
        let reader_page_fast = self
            .reader_page_fast_until
            .get()
            .is_some_and(|until| Instant::now() <= until);
        if !reader_page_fast {
            self.reader_page_fast_until.set(None);
        }
        if reader_page_fast && let Some(content_top) = self.reader_page_content_top.get() {
            let split_y = (f32::from(content_top) * self.scale_factor.get())
                .round()
                .clamp(0.0, update.image.height() as f32) as u32;
            let damage_bottom = update.damage.y.saturating_add(update.damage.height);
            let content_y = update.damage.y.max(split_y);
            let content_bottom = damage_bottom.min(update.image.height());
            let toolbar_bottom = damage_bottom.min(split_y);
            let present_started = Instant::now();
            if let Some(presenter) = state.presenter.as_mut() {
                if content_bottom > content_y {
                    presenter.present_update(
                        &FrameUpdate {
                            image: update.image.clone(),
                            previous_image: update.previous_image.clone(),
                            damage: PixelRect {
                                x: update.damage.x,
                                y: content_y,
                                width: update.damage.width,
                                height: content_bottom - content_y,
                            },
                        },
                        RefreshMode::TextMono,
                    )?;
                }
                if toolbar_bottom > update.damage.y {
                    presenter.present_update(
                        &FrameUpdate {
                            image: update.image.clone(),
                            previous_image: update.previous_image.clone(),
                            damage: PixelRect {
                                x: update.damage.x,
                                y: update.damage.y,
                                width: update.damage.width,
                                height: toolbar_bottom - update.damage.y,
                            },
                        },
                        RefreshMode::TextMono,
                    )?;
                }
            }
            println!(
                "GPUI reader page refresh: content=DU toolbar=DU split_y={split_y} damage=x:{},y:{},w:{},h:{} present_ms={} total_ms={}",
                update.damage.x,
                update.damage.y,
                update.damage.width,
                update.damage.height,
                present_started.elapsed().as_millis(),
                pipeline_started.elapsed().as_millis(),
            );
            state.scheduler.record_fast_presentation(&update, Instant::now());
            return Ok(());
        }
        let schedule_started = Instant::now();
        state.scheduler.enqueue(update, scrolling);
        let refresh = state.scheduler.flush(Instant::now());
        if let Some(refresh) = refresh {
            let schedule_ms = schedule_started.elapsed().as_millis();
            println!(
                "GPUI refresh: mode={:?} reason={:?} damage=x:{},y:{},w:{},h:{}",
                refresh.mode,
                refresh.reason,
                refresh.update.damage.x,
                refresh.update.damage.y,
                refresh.update.damage.width,
                refresh.update.damage.height
            );
            let present_started = Instant::now();
            if let Some(presenter) = state.presenter.as_mut() {
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

    fn dispatch_scroll(&self, state: &mut KoboRunState, canvas_delta_y: f32) -> Result<()> {
        let Some(window) = self.window() else {
            return Ok(());
        };
        let dispatch_started = Instant::now();
        let logical_size = self.logical_size();
        let position = point(logical_size.width / 2.0, logical_size.height / 2.0);
        window.dispatch_input(
            PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(point(
                    px(0.0),
                    px(canvas_delta_y / self.scale_factor.get()),
                )),
                touch_phase: gpui::TouchPhase::Ended,
                ..Default::default()
            }),
            position,
        );
        let dispatch_ms = dispatch_started.elapsed().as_millis();
        let frame_started = Instant::now();
        let result = self.request_and_present(state, true, false);
        println!(
            "GPUI scroll timing: delta={canvas_delta_y:.1} dispatch_ms={dispatch_ms} frame_ms={} total_ms={}",
            frame_started.elapsed().as_millis(),
            dispatch_started.elapsed().as_millis()
        );
        result
    }

    fn dispatch_page_key(&self, state: &mut KoboRunState, key: &str) -> Result<()> {
        let Some(window) = self.window() else {
            return Ok(());
        };
        if self.experience_mode.get() == KoboExperienceMode::ReaderQuality {
            self.reader_page_content_top.set(Some(px(58.0)));
            self.reader_page_fast_until
                .set(Some(Instant::now() + Duration::from_millis(750)));
        }
        let dispatch_started = Instant::now();
        let position = window.mouse_position();
        window.dispatch_input(
            PlatformInput::KeyDown(KeyDownEvent {
                keystroke: Keystroke::parse(key)
                    .map_err(|error| anyhow!("invalid Kobo page key {key}: {error}"))?,
                key_location: KeyLocation::Standard,
                is_held: false,
                prefer_character_input: false,
            }),
            position,
        );
        let dispatch_ms = dispatch_started.elapsed().as_millis();
        let frame_started = Instant::now();
        let result = self.request_and_present(state, true, false);
        println!(
            "GPUI page-key timing: key={key} dispatch_ms={dispatch_ms} frame_ms={} total_ms={}",
            frame_started.elapsed().as_millis(),
            dispatch_started.elapsed().as_millis()
        );
        result
    }

    fn dispatch_touch(
        &self,
        state: &mut KoboRunState,
        mapped: crate::MappedTouch,
        gesture: Option<Gesture>,
    ) -> Result<()> {
        let position = point(
            px(mapped.x / self.scale_factor.get()),
            px(mapped.y / self.scale_factor.get()),
        );
        let logical_y = mapped.y / self.scale_factor.get();
        match mapped.phase {
            TouchPhase::Down => {
                state.touch = TouchAccumulator {
                    last_y: Some(logical_y),
                    scroll_y: 0.0,
                    exit_candidate: self.exit_button_hit(mapped.x, mapped.y),
                };
                return Ok(());
            }
            TouchPhase::Move => {
                if let Some(last_y) = state.touch.last_y {
                    state.touch.scroll_y += logical_y - last_y;
                }
                state.touch.last_y = Some(logical_y);
                state.touch.exit_candidate &= self.exit_button_hit(mapped.x, mapped.y);
                return Ok(());
            }
            TouchPhase::Cancel => {
                state.touch = TouchAccumulator::default();
                return Ok(());
            }
            TouchPhase::Up => {}
        }

        let mut touch = state.touch;
        if let Some(last_y) = touch.last_y {
            touch.scroll_y += logical_y - last_y;
        }
        let scroll_y = touch.scroll_y;
        let exit_requested = touch.exit_candidate && self.exit_button_hit(mapped.x, mapped.y);
        state.touch = TouchAccumulator::default();
        if exit_requested && !matches!(gesture, Some(Gesture::Swipe { .. })) {
            self.request_quit();
            return self.present_return_screen(state);
        }
        let Some(window) = self.window() else {
            return Ok(());
        };
        let dispatched_window_id = window.handle().window_id();
        let scrolling = matches!(gesture, Some(Gesture::Swipe { .. }))
            || gesture.is_none() && scroll_y.abs() >= 12.0;
        if scrolling {
            window.dispatch_input(
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(point(px(0.0), px(scroll_y))),
                    touch_phase: gpui::TouchPhase::Ended,
                    ..Default::default()
                }),
                position,
            );
        } else {
            let button = if matches!(gesture, Some(Gesture::LongPress { .. })) {
                MouseButton::Right
            } else {
                MouseButton::Left
            };
            let dispatch_started = Instant::now();
            let down_handled = window.dispatch_input(
                PlatformInput::MouseDown(MouseDownEvent {
                    button,
                    position,
                    click_count: 1,
                    ..Default::default()
                }),
                position,
            );
            let up_handled = window.dispatch_input(
                PlatformInput::MouseUp(MouseUpEvent {
                    button,
                    position,
                    click_count: 1,
                    ..Default::default()
                }),
                position,
            );
            println!(
                "GPUI click dispatch: button={button:?} position={position:?} down_handled={down_handled} up_handled={up_handled} dispatch_ms={}",
                dispatch_started.elapsed().as_millis(),
            );
        }
        window.dispatch_input(
            PlatformInput::Touch(TouchEvent {
                id: TouchId(0),
                phase: gpui::TouchPhase::Ended,
                position,
                force: None,
            }),
            position,
        );
        // A mouse callback can remove its own GPUI window. Release the platform
        // window before resolving the next active window so a previous logical
        // window (for example, the library behind a reader) can be promoted.
        drop(window);

        let active_window_changed = self
            .window()
            .is_some_and(|window| window.handle().window_id() != dispatched_window_id);
        if active_window_changed {
            println!("Kobo logical window changed; forcing promoted window frame");
        }

        if active_window_changed && self.full_repaint_requested.get() {
            println!("Kobo promoted window frame coalesced with pending full repaint");
            return Ok(());
        }

        if self.quit.get() {
            Ok(())
        } else {
            self.request_and_present(state, scrolling, active_window_changed)
        }
    }

    fn dispatch_button(&self, state: &mut KoboRunState, event: ButtonEvent) -> Result<()> {
        let button_started = Instant::now();
        if crate::verbose_logging_enabled() {
            println!(
                "hardware button received: {:?} pressed={} repeated={}",
                event.button, event.pressed, event.repeated
            );
        }
        if matches!(event.button, HardwareButton::Power | HardwareButton::Sleep)
            && !event.pressed
            && !event.repeated
        {
            if std::mem::take(&mut state.suppress_next_power_release) {
                println!("ignored power-button release that completed Kobo wake");
                return Ok(());
            }
            if self.options.power_button_opens_menu {
                POWER_MENU_PENDING.with(|pending| pending.set(true));
                return Ok(());
            }
            println!("hardware power button requested Kobo suspend");
            if let Err(error) = self.request_suspend(state) {
                println!("hardware power suspend failed; staying in app: {error}");
            }
            return Ok(());
        }
        if !event.pressed || event.repeated {
            return Ok(());
        }
        let result = match (event.button, self.options.page_buttons) {
            (HardwareButton::PreviousPage, PageButtonBehavior::Scroll) => {
                self.dispatch_scroll(state, 300.0)
            }
            (HardwareButton::NextPage, PageButtonBehavior::Scroll) => {
                self.dispatch_scroll(state, -300.0)
            }
            (HardwareButton::PreviousPage, PageButtonBehavior::ArrowKeys) => {
                self.dispatch_page_key(state, "left")
            }
            (HardwareButton::NextPage, PageButtonBehavior::ArrowKeys) => {
                self.dispatch_page_key(state, "right")
            }
            (HardwareButton::PreviousPage, PageButtonBehavior::PageKeys) => {
                self.dispatch_page_key(state, "pageup")
            }
            (HardwareButton::NextPage, PageButtonBehavior::PageKeys) => {
                self.dispatch_page_key(state, "pagedown")
            }
            (HardwareButton::Home, _) => {
                self.request_quit();
                self.present_return_screen(state)
            }
            (
                HardwareButton::RotateScreen {
                    rotation,
                    from_gyro,
                },
                _,
            ) if event.pressed && !event.repeated => {
                self.dispatch_rotation(state, rotation, from_gyro)
            }
            (HardwareButton::RotateScreen { .. }, _) => Ok(()),
            (HardwareButton::Power | HardwareButton::Sleep | HardwareButton::Unknown(_), _) => {
                Ok(())
            }
        };
        println!(
            "hardware button complete: {:?} total_ms={}",
            event.button,
            button_started.elapsed().as_millis()
        );
        result
    }

    fn dispatch_rotation(
        &self,
        state: &mut KoboRunState,
        rotation: u8,
        from_gyro: bool,
    ) -> Result<()> {
        // Manual rotation remains available while the lock is enabled, but
        // gyro-driven geometry changes must be ignored when auto-rotation is
        // disabled.
        if from_gyro && !self.auto_rotate.get() {
            return Ok(());
        }
        let geometry = {
            let Some(presenter) = state.presenter.as_mut() else {
                return Ok(());
            };
            let native_rotation = presenter.geometry().current_rotation;
            let next_rotation = if rotation == u8::MAX {
                native_rotation.wrapping_add(1) % 4
            } else {
                rotation % 4
            };
            if presenter.geometry().current_rotation != next_rotation {
                println!(
                    "Kobo rotation change: {} -> {} (from_gyro={})",
                    native_rotation,
                    next_rotation,
                    from_gyro,
                );
            }
            presenter.set_current_rotation(next_rotation)?;
            presenter.geometry().clone()
        };
        self.full_gc16_requested.set(false);
        self.apply_display_geometry(state, &geometry);
        Ok(())
    }

    fn process_event(&self, state: &mut KoboRunState, event: RuntimeEvent) -> Result<()> {
        match event {
            RuntimeEvent::Touch { mapped, gesture } => {
                state.suppress_next_power_release = false;
                if crate::verbose_logging_enabled() {
                    if let Some(gesture) = gesture {
                        println!("input gesture: {gesture:?}");
                    }
                }
                self.dispatch_touch(state, mapped, gesture)
            }
            RuntimeEvent::Button(event) => {
                if !matches!(event.button, HardwareButton::Power | HardwareButton::Sleep) {
                    state.suppress_next_power_release = false;
                }
                self.dispatch_button(state, event)
            }
            RuntimeEvent::Idle => {
                if let Some(refresh) = state.scheduler.idle_cleanup(Instant::now()) {
                    if let Some(presenter) = state.presenter.as_mut() {
                        presenter.present_update(&refresh.update, refresh.mode)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn run_event_loop(&self, state: &mut KoboRunState) -> Result<()> {
        loop {
            let Some(runtime) = state.runtime.as_mut() else {
                break;
            };
            let event = runtime.next_event(Duration::ZERO)?;
            if matches!(event, RuntimeEvent::Idle) {
                break;
            }
            self.process_event(state, event)?;
            self.present_pending_full_repaint(state)?;
        }
        loop {
            let mut events = self.queued_events.take();
            let event = events.pop_front();
            self.queued_events.set(events);
            let Some(event) = event else {
                break;
            };
            self.process_event(state, event)?;
            self.present_pending_full_repaint(state)?;
        }
        self.request_and_present(state, false, true)?;
        if !self.options.interactive || self.quit.get() {
            self.present_return_screen(state)?;
            return Ok(());
        }
        let deadline = if self.options.timeout.is_zero() {
            None
        } else {
            Some(Instant::now() + self.options.timeout)
        };
        while !self.quit.get() {
            let ran_callbacks = self.dispatcher.run_pending() > 0;
            if self.display_geometry_refresh_requested.replace(false) && self.auto_rotate.get() {
                let geometry = state
                    .presenter
                    .as_ref()
                    .map(|presenter| presenter.geometry().clone());
                if let Some(geometry) = geometry {
                    self.apply_display_geometry(state, &geometry);
                }
            }
            let frame_requested = self.window().is_some_and(|window| window.0.frame_requested.get() || window.0.activation_requested.get());
            if !self.present_pending_full_repaint(state)? && (ran_callbacks || frame_requested) {
                self.request_and_present(state, false, false)?;
            }
            if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    println!("interactive timeout reached");
                    break;
                }
            }
            let remaining = deadline
                .as_ref()
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|| Duration::from_millis(50));
            let runtime = state
                .runtime
                .as_mut()
                .ok_or_else(|| anyhow!("interactive Kobo platform has no input runtime"))?;
            let event = runtime.next_event(remaining.min(Duration::from_millis(50)))?;
            self.process_event(state, event)?;
            self.present_pending_full_repaint(state)?;
            state.resource_monitor.sample_if_due(Instant::now());
        }
        self.present_return_screen(state)?;
        if let Some(window) = self.window() {
            window.close();
        }
        Ok(())
    }
}

impl Platform for KoboPlatform {
    fn supports_animations(&self) -> bool {
        false
    }

    fn supports_touch_input(&self) -> bool {
        true
    }

    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }
    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }
    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }
    fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
        on_finish_launching();
        let Some(mut state) = self.run_state.take() else {
            self.fail("Kobo platform event loop was started more than once");
            return;
        };
        if let Err(error) = self.run_event_loop(&mut state) {
            self.fail(error);
        }
        let callback = self.quit_callback.take();
        if let Some(mut callback) = callback {
            callback();
        }
    }
    fn quit(&self) {
        self.request_quit();
    }
    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<OsString>) {
        self.quit();
    }
    fn activate(&self, _ignoring_other_apps: bool) {}
    fn hide(&self) {}
    fn hide_other_apps(&self) {}
    fn unhide_other_apps(&self) {}
    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }
    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }
    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.window().map(|window| window.handle())
    }
    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        let handles = self.with_windows(|windows| {
            windows
                .iter()
                .filter_map(Weak::upgrade)
                .map(|window| window.handle)
                .collect()
        });
        Some(handles)
    }
    fn open_window(
        &self,
        handle: AnyWindowHandle,
        mut params: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        params.bounds = self.display.bounds();
        let window = KoboWindow::new(
            handle,
            params,
            self.display.clone(),
            self.scale_factor.get(),
            self.options.show_exit_button,
        );
        self.with_windows(|windows| windows.push(Rc::downgrade(&window.0)));
        Ok(Box::new(window))
    }
    fn window_appearance(&self) -> WindowAppearance {
        WindowAppearance::Light
    }
    fn open_url(&self, url: &str) {
        eprintln!("Kobo cannot open URL: {url}");
    }
    fn on_open_urls(&self, _callback: Box<dyn FnMut(Vec<String>)>) {}
    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }
    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
        _filters: Vec<FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(None));
        rx
    }
    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
        _filters: Vec<FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(None));
        rx
    }
    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }
    fn reveal_path(&self, _path: &Path) {}
    fn open_with_system(&self, _path: &Path) {}
    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.quit_callback.set(Some(callback));
    }
    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {}
    fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
        self.wake_callback.set(Some(callback));
    }
    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}
    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}
    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}
    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}
    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}
    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }
    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}
    fn compositor_name(&self) -> &'static str {
        "FBInk"
    }
    fn app_path(&self) -> Result<PathBuf> {
        Ok(std::env::current_exe()?)
    }
    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        Ok(std::env::current_exe()?
            .parent()
            .ok_or_else(|| anyhow!("executable has no parent"))?
            .join(name))
    }
    fn set_cursor_style(&self, _style: CursorStyle) {}
    fn hide_cursor_until_mouse_moves(&self) {}
    fn is_cursor_visible(&self) -> bool {
        false
    }
    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }
    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.clipboard.lock().clone()
    }
    fn write_to_clipboard(&self, item: ClipboardItem) {
        *self.clipboard.lock() = Some(item);
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn read_from_primary(&self) -> Option<ClipboardItem> {
        self.read_from_clipboard()
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn write_to_primary(&self, item: ClipboardItem) {
        self.write_to_clipboard(item);
    }
    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("credential storage is unsupported on Kobo")))
    }
    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Err(anyhow!("credential storage is unsupported on Kobo")))
    }
    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("credential storage is unsupported on Kobo")))
    }
    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(KoboKeyboardLayout)
    }
    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }
    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}

#[cfg(test)]
mod sleep_tests {
    use super::*;
    thread_local! { static WAKE_NOTIFICATIONS: Cell<usize> = const { Cell::new(0) }; }
    fn wake_ui_ready() {
        WAKE_NOTIFICATIONS.with(|count| count.set(count.get() + 1));
    }

    #[test]
    fn sleep_page_clears_previous_content_in_both_orientations() {
        for (width, height) in [(600, 800), (800, 600)] {
            let mut frame = GrayImage::from_pixel(width, height, Luma([42]));
            draw_sleep_screen(&mut frame);
            assert!(frame.pixels().all(|pixel| matches!(pixel.0[0], 0 | 255)));
            let dark: Vec<_> = frame
                .enumerate_pixels()
                .filter(|(_, _, pixel)| pixel.0[0] == 0)
                .map(|(x, y, _)| (x, y))
                .collect();
            assert!(!dark.is_empty());
            let left = dark.iter().map(|&(x, _)| x).min().unwrap();
            let right = dark.iter().map(|&(x, _)| x).max().unwrap();
            let top = dark.iter().map(|&(_, y)| y).min().unwrap();
            let bottom = dark.iter().map(|&(_, y)| y).max().unwrap();
            assert_eq!(
                left + right,
                width - 1,
                "label must be centered horizontally"
            );
            assert_eq!(
                top + bottom,
                height - 1,
                "label must be centered vertically"
            );
        }
    }

    #[test]
    fn suspend_restores_application_on_success_and_failure() {
        for handler in [
            (|| Ok(())) as fn() -> std::result::Result<(), String>,
            || Err("suspend unavailable".to_owned()),
        ] {
            let platform = KoboPlatform::new(KoboPlatformOptions {
                display: false,
                interactive: false,
                power_button_handler: Some(handler),
                wake_ui_ready_handler: Some(wake_ui_ready),
                ..Default::default()
            })
            .unwrap();
            let frame = GrayImage::from_pixel(600, 800, Luma([42]));
            platform.cached_frame.set(Some(frame.clone()));
            let mut state = platform.run_state.take().unwrap();
            WAKE_NOTIFICATIONS.with(|count| count.set(0));
            let result = platform.request_suspend(&mut state);
            assert!(state.wake_frame_pending);
            WAKE_NOTIFICATIONS.with(|count| assert_eq!(count.get(), 0));
            platform.notify_wake_ui_ready(&mut state);
            platform.notify_wake_ui_ready(&mut state);
            WAKE_NOTIFICATIONS.with(|count| assert_eq!(count.get(), 1));
            assert_eq!(result.is_ok(), handler().is_ok());
            assert!(platform.full_repaint_requested.get());
            assert!(platform.full_gc16_requested.get());
            assert_eq!(platform.last_frame().unwrap(), frame);
            platform.run_state.set(Some(state));
        }
    }
}

#[cfg(test)]
mod frame_scheduling_tests {
    use super::*;
    use gpui::{AppContext, Application, Context, IntoElement, Render, Window, WindowOptions, div};

    struct TestView;
    impl Render for TestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[test]
    fn suspend_announces_inactive_then_active_even_on_failure() {
        let platform = KoboPlatform::new(KoboPlatformOptions {
            display: false,
            interactive: false,
            power_button_handler: Some(|| Err("test suspend failure".into())),
            ..Default::default()
        }).unwrap();
        let launched = platform.clone();
        Application::new_inaccessible(platform).run(move |cx| {
            cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| TestView)).unwrap();
            let window = launched.window().unwrap();
            let events = Rc::new(RefCell::new(Vec::new()));
            let observed = events.clone();
            window.on_active_status_change(Box::new(move |active| observed.borrow_mut().push(active)));
            launched.cached_frame.set(Some(GrayImage::from_pixel(600, 800, Luma([42]))));
            let mut state = launched.run_state.take().unwrap();
            // Launch already holds the App borrow; this test observes platform
            // activation directly rather than re-entering GPUI's wake callback.
            let wake = launched.wake_callback.take();
            assert!(launched.request_suspend(&mut state).is_err());
            launched.wake_callback.set(wake);
            assert_eq!(*events.borrow(), vec![false, true]);
            assert!(window.is_active());
            launched.run_state.set(Some(state));
            cx.quit();
        });
    }

    #[test]
    fn kobo_cannot_reenable_animations() {
        let platform = KoboPlatform::new(KoboPlatformOptions {
            display: false,
            interactive: false,
            ..Default::default()
        }).unwrap();
        Application::new_inaccessible(platform).run(|cx| {
            assert!(cx.reduce_motion());
            cx.set_reduce_motion(false);
            assert!(cx.reduce_motion());
            cx.quit();
        });
    }

    #[test]
    fn frame_wakeup_is_deferred_and_demand_during_dispatch_survives() {
        let platform = KoboPlatform::new(KoboPlatformOptions {
            display: false,
            interactive: false,
            ..Default::default()
        }).unwrap();
        let platform_for_launch = platform.clone();
        Application::new_inaccessible(platform.clone()).run(move |cx| {
            cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| TestView)).unwrap();
            let window = platform_for_launch.window().unwrap();
            let original = window.0.request_frame.take();
            let calls = Rc::new(Cell::new(0));
            let callback_calls = calls.clone();
            let demand = window.0.frame_requested.clone();
            window.on_request_frame(Box::new(move |_| {
                callback_calls.set(callback_calls.get() + 1);
                demand.set(true);
            }));
            window.schedule_frame(); // Inherited no-op; must not re-enter GPUI.
            let wake = window.frame_waker().unwrap();
            wake();
            wake();
            assert_eq!(calls.get(), 0);
            assert!(window.0.frame_requested.get());
            window.request_frame(false);
            assert_eq!(calls.get(), 1);
            assert!(window.0.frame_requested.get(), "demand during a frame must survive");
            let activations = Rc::new(Cell::new(0));
            let activation_calls = activations.clone();
            let original_active = window.0.active_changed.take();
            let original_hover = window.0.hover_changed.take();
            window.on_active_status_change(Box::new(move |_| activation_calls.set(activation_calls.get() + 1)));
            window.activate();
            window.activate();
            assert_eq!(activations.get(), 0);
            window.activate_callbacks();
            window.activate_callbacks();
            assert_eq!(activations.get(), 1);
            window.0.active_changed.set(original_active);
            window.0.hover_changed.set(original_hover);

            let expected_gc16 = platform_for_launch.clone();
            window.on_request_frame(Box::new(move |_| assert!(expected_gc16.full_gc16_requested.get())));
            let mut state = platform_for_launch.run_state.take().unwrap();
            platform_for_launch.full_repaint_requested.set(true);
            platform_for_launch.full_gc16_requested.set(true);
            platform_for_launch.present_pending_full_repaint(&mut state).unwrap();
            assert!(platform_for_launch.full_gc16_requested.get(), "no image means GC16 remains pending");
            window.0.render.borrow_mut().previous_frame = Some(crate::GrayFrame::unpooled(GrayImage::from_pixel(2, 2, Luma([255]))));
            platform_for_launch.full_repaint_requested.set(true);
            platform_for_launch.present_pending_full_repaint(&mut state).unwrap();
            assert!(!platform_for_launch.full_gc16_requested.get(), "successful presentation consumes GC16");
            platform_for_launch.run_state.set(Some(state));
            window.0.request_frame.set(original);
        });
        assert!(platform.take_error().is_none());
    }
}
