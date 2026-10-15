use crate::display::AndroidDisplay;
use crate::events::ClickState;
use crate::frame::{FRAME_INTERVAL, FrameScheduler};
use crate::lifecycle::SurfaceLifecycle;
use android_activity::AndroidApp;
use android_activity::input::{TextInputState, TextSpan};
use anyhow::Context as _;
use gpui::{
    AnyWindowHandle, Bounds, Capslock, Decorations, DevicePixels, DispatchEventResult, Edges,
    GpuSpecs, Modifiers, Pixels, PlatformAtlas, PlatformDisplay, PlatformInput,
    PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    ResizeEdge, Scene, Size, WindowAppearance, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, WindowControls, WindowDecorations, WindowInsets, WindowParams, px,
};
use gpui_wgpu::{GpuContext, SurfaceSizePolicy, WgpuRenderer, WgpuRendererOptions, WgpuSurfaceConfig};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

/// Wraps the current `ANativeWindow` so `WgpuRenderer` can create a surface
/// from it via raw-window-handle.
#[derive(Clone, Debug)]
pub(crate) struct RawWindow {
    native_window: android_activity::ndk::native_window::NativeWindow,
}

impl RawWindow {
    fn physical_size(&self) -> Size<DevicePixels> {
        Size {
            width: DevicePixels(self.native_window.width()),
            height: DevicePixels(self.native_window.height()),
        }
    }
}

impl raw_window_handle::HasWindowHandle for RawWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let ptr =
            std::ptr::NonNull::new(self.native_window.ptr().as_ptr().cast::<std::ffi::c_void>())
                .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = raw_window_handle::AndroidNdkWindowHandle::new(ptr);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl raw_window_handle::HasDisplayHandle for RawWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::android())
    }
}

#[derive(Default)]
pub(crate) struct AndroidWindowCallbacks {
    pub(crate) request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    pub(crate) input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    pub(crate) active_status_change: Option<Box<dyn FnMut(bool)>>,
    pub(crate) hover_status_change: Option<Box<dyn FnMut(bool)>>,
    pub(crate) resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    pub(crate) moved: Option<Box<dyn FnMut()>>,
    pub(crate) should_close: Option<Box<dyn FnMut() -> bool>>,
    pub(crate) close: Option<Box<dyn FnOnce()>>,
    pub(crate) appearance_changed: Option<Box<dyn FnMut()>>,
    pub(crate) insets_changed: Option<Box<dyn FnMut(WindowInsets)>>,
    pub(crate) hit_test_window_control: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
}

pub(crate) struct AndroidWindowState {
    pub(crate) renderer: WgpuRenderer,
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) scale_factor: f32,
    pub(crate) title: String,
    pub(crate) input_handler: Option<PlatformInputHandler>,
    pub(crate) is_active: bool,
    pub(crate) mouse_position: Point<Pixels>,
    pub(crate) modifiers: Modifiers,
    pub(crate) capslock: Capslock,
    physical_insets: AndroidPhysicalInsets,
    insets: WindowInsets,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AndroidPhysicalEdges {
    top: u32,
    right: u32,
    bottom: u32,
    left: u32,
}

impl AndroidPhysicalEdges {
    pub(crate) fn from_android(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            top: top.max(0) as u32,
            right: right.max(0) as u32,
            bottom: bottom.max(0) as u32,
            left: left.max(0) as u32,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AndroidPhysicalInsets {
    pub(crate) safe_area: AndroidPhysicalEdges,
    pub(crate) ime: AndroidPhysicalEdges,
}

pub(crate) struct AndroidWindowInner {
    pub(crate) app: AndroidApp,
    pub(crate) gpu_context: GpuContext,
    pub(crate) state: RefCell<AndroidWindowState>,
    pub(crate) callbacks: RefCell<AndroidWindowCallbacks>,
    pub(crate) click_state: RefCell<ClickState>,
    surface: RefCell<SurfaceLifecycle<RawWindow>>,
    display: Rc<AndroidDisplay>,
    pub(crate) appearance: Cell<WindowAppearance>,
    pub(crate) pending_ime_tap: Cell<Option<Point<Pixels>>>,
    pub(crate) pending_ime_tap_had_handler: Cell<bool>,
    pub(crate) pending_ime_previous_bounds: Cell<Option<Bounds<Pixels>>>,
    pub(crate) pending_ime_registrations: Cell<u8>,
    pending_physical_size: Cell<Option<Size<DevicePixels>>>,
    input_handler_registered: Cell<bool>,
    last_text_input_state: RefCell<Option<TextInputState>>,
    frames: Rc<RefCell<FrameScheduler>>,
}

pub struct AndroidWindow {
    pub(crate) inner: Rc<AndroidWindowInner>,
    display: Rc<dyn PlatformDisplay>,
    #[allow(dead_code)]
    handle: AnyWindowHandle,
}

fn surface_config(size: Size<DevicePixels>) -> WgpuSurfaceConfig {
    WgpuSurfaceConfig {
        size,
        size_policy: SurfaceSizePolicy::Exact,
        transparent: false,
        // Mailbox avoids blocking in get_current_texture() during Android
        // lifecycle transitions; the renderer falls back to Fifo if unsupported.
        preferred_present_mode: Some(wgpu::PresentMode::Mailbox),
    }
}

pub(crate) fn scale_factor(app: &AndroidApp) -> f32 {
    app.config().density().map_or(2.0, |dpi| dpi as f32 / 160.0)
}

impl AndroidWindow {
    pub(crate) fn new(
        handle: AnyWindowHandle,
        _params: WindowParams,
        app: AndroidApp,
        gpu_context: GpuContext,
        display: Rc<AndroidDisplay>,
        appearance: WindowAppearance,
        renderer_options: WgpuRendererOptions,
    ) -> anyhow::Result<Self> {
        let native_window = app
            .native_window()
            .context("no native window: open_window must be called after the first InitWindow")?;
        let raw_window = RawWindow { native_window };
        let physical_size = raw_window.physical_size();
        let scale = scale_factor(&app);

        let renderer = WgpuRenderer::new_with_options(
            gpu_context.clone(),
            &raw_window,
            surface_config(physical_size),
            None,
            renderer_options,
        )?;

        let mut bounds = Bounds {
            origin: Point::default(),
            size: logical_size(physical_size, scale),
        };

        display.update_window_bounds(physical_size, scale, &mut bounds);
        let state = AndroidWindowState {
            renderer,
            bounds,
            scale_factor: scale,
            title: String::new(),
            input_handler: None,
            is_active: true,
            mouse_position: Point::default(),
            modifiers: Modifiers::default(),
            capslock: Capslock::default(),
            physical_insets: AndroidPhysicalInsets::default(),
            insets: WindowInsets::default(),
        };

        let inner = Rc::new(AndroidWindowInner {
            app,
            gpu_context,
            state: RefCell::new(state),
            callbacks: RefCell::new(AndroidWindowCallbacks::default()),
            click_state: RefCell::new(ClickState::default()),
            surface: RefCell::new(SurfaceLifecycle::new(raw_window)),
            display: display.clone(),
            appearance: Cell::new(appearance),
            pending_ime_tap: Cell::new(None),
            pending_ime_tap_had_handler: Cell::new(false),
            pending_ime_previous_bounds: Cell::new(None),
            pending_ime_registrations: Cell::new(0),
            pending_physical_size: Cell::new(None),
            input_handler_registered: Cell::new(false),
            last_text_input_state: RefCell::new(None),
            frames: Rc::new(RefCell::new(FrameScheduler::default())),
        });

        Ok(Self {
            inner,
            display,
            handle,
        })
    }
}

fn logical_size(physical: Size<DevicePixels>, scale: f32) -> Size<Pixels> {
    Size {
        width: px(physical.width.0 as f32 / scale),
        height: px(physical.height.0 as f32 / scale),
    }
}

fn logical_edges(edges: AndroidPhysicalEdges, scale: f32) -> Edges<Pixels> {
    if !scale.is_finite() || scale <= 0.0 {
        return Edges::default();
    }
    Edges {
        top: px(edges.top as f32 / scale),
        right: px(edges.right as f32 / scale),
        bottom: px(edges.bottom as f32 / scale),
        left: px(edges.left as f32 / scale),
    }
}

fn logical_insets(insets: AndroidPhysicalInsets, scale: f32) -> WindowInsets {
    WindowInsets {
        safe_area: logical_edges(insets.safe_area, scale),
        ime: logical_edges(insets.ime, scale),
    }
}

fn text_input_states_equal(first: &TextInputState, second: &TextInputState) -> bool {
    let span = |span: TextSpan| (span.start, span.end);
    first.text == second.text
        && span(first.selection) == span(second.selection)
        && first.compose_region.map(span) == second.compose_region.map(span)
}

impl AndroidWindowInner {
    pub(crate) fn input_handler_text(input_handler: &mut PlatformInputHandler) -> Option<String> {
        let text_length = input_handler.text_length_utf16().unwrap_or(65_536);
        let mut adjusted_range = None;
        input_handler.text_for_range(0..text_length, &mut adjusted_range)
    }

    fn text_input_state(input_handler: &mut PlatformInputHandler) -> Option<TextInputState> {
        let text = Self::input_handler_text(input_handler)?;
        let selection = input_handler.selected_text_range(true)?;
        let selection = if selection.reversed {
            TextSpan {
                start: selection.range.end,
                end: selection.range.start,
            }
        } else {
            TextSpan {
                start: selection.range.start,
                end: selection.range.end,
            }
        };
        let compose_region = input_handler.marked_text_range().map(|range| TextSpan {
            start: range.start,
            end: range.end,
        });
        Some(TextInputState {
            text,
            selection,
            compose_region,
        })
    }

    pub(crate) fn sync_text_input_state(&self) {
        // Handler queries re-enter GPUI, so this must run after draw/input callbacks.
        let registered = self.input_handler_registered.replace(false);
        let text_input_state = self.with_input_handler(Self::text_input_state).flatten();
        if let Some(text_input_state) = text_input_state {
            let changed = self
                .last_text_input_state
                .borrow()
                .as_ref()
                .is_none_or(|previous| !text_input_states_equal(previous, &text_input_state));
            if changed {
                self.app.set_text_input_state(text_input_state.clone());
                self.last_text_input_state.replace(Some(text_input_state));
            }
        } else if self.state.borrow().input_handler.is_none()
            && self.last_text_input_state.borrow_mut().take().is_some()
        {
            self.app.hide_soft_input(false);
        }

        if registered && let Some(tap_position) = self.pending_ime_tap.get() {
            let input_bounds = self
                .with_input_handler(|handler| handler.element_bounds())
                .flatten();
            let previous_bounds = self.pending_ime_previous_bounds.get();
            let changed_handler = self.pending_ime_tap_had_handler.get()
                && previous_bounds.is_some()
                && input_bounds.is_some()
                && previous_bounds != input_bounds;
            let tapped_input = self.state.borrow().input_handler.is_some()
                && (!self.pending_ime_tap_had_handler.get()
                    || changed_handler
                    || input_bounds.is_some_and(|bounds| bounds.contains(&tap_position)));
            let registrations = self.pending_ime_registrations.get().saturating_sub(1);
            self.pending_ime_registrations.set(registrations);
            if tapped_input || registrations == 0 {
                self.pending_ime_tap.set(None);
                self.pending_ime_tap_had_handler.set(false);
                self.pending_ime_previous_bounds.set(None);
                self.pending_ime_registrations.set(0);
            }
            if tapped_input {
                self.app.show_soft_input(false);
            }
        }
    }

    pub(crate) fn cancel_pending_input(&self) {
        self.pending_ime_tap.set(None);
        self.pending_ime_tap_had_handler.set(false);
        self.pending_ime_previous_bounds.set(None);
        self.pending_ime_registrations.set(0);
        *self.click_state.borrow_mut() = ClickState::default();
    }

    pub(crate) fn accept_text_input_state(&self, state: &TextInputState) {
        self.last_text_input_state.replace(Some(state.clone()));
    }

    fn insets(&self) -> WindowInsets {
        self.state.borrow().insets.clone()
    }

    fn notify_insets_changed(&self, insets: WindowInsets) {
        if let Some(callback) = self.callbacks.borrow_mut().insets_changed.as_mut() {
            callback(insets);
        }
    }

    pub(crate) fn update_physical_insets(&self, physical_insets: AndroidPhysicalInsets) {
        let insets = {
            let mut state = self.state.borrow_mut();
            let insets = logical_insets(physical_insets, state.scale_factor);
            state.physical_insets = physical_insets;
            if state.insets == insets {
                return;
            }
            state.insets = insets.clone();
            insets
        };
        self.notify_insets_changed(insets);
    }

    /// Called on `MainEvent::InitWindow` after the native window was destroyed
    /// and recreated (backgrounding, rotation). Recreates the wgpu surface on
    /// the same device so cached atlas textures stay valid.
    pub(crate) fn handle_surface_created(&self) {
        let Some(native_window) = self.app.native_window() else {
            log::error!("InitWindow received but native_window() returned None");
            return;
        };
        self.state.borrow_mut().renderer.unconfigure_surface();
        self.surface
            .borrow_mut()
            .replace_window(RawWindow { native_window });
        self.update_size();
        self.schedule_frame(true);
    }

    /// Called on `MainEvent::TerminateWindow`: the `ANativeWindow` is about to
    /// be destroyed, so rendering must stop until a new surface arrives.
    pub(crate) fn handle_surface_destroyed(&self) {
        self.state.borrow_mut().renderer.unconfigure_surface();
        self.surface.borrow_mut().clear();
        self.pending_physical_size.set(None);
    }

    pub(crate) fn update_size(&self) {
        let scale = scale_factor(&self.app);
        let (physical_size, changed, changed_insets) = {
            let mut state = self.state.borrow_mut();
            let surface = self.surface.borrow();
            let Some(raw_window) = surface.window() else {
                return;
            };
            let physical_size = raw_window.physical_size();
            let changed =
                self.display
                    .update_window_bounds(physical_size, scale, &mut state.bounds)
                    || state.scale_factor != scale;
            state.scale_factor = scale;
            let insets = logical_insets(state.physical_insets, scale);
            let changed_insets = (state.insets != insets).then_some(insets.clone());
            state.insets = insets;
            (physical_size, changed, changed_insets)
        };

        if changed {
            self.pending_physical_size.set(Some(physical_size));

            let logical = logical_size(physical_size, scale);
            let mut callbacks = self.callbacks.borrow_mut();
            if let Some(ref mut callback) = callbacks.resize {
                callback(logical, scale);
            }
        }
        if let Some(insets) = changed_insets {
            self.notify_insets_changed(insets);
        }
    }

    pub(crate) fn schedule_frame(&self, force_render: bool) {
        self.frames.borrow_mut().request(force_render);
        self.app.create_waker().wake();
    }

    pub(crate) fn frame_deadline(&self, now: Instant) -> Option<Instant> {
        // A missing native surface is restored by an Android lifecycle event.
        self.surface.borrow().window()?;
        self.frames.borrow().deadline(now)
    }

    pub(crate) fn request_pending_frame(&self, now: Instant) {
        if self.surface.borrow().window().is_none() {
            return;
        }
        let force_render = self.frames.borrow_mut().take_due(now);
        if let Some(force_render) = force_render {
            self.request_frame(force_render);
        }
    }

    fn request_frame(&self, force_render: bool) {
        let force_render = {
            let mut state = self.state.borrow_mut();
            let mut surface = self.surface.borrow_mut();
            let recovering = state.renderer.device_lost();
            if recovering {
                if surface.is_ready() {
                    surface.invalidate();
                }
                if state.renderer.recovery_pending() {
                    self.frames
                        .borrow_mut()
                        .defer_until(Instant::now() + FRAME_INTERVAL);
                    return;
                }
            }
            let configured = surface.try_configure(Instant::now(), |raw_window| {
                if recovering {
                    state.renderer.recover(raw_window)
                } else {
                    let instance = self
                        .gpu_context
                        .borrow()
                        .as_ref()
                        .context("surface recreation requested before the GPU context exists")?
                        .instance
                        .clone();
                    state.renderer.replace_surface(
                        raw_window,
                        surface_config(raw_window.physical_size()),
                        &instance,
                    )
                }
            });
            let configured = match configured {
                Ok(configured) => configured,
                Err(error) => {
                    if !recovering || !state.renderer.recovery_pending() {
                        log::error!("failed to restore Android surface; will retry: {error:#}");
                    }
                    let retry = surface
                        .retry_deadline()
                        .unwrap_or_else(|| Instant::now() + FRAME_INTERVAL);
                    self.frames.borrow_mut().defer_until(retry);
                    return;
                }
            };
            if !surface.is_ready() {
                let retry = surface
                    .retry_deadline()
                    .unwrap_or_else(|| Instant::now() + FRAME_INTERVAL);
                self.frames.borrow_mut().defer_until(retry);
                return;
            }
            state.renderer.needs_redraw() || configured || force_render
        };
        let mut callbacks = self.callbacks.borrow_mut();
        if let Some(ref mut callback) = callbacks.request_frame {
            callback(RequestFrameOptions {
                require_presentation: force_render,
                force_render,
            });
        }
        drop(callbacks);
        // A failed GPU submission may need another frame even if GPUI is clean.
        let mut state = self.state.borrow_mut();
        if state.renderer.needs_redraw() || state.renderer.device_lost() {
            self.frames
                .borrow_mut()
                .defer_until(Instant::now() + FRAME_INTERVAL);
        }
    }

    pub(crate) fn set_active(&self, is_active: bool) {
        self.state.borrow_mut().is_active = is_active;
        let mut callbacks = self.callbacks.borrow_mut();
        if let Some(ref mut callback) = callbacks.active_status_change {
            callback(is_active);
        }
    }

    pub(crate) fn set_appearance(&self, appearance: WindowAppearance) {
        if self.appearance.replace(appearance) == appearance {
            return;
        }
        let mut callbacks = self.callbacks.borrow_mut();
        if let Some(ref mut callback) = callbacks.appearance_changed {
            callback();
        }
    }

    pub(crate) fn dispatch_input(&self, input: PlatformInput) -> Option<DispatchEventResult> {
        let mut callbacks = self.callbacks.borrow_mut();
        let callback = callbacks.input.as_mut()?;
        Some(callback(input))
    }

    pub(crate) fn with_input_handler<R>(
        &self,
        f: impl FnOnce(&mut PlatformInputHandler) -> R,
    ) -> Option<R> {
        let mut handler = self.state.borrow_mut().input_handler.take()?;
        let result = f(&mut handler);
        self.state.borrow_mut().input_handler = Some(handler);
        Some(result)
    }
}

impl raw_window_handle::HasWindowHandle for AndroidWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let surface = self.inner.surface.borrow();
        let raw_window = surface
            .window()
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = raw_window.window_handle()?.as_raw();
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle) })
    }
}

impl raw_window_handle::HasDisplayHandle for AndroidWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::android())
    }
}

impl PlatformWindow for AndroidWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.inner.state.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.inner.state.borrow().bounds.size
    }

    fn resize(&mut self, _size: Size<Pixels>) {}

    fn scale_factor(&self) -> f32 {
        self.inner.state.borrow().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        self.inner.appearance.get()
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.inner.state.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.inner.state.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.inner.state.borrow().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.inner.state.borrow_mut().input_handler = Some(input_handler);
        self.inner.input_handler_registered.set(true);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.inner.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {
        self.inner.state.borrow_mut().is_active = true;
    }

    fn is_active(&self) -> bool {
        self.inner.state.borrow().is_active
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        self.inner.state.borrow_mut().title = title.to_owned();
    }

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        // GPUI may call this while the window state/callbacks are borrowed.
        // Only record demand here; the event loop delivers it after unwinding.
        let frames = self.inner.frames.clone();
        let waker = self.inner.app.create_waker();
        Some(Rc::new(move || {
            frames.borrow_mut().request(false);
            waker.wake();
        }))
    }

    fn schedule_frame(&self) {
        self.inner.schedule_frame(false);
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.inner.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.inner.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.inner.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.inner.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.inner.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.inner.callbacks.borrow_mut().hit_test_window_control = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        if !self.inner.surface.borrow().is_ready() {
            return;
        }
        let mut state = self.inner.state.borrow_mut();
        if !state.renderer.device_lost() {
            if let Some(physical_size) = self.inner.pending_physical_size.take() {
                state.renderer.update_drawable_size(physical_size);
            }
            state.renderer.draw(scene);
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.inner.state.borrow().renderer.sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        self.inner
            .state
            .borrow()
            .renderer
            .supports_dual_source_blending()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        Some(self.inner.state.borrow().renderer.gpu_specs())
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn request_decorations(&self, _decorations: WindowDecorations) {}

    fn show_window_menu(&self, _position: Point<Pixels>) {}

    fn start_window_move(&self) {}

    fn start_window_resize(&self, _edge: ResizeEdge) {}

    fn window_decorations(&self) -> Decorations {
        Decorations::Server
    }

    fn set_app_id(&mut self, _app_id: &str) {}

    fn window_controls(&self) -> WindowControls {
        WindowControls {
            fullscreen: false,
            maximize: false,
            minimize: false,
            window_menu: false,
        }
    }

    fn set_client_inset(&self, _inset: Pixels) {}

    fn insets(&self) -> WindowInsets {
        self.inner.insets()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.inner.callbacks.borrow_mut().insets_changed = Some(callback);
    }
}
