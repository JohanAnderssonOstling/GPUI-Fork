use crate::dispatcher::WebDispatcher;
use crate::display::WebDisplay;
use crate::events::EventListenerHandle;
use crate::http_client::FetchHttpClient;
use crate::keyboard::WebKeyboardLayout;
use crate::window::WebWindow;
use anyhow::Result;
use futures::channel::oneshot;
use gpui::{
    Action, AnyWindowHandle, BackgroundExecutor, ClipboardEntry, ClipboardItem, ClipboardReadError,
    ClipboardString, CursorStyle, DummyKeyboardMapper, ForegroundExecutor, Image, ImageFormat,
    Keymap, Menu, MenuItem, PathPromptOptions, Platform, PlatformDisplay, PlatformKeyboardLayout,
    PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, SelectedDirectory,
    SelectedDirectoryFile, SelectedFile, Task, ThermalState, WindowAppearance, WindowKind,
    WindowParams, popup::PopupNotSupportedError,
};
use gpui_wgpu::{PreparedWebGraphics, WebBackendPreference, WgpuContext, wgpu};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
const gpuiDirectoryFiles = new Map();
let gpuiDirectoryFileSequence = 0;

function retainDirectoryFile(file) {
    const token = `gpui-directory-${Date.now()}-${gpuiDirectoryFileSequence++}`;
    gpuiDirectoryFiles.set(token, { file, createdAt: Date.now() });
    return token;
}

function discardExpiredDirectoryFiles() {
    const cutoff = Date.now() - 60 * 60 * 1000;
    for (const [token, retained] of gpuiDirectoryFiles) {
        if (retained.createdAt < cutoff) gpuiDirectoryFiles.delete(token);
    }
}

export async function gpuiReadDirectoryFile(token, offset, length) {
    const retained = gpuiDirectoryFiles.get(token);
    if (!retained) throw new Error("selected browser file is no longer available");
    if (!Number.isSafeInteger(offset) || offset < 0 || !Number.isInteger(length) || length < 1 || length > 256 * 1024)
        throw new Error("invalid selected-file range");
    retained.createdAt = Date.now();
    return new Uint8Array(await retained.file.slice(offset, offset + length).arrayBuffer());
}
export function gpuiTakeDirectoryFile(token) {
    const entry = gpuiDirectoryFiles.get(token);
    if (!entry) throw new Error('Selected file is no longer available');
    gpuiDirectoryFiles.delete(token);
    return entry.file;
}
export function gpuiReleaseDirectoryFile(token) { gpuiDirectoryFiles.delete(token); }

export function gpuiPromptForFiles(accept, multiple) {
    return new Promise((resolve, reject) => {
        const input = document.createElement("input");
        input.type = "file";
        input.accept = accept;
        input.multiple = multiple;
        input.style.display = "none";
        document.body.appendChild(input);

        let settled = false;
        const finish = (value) => {
            if (settled) return;
            settled = true;
            input.remove();
            resolve(value);
        };
        input.addEventListener("cancel", () => finish(null), { once: true });
        input.addEventListener("change", () => {
            try {
                discardExpiredDirectoryFiles();
                const selected = Array.from(input.files ?? []).map(file => ({
                    name: file.name,
                    token: retainDirectoryFile(file),
                }));
                finish(selected);
            } catch (error) {
                input.remove();
                reject(error);
            }
        }, { once: true });

        try {
            if (typeof input.showPicker === "function") input.showPicker();
            else input.click();
        } catch (error) {
            input.remove();
            reject(error);
        }
    });
}

export function gpuiPromptForDirectory(accept) {
    return new Promise((resolve, reject) => {
        discardExpiredDirectoryFiles();
        const input = document.createElement("input");
        input.type = "file";
        input.webkitdirectory = true;
        input.multiple = true;
        input.style.display = "none";
        document.body.appendChild(input);

        const extensions = new Set(
            accept.split(",")
                .map(extension => extension.trim().toLowerCase())
                .filter(Boolean),
        );
        const accepted = file => extensions.size === 0 ||
            Array.from(extensions).some(extension =>
                file.name.toLowerCase().endsWith(extension),
            );
        let settled = false;
        const finish = value => {
            if (settled) return;
            settled = true;
            input.remove();
            resolve(value);
        };
        input.addEventListener("cancel", () => finish(null), { once: true });
        input.addEventListener("change", () => {
            try {
                const files = Array.from(input.files ?? []);
                const rootName = files
                    .map(file => file.webkitRelativePath.split("/")[0])
                    .find(Boolean);
                if (!rootName) {
                    finish(null);
                    return;
                }
                const entries = files.map(file => ({
                    path: file.webkitRelativePath,
                    size: file.size,
                    token: accepted(file) ? retainDirectoryFile(file) : null,
                }));
                finish({ name: rootName, entries });
            } catch (error) {
                input.remove();
                reject(error);
            }
        }, { once: true });

        try {
            if (typeof input.showPicker === "function") input.showPicker();
            else input.click();
        } catch (error) {
            input.remove();
            reject(error);
        }
    });
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = gpuiPromptForFiles)]
    fn prompt_for_browser_files(accept: &str, multiple: bool) -> js_sys::Promise;

    #[wasm_bindgen(js_name = gpuiPromptForDirectory)]
    fn prompt_for_browser_directory(accept: &str) -> js_sys::Promise;

    #[wasm_bindgen(js_name = gpuiReadDirectoryFile)]
    fn read_browser_directory_file(token: &str, offset: f64, length: u32) -> js_sys::Promise;
    #[wasm_bindgen(js_name = gpuiTakeDirectoryFile)]
    fn take_browser_directory_file(token: &str) -> JsValue;
    #[wasm_bindgen(js_name = gpuiReleaseDirectoryFile)]
    fn release_browser_directory_file(token: &str);
}

static BUNDLED_FONTS: &[&[u8]] = &[
    include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"),
    include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf"),
    include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Bold.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Italic.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-BoldItalic.ttf"),
];

pub struct WebPlatform {
    browser_window: web_sys::Window,
    dispatcher: Arc<WebDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    active_window: Rc<RefCell<Option<AnyWindowHandle>>>,
    active_display: Rc<dyn PlatformDisplay>,
    callbacks: RefCell<WebPlatformCallbacks>,
    backend_preference: WebBackendPreference,
    wgpu_context: Rc<RefCell<Option<WgpuContext>>>,
    prepared_window: Rc<RefCell<Option<PreparedWebWindow>>>,
    window_lifecycle: Rc<Cell<WebWindowLifecycle>>,
    cursor_visible: Rc<Cell<bool>>,
    last_cursor_css: Rc<Cell<&'static str>>,
    _cursor_restore_listeners: Vec<EventListenerHandle>,
}

struct PreparedWebWindow {
    canvas: web_sys::HtmlCanvasElement,
    surface: wgpu::Surface<'static>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WebWindowLifecycle {
    Available,
    Open,
    Closed,
    Unavailable,
}

#[derive(Debug)]
pub enum WebWindowError {
    AlreadyOpen,
    ReopeningUnsupported,
    UnsupportedWindowKind(&'static str),
    /// Graphics initialization has not completed yet; retrying after it
    /// finishes (e.g. from the `Platform::run` callback) can succeed.
    GraphicsInitializationPending,
    /// Graphics initialization or an earlier window creation failed;
    /// retrying cannot succeed.
    GraphicsUnavailable,
}

impl std::fmt::Display for WebWindowError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyOpen => formatter.write_str(
                "GPUI web supports only one top-level window; a window is already open",
            ),
            Self::ReopeningUnsupported => formatter.write_str(
                "reopening the GPUI web top-level window after it closes is not supported",
            ),
            Self::UnsupportedWindowKind(kind) => write!(
                formatter,
                "GPUI web does not support {kind} as a separate top-level window; render it inside the normal window instead"
            ),
            Self::GraphicsInitializationPending => formatter.write_str(
                "browser graphics initialization has not completed yet; open windows from the callback passed to Platform::run",
            ),
            Self::GraphicsUnavailable => formatter.write_str(
                "browser graphics are unavailable because graphics initialization or an earlier window creation failed",
            ),
        }
    }
}

impl std::error::Error for WebWindowError {}

#[derive(Default)]
struct WebPlatformCallbacks {
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    quit: Option<Box<dyn FnMut()>>,
    reopen: Option<Box<dyn FnMut()>>,
    app_menu_action: Option<Box<dyn FnMut(&dyn Action)>>,
    will_open_app_menu: Option<Box<dyn FnMut()>>,
    validate_app_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
    keyboard_layout_change: Option<Box<dyn FnMut()>>,
    thermal_state_change: Option<Box<dyn FnMut()>>,
}

impl WebPlatform {
    pub fn new(allow_multi_threading: bool) -> Self {
        Self::new_with_backend(allow_multi_threading, WebBackendPreference::Auto)
    }

    pub fn new_with_backend(
        allow_multi_threading: bool,
        backend_preference: WebBackendPreference,
    ) -> Self {
        let browser_window =
            web_sys::window().expect("must be running in a browser window context");
        let dispatcher = Arc::new(WebDispatcher::new(
            browser_window.clone(),
            allow_multi_threading,
        ));
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        let text_system = Arc::new(gpui_wgpu::CosmicTextSystem::new_without_system_fonts(
            "IBM Plex Sans",
        ));
        let fonts = BUNDLED_FONTS
            .iter()
            .map(|bytes| Cow::Borrowed(*bytes))
            .collect();
        if let Err(error) = text_system.add_fonts(fonts) {
            log::error!("failed to load bundled fonts: {error:#}");
        }
        let text_system: Arc<dyn PlatformTextSystem> = text_system;
        let active_display: Rc<dyn PlatformDisplay> =
            Rc::new(WebDisplay::new(browser_window.clone()));

        let cursor_visible = Rc::new(Cell::new(true));
        let last_cursor_css = Rc::new(Cell::new("default"));
        let cursor_restore_listeners = cursor_restore_listeners(
            &browser_window,
            cursor_visible.clone(),
            last_cursor_css.clone(),
        );

        Self {
            browser_window,
            dispatcher,
            background_executor,
            foreground_executor,
            text_system,
            active_window: Rc::new(RefCell::new(None)),
            active_display,
            callbacks: RefCell::new(WebPlatformCallbacks::default()),
            backend_preference,
            wgpu_context: Rc::new(RefCell::new(None)),
            prepared_window: Rc::new(RefCell::new(None)),
            window_lifecycle: Rc::new(Cell::new(WebWindowLifecycle::Available)),
            cursor_visible,
            last_cursor_css,
            _cursor_restore_listeners: cursor_restore_listeners,
        }
    }

    /// Returns an HTTP client that runs browser Fetch operations on this platform's main thread.
    pub fn fetch_http_client(&self) -> FetchHttpClient {
        FetchHttpClient::new(self.dispatcher.clone())
    }

    /// Returns a browser Fetch HTTP client with the given reported user agent.
    pub fn fetch_http_client_with_user_agent(
        &self,
        user_agent: &str,
    ) -> anyhow::Result<FetchHttpClient> {
        FetchHttpClient::with_user_agent(self.dispatcher.clone(), user_agent)
    }
}

async fn initialize_graphics(
    browser_window: &web_sys::Window,
    preference: WebBackendPreference,
) -> anyhow::Result<(
    web_sys::HtmlCanvasElement,
    WgpuContext,
    wgpu::Surface<'static>,
)> {
    match preference {
        WebBackendPreference::Auto => {
            let webgpu_canvas = WebWindow::prepare_canvas(browser_window)?;
            let webgpu_result = if wgpu::util::is_browser_webgpu_supported().await {
                WgpuContext::new_web(&webgpu_canvas, WebBackendPreference::WebGpu).await
            } else {
                Err(anyhow::anyhow!(
                    "browser WebGPU probe did not return a usable adapter"
                ))
            };
            match webgpu_result {
                Ok(PreparedWebGraphics { context, surface }) => {
                    return Ok((webgpu_canvas, context, surface));
                }
                Err(webgpu_error) => {
                    let canvas: &web_sys::Element = webgpu_canvas.as_ref();
                    canvas.remove();
                    log::warn!(
                        "WebGPU initialization failed; falling back to WebGL2: {webgpu_error:#}"
                    );

                    let webgl_canvas =
                        WebWindow::prepare_canvas(browser_window).map_err(|error| {
                            anyhow::anyhow!(
                                "WebGPU initialization failed: {webgpu_error:#}. \
                             Failed to prepare a replacement canvas for WebGL2: {error:#}"
                            )
                        })?;
                    match WgpuContext::new_web(&webgl_canvas, WebBackendPreference::WebGl).await {
                        Ok(PreparedWebGraphics { context, surface }) => {
                            Ok((webgl_canvas, context, surface))
                        }
                        Err(webgl_error) => {
                            let canvas: &web_sys::Element = webgl_canvas.as_ref();
                            canvas.remove();
                            Err(anyhow::anyhow!(
                                "No browser graphics backend could be initialized. \
                                 Tried WebGPU, then WebGL2. \
                                 WebGPU failure: {webgpu_error:#}. \
                                 WebGL2 failure: {webgl_error:#}"
                            ))
                        }
                    }
                }
            }
        }
        WebBackendPreference::WebGpu | WebBackendPreference::WebGl => {
            let backend_name = if preference == WebBackendPreference::WebGpu {
                "WebGPU"
            } else {
                "WebGL2"
            };
            let canvas = WebWindow::prepare_canvas(browser_window)?;
            match WgpuContext::new_web(&canvas, preference).await {
                Ok(PreparedWebGraphics { context, surface }) => Ok((canvas, context, surface)),
                Err(error) => {
                    let canvas: &web_sys::Element = canvas.as_ref();
                    canvas.remove();
                    Err(anyhow::anyhow!(
                        "No browser graphics backend could be initialized. \
                         Only {backend_name} was tried because the application requested \
                         it explicitly. {backend_name} failure: {error:#}"
                    ))
                }
            }
        }
    }
}

impl Platform for WebPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        let wgpu_context = self.wgpu_context.clone();
        let prepared_window = self.prepared_window.clone();
        let window_lifecycle = self.window_lifecycle.clone();
        let browser_window = self.browser_window.clone();
        let backend_preference = self.backend_preference;
        wasm_bindgen_futures::spawn_local(async move {
            match initialize_graphics(&browser_window, backend_preference).await {
                Ok((canvas, context, surface)) => {
                    log::info!(
                        "Browser graphics initialized successfully with {:?}",
                        context.backend()
                    );
                    *wgpu_context.borrow_mut() = Some(context);
                    *prepared_window.borrow_mut() = Some(PreparedWebWindow { canvas, surface });
                    on_finish_launching();
                }
                Err(error) => {
                    window_lifecycle.set(WebWindowLifecycle::Unavailable);
                    log::error!("Failed to initialize browser graphics: {error:#}");
                    show_graphics_unavailable_message(&browser_window, &error);
                }
            }
        });
    }

    fn quit(&self) {
        log::warn!("WebPlatform::quit called, but quitting is not supported in the browser .");
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<std::ffi::OsString>) {}

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.active_display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.active_display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        *self.active_window.borrow()
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        params: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        match &params.kind {
            WindowKind::Normal => {}
            WindowKind::AnchoredPopup(_) => return Err(PopupNotSupportedError.into()),
            WindowKind::PopUp => {
                return Err(WebWindowError::UnsupportedWindowKind("popup windows").into());
            }
            WindowKind::Floating => {
                return Err(WebWindowError::UnsupportedWindowKind("floating windows").into());
            }
            WindowKind::Dialog => {
                return Err(WebWindowError::UnsupportedWindowKind("dialog windows").into());
            }
        }

        match self.window_lifecycle.get() {
            WebWindowLifecycle::Open => return Err(WebWindowError::AlreadyOpen.into()),
            WebWindowLifecycle::Closed => {
                return Err(WebWindowError::ReopeningUnsupported.into());
            }
            WebWindowLifecycle::Unavailable => {
                return Err(WebWindowError::GraphicsUnavailable.into());
            }
            WebWindowLifecycle::Available => {}
        }

        let context_ref = self.wgpu_context.borrow();
        let context = context_ref
            .as_ref()
            .ok_or(WebWindowError::GraphicsInitializationPending)?;
        let prepared_window = self
            .prepared_window
            .borrow_mut()
            .take()
            .ok_or(WebWindowError::GraphicsInitializationPending)?;
        let canvas = prepared_window.canvas;
        let canvas_for_cleanup = canvas.clone();

        let window = WebWindow::new(
            handle,
            params,
            context,
            canvas,
            prepared_window.surface,
            self.browser_window.clone(),
            self.window_lifecycle.clone(),
            self.active_window.clone(),
        );
        match window {
            Ok(window) => {
                self.window_lifecycle.set(WebWindowLifecycle::Open);
                *self.active_window.borrow_mut() = Some(handle);
                Ok(Box::new(window))
            }
            Err(error) => {
                let canvas: &web_sys::Element = canvas_for_cleanup.as_ref();
                canvas.remove();
                self.window_lifecycle.set(WebWindowLifecycle::Unavailable);
                Err(error)
            }
        }
    }

    fn window_appearance(&self) -> WindowAppearance {
        let Ok(Some(media_query)) = self
            .browser_window
            .match_media("(prefers-color-scheme: dark)")
        else {
            return WindowAppearance::Light;
        };
        if media_query.matches() {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        }
    }

    fn open_url(&self, url: &str) {
        if let Err(error) = self.browser_window.open_with_url(url) {
            log::warn!("Failed to open URL '{url}': {error:?}");
        }
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.callbacks.borrow_mut().open_urls = Some(callback);
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
        _filters: Vec<gpui::FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow::anyhow!(
            "prompt_for_paths is not supported on the web"
        )))
        .ok();
        rx
    }

    fn prompt_for_files(
        &self,
        options: PathPromptOptions,
        filters: Vec<gpui::FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<Vec<SelectedFile>>>> {
        let (sender, receiver) = oneshot::channel();
        if !options.files || options.directories {
            sender
                .send(Err(anyhow::anyhow!(
                    "web file-content prompts support files only"
                )))
                .ok();
            return receiver;
        }

        let accept = filters
            .iter()
            .flat_map(|filter| filter.extensions.iter())
            .map(|extension| format!(".{}", extension.trim_start_matches('.')))
            .collect::<Vec<_>>()
            .join(",");
        let promise = prompt_for_browser_files(&accept, options.multiple);
        wasm_bindgen_futures::spawn_local(async move {
            let result = wasm_bindgen_futures::JsFuture::from(promise)
                .await
                .map_err(|error| anyhow::anyhow!(js_error_message(&error)))
                .and_then(selected_files_from_js);
            sender.send(result).ok();
        });
        receiver
    }

    fn prompt_for_directories(
        &self,
        options: PathPromptOptions,
        filters: Vec<gpui::FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<Vec<SelectedDirectory>>>> {
        let (sender, receiver) = oneshot::channel();
        if options.files || !options.directories {
            sender
                .send(Err(anyhow::anyhow!(
                    "web directory-content prompts support directories only"
                )))
                .ok();
            return receiver;
        }

        let accept = filters
            .iter()
            .flat_map(|filter| filter.extensions.iter())
            .map(|extension| format!(".{}", extension.trim_start_matches('.')))
            .collect::<Vec<_>>()
            .join(",");
        let promise = prompt_for_browser_directory(&accept);
        wasm_bindgen_futures::spawn_local(async move {
            let result = wasm_bindgen_futures::JsFuture::from(promise)
                .await
                .map_err(|error| anyhow::anyhow!(js_error_message(&error)))
                .and_then(selected_directory_from_js)
                .map(|directory| directory.map(|directory| vec![directory]));
            sender.send(result).ok();
        });
        receiver
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
        _filters: Vec<gpui::FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (sender, receiver) = oneshot::channel();
        sender
            .send(Err(anyhow::anyhow!(
                "prompt_for_new_path is not supported on the web"
            )))
            .ok();
        receiver
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {}

    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().quit = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().reopen = Some(callback);
    }

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.callbacks.borrow_mut().app_menu_action = Some(callback);
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().will_open_app_menu = Some(callback);
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.callbacks.borrow_mut().validate_app_menu_command = Some(callback);
    }

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().thermal_state_change = Some(callback);
    }

    fn compositor_name(&self) -> &'static str {
        "Web"
    }

    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow::anyhow!("app_path is not available on the web"))
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow::anyhow!(
            "path_for_auxiliary_executable is not available on the web"
        ))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        let css_cursor = match style {
            CursorStyle::Arrow => "default",
            CursorStyle::IBeam => "text",
            CursorStyle::Crosshair => "crosshair",
            CursorStyle::ClosedHand => "grabbing",
            CursorStyle::OpenHand => "grab",
            CursorStyle::PointingHand => "pointer",
            CursorStyle::ResizeLeft | CursorStyle::ResizeRight | CursorStyle::ResizeLeftRight => {
                "ew-resize"
            }
            CursorStyle::ResizeUp | CursorStyle::ResizeDown | CursorStyle::ResizeUpDown => {
                "ns-resize"
            }
            CursorStyle::ResizeUpLeftDownRight => "nesw-resize",
            CursorStyle::ResizeUpRightDownLeft => "nwse-resize",
            CursorStyle::ResizeColumn => "col-resize",
            CursorStyle::ResizeRow => "row-resize",
            CursorStyle::IBeamCursorForVerticalLayout => "vertical-text",
            CursorStyle::OperationNotAllowed => "not-allowed",
            CursorStyle::DragLink => "alias",
            CursorStyle::DragCopy => "copy",
            CursorStyle::ContextualMenu => "context-menu",
        };

        self.last_cursor_css.set(css_cursor);
        if self.cursor_visible.get() {
            set_body_cursor(&self.browser_window, css_cursor);
        }
    }

    fn hide_cursor_until_mouse_moves(&self) {
        if !self.cursor_visible.replace(false) {
            return;
        }
        set_body_cursor(&self.browser_window, "none");
    }

    fn is_cursor_visible(&self) -> bool {
        self.cursor_visible.get()
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        None
    }

    fn read_from_clipboard_async(&self) -> Task<Result<Option<ClipboardItem>, ClipboardReadError>> {
        let navigator = self.browser_window.navigator();
        // `navigator.clipboard` is undefined outside secure contexts; probing
        // first avoids a wasm-bindgen abort from invoking a method on
        // `undefined`.
        let clipboard_available = js_sys::Reflect::get(navigator.as_ref(), &"clipboard".into())
            .is_ok_and(|clipboard| !clipboard.is_undefined() && !clipboard.is_null());
        if !clipboard_available {
            return Task::ready(Err(ClipboardReadError::Unavailable));
        }
        // `read()` must be called synchronously so it is still covered by the
        // user activation (e.g. the click on a context-menu item) that
        // triggered this read.
        let read = navigator.clipboard().read();
        self.foreground_executor.spawn(async move {
            let items = wasm_bindgen_futures::JsFuture::from(read)
                .await
                .map_err(clipboard_read_rejection_error)?;
            let mut entries = Vec::new();
            let mut saw_unsupported_type = false;
            for item in js_sys::Array::from(&items).iter() {
                let item: web_sys::ClipboardItem = item.unchecked_into();
                for mime_type in item.types().iter() {
                    let Some(mime_type) = mime_type.as_string() else {
                        continue;
                    };
                    // Only fetch blobs for types we can convert; copies from
                    // other web apps routinely carry `text/html` and
                    // `web application/...` custom formats whose `getType`
                    // fetches would be wasted or rejected.
                    if mime_type == "text/plain" {
                        match read_clipboard_item_text(&item, &mime_type).await {
                            Ok(text) if !text.is_empty() => {
                                entries.push(ClipboardEntry::String(ClipboardString::new(text)));
                            }
                            Ok(_) => {}
                            Err(error) => log_clipboard_entry_error(&mime_type, &error),
                        }
                    } else if let Some(format) = ImageFormat::from_mime_type(&mime_type) {
                        match read_clipboard_item_bytes(&item, &mime_type).await {
                            Ok(bytes) => {
                                entries
                                    .push(ClipboardEntry::Image(Image::from_bytes(format, bytes)));
                            }
                            Err(error) => log_clipboard_entry_error(&mime_type, &error),
                        }
                    } else {
                        saw_unsupported_type = true;
                    }
                }
            }
            if !entries.is_empty() {
                Ok(Some(ClipboardItem { entries }))
            } else if saw_unsupported_type {
                Err(ClipboardReadError::UnsupportedContent)
            } else {
                Ok(None)
            }
        })
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        if let Some(text) = item.text()
            && let Some(window) = web_sys::window()
        {
            // Fire-and-forget; called synchronously inside the user's input
            // event, which satisfies the browser's user-activation requirement.
            drop(window.navigator().clipboard().write_text(&text));
        }
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "credential storage is not available on the web"
        )))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "credential storage is not available on the web"
        )))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(WebKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().keyboard_layout_change = Some(callback);
    }
}

/// Maps a `navigator.clipboard.read()` rejection to a [`ClipboardReadError`].
///
/// Only `NotAllowedError` and `SecurityError` mean the user or browser
/// refused access; other rejections (e.g. `DataError`) indicate content the
/// clipboard could not represent, where "allow clipboard access" guidance
/// would mislead.
fn clipboard_read_rejection_error(error: JsValue) -> ClipboardReadError {
    match js_error_name(&error).as_deref() {
        Some("NotAllowedError") | Some("SecurityError") => {
            ClipboardReadError::Denied(js_error_message(&error))
        }
        _ => ClipboardReadError::UnsupportedContent,
    }
}

async fn read_clipboard_item_blob(
    item: &web_sys::ClipboardItem,
    mime_type: &str,
) -> Result<web_sys::Blob, JsValue> {
    let blob = wasm_bindgen_futures::JsFuture::from(item.get_type(mime_type)).await?;
    Ok(blob.unchecked_into())
}

async fn read_clipboard_item_text(
    item: &web_sys::ClipboardItem,
    mime_type: &str,
) -> Result<String, JsValue> {
    let blob = read_clipboard_item_blob(item, mime_type).await?;
    let text = wasm_bindgen_futures::JsFuture::from(blob.text()).await?;
    text.as_string()
        .ok_or_else(|| JsValue::from_str("blob text is not a string"))
}

async fn read_clipboard_item_bytes(
    item: &web_sys::ClipboardItem,
    mime_type: &str,
) -> Result<Vec<u8>, JsValue> {
    let blob = read_clipboard_item_blob(item, mime_type).await?;
    read_blob_bytes(&blob).await
}

pub(crate) async fn read_blob_bytes(blob: &web_sys::Blob) -> Result<Vec<u8>, JsValue> {
    let buffer = wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

fn js_error_name(error: &JsValue) -> Option<String> {
    js_sys::Reflect::get(error, &"name".into())
        .ok()
        .and_then(|name| name.as_string())
}

pub(crate) fn js_error_message(error: &JsValue) -> String {
    js_sys::Reflect::get(error, &"message".into())
        .ok()
        .and_then(|message| message.as_string())
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "unknown browser error".to_string())
}

struct SelectedBrowserToken(String);
impl Drop for SelectedBrowserToken {
    fn drop(&mut self) {
        release_browser_directory_file(&self.0);
    }
}

fn selected_browser_file(path: Vec<String>, token: String) -> SelectedDirectoryFile {
    // Either consumer must keep the browser File alive after the other
    // capability is dropped. The last owner releases an unused token.
    let token = Arc::new(SelectedBrowserToken(token));
    let reader_token = token.clone();
    SelectedDirectoryFile::new(path, move || Box::pin(async move { Ok(selected_browser_reader(reader_token)) }))
        .with_browser_file(move || take_browser_directory_file(&token.0))
}

fn selected_browser_reader(token: Arc<SelectedBrowserToken>) -> gpui::SelectedDirectoryReader {
    let stream = futures::stream::try_unfold(
        (token, 0_u64),
        |(token, offset)| async move {
            let bytes = wasm_bindgen_futures::JsFuture::from(read_browser_directory_file(
                &token.0,
                offset as f64,
                256 * 1024,
            ))
            .await
            .map_err(|error| std::io::Error::other(js_error_message(&error)))?;
            let bytes = js_sys::Uint8Array::new(&bytes).to_vec();
            if bytes.is_empty() {
                return Ok(None);
            }
            let next = offset + bytes.len() as u64;
            Ok(Some((bytes, (token, next))))
        },
    );
    let reader: gpui::SelectedDirectoryReader =
        Box::pin(futures::TryStreamExt::into_async_read(stream));
    reader
}

fn selected_files_from_js(value: JsValue) -> Result<Option<Vec<SelectedFile>>> {
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    if !js_sys::Array::is_array(&value) {
        return Err(anyhow::anyhow!(
            "browser file picker returned an invalid result"
        ));
    }
    let mut selected = Vec::new();
    for entry in js_sys::Array::from(&value) {
        let name = js_sys::Reflect::get(&entry, &JsValue::from_str("name"))
            .map_err(|error| anyhow::anyhow!(js_error_message(&error)))?
            .as_string()
            .ok_or_else(|| anyhow::anyhow!("selected browser file has no name"))?;
        let token = js_sys::Reflect::get(&entry, &JsValue::from_str("token"))
            .map_err(|error| anyhow::anyhow!(js_error_message(&error)))?
            .as_string()
            .ok_or_else(|| anyhow::anyhow!("selected browser file has no transfer token"))?;
        selected.push(SelectedFile {
            name,
            source: selected_browser_file(Vec::new(), token),
        });
    }
    Ok(Some(selected))
}

fn selected_directory_from_js(value: JsValue) -> Result<Option<SelectedDirectory>> {
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    let name = js_sys::Reflect::get(&value, &JsValue::from_str("name"))
        .map_err(|error| anyhow::anyhow!(js_error_message(&error)))?
        .as_string()
        .ok_or_else(|| anyhow::anyhow!("selected browser directory has no name"))?;
    let entries = js_sys::Reflect::get(&value, &JsValue::from_str("entries"))
        .map_err(|error| anyhow::anyhow!(js_error_message(&error)))?;
    if !js_sys::Array::is_array(&entries) {
        return Err(anyhow::anyhow!(
            "browser directory picker returned invalid entries"
        ));
    }

    let mut directories = std::collections::BTreeSet::new();
    let mut files = Vec::new();
    for entry in js_sys::Array::from(&entries) {
        let path = js_sys::Reflect::get(&entry, &JsValue::from_str("path"))
            .map_err(|error| anyhow::anyhow!(js_error_message(&error)))?
            .as_string()
            .ok_or_else(|| anyhow::anyhow!("selected browser file has no relative path"))?;
        let mut components = path
            .split('/')
            .filter(|component| !component.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if components.first().is_none_or(|root| root != &name) {
            return Err(anyhow::anyhow!(
                "selected browser file is outside the selected directory"
            ));
        }
        components.remove(0);
        if components.is_empty()
            || components
                .iter()
                .any(|component| component == "." || component == "..")
        {
            return Err(anyhow::anyhow!(
                "selected browser file has an invalid relative path"
            ));
        }
        for end in 1..components.len() {
            directories.insert(components[..end].to_vec());
        }

        let token = js_sys::Reflect::get(&entry, &JsValue::from_str("token"))
            .map_err(|error| anyhow::anyhow!(js_error_message(&error)))?;
        if token.is_null() || token.is_undefined() {
            continue;
        }
        let token = token
            .as_string()
            .ok_or_else(|| anyhow::anyhow!("selected browser file has no transfer token"))?;
        let size = js_sys::Reflect::get(&entry, &JsValue::from_str("size")).ok().and_then(|value| value.as_f64())
            .filter(|size| size.is_finite() && *size >= 0.0 && size.fract() == 0.0 && *size <= 9_007_199_254_740_991.0).map(|size| size as u64);
        files.push(selected_browser_file(components, token).with_size_bytes(size));
    }

    Ok(Some(SelectedDirectory {
        local_path: None,
        name,
        directories: directories.into_iter().collect(),
        files,
    }))
}

fn log_clipboard_entry_error(mime_type: &str, error: &JsValue) {
    log::warn!(
        "failed to read clipboard entry with type {mime_type}: {}",
        js_error_message(error)
    );
}

fn cursor_restore_listeners(
    browser_window: &web_sys::Window,
    cursor_visible: Rc<Cell<bool>>,
    last_cursor_css: Rc<Cell<&'static str>>,
) -> Vec<EventListenerHandle> {
    let mut handles = Vec::new();
    let Some(document) = browser_window.document() else {
        return handles;
    };

    let mut add_listener = |target: &web_sys::EventTarget, event_name: &'static str| {
        let browser_window = browser_window.clone();
        let cursor_visible = cursor_visible.clone();
        let last_cursor_css = last_cursor_css.clone();
        handles.push(EventListenerHandle::add(
            target,
            event_name,
            move |_event: JsValue| {
                if !cursor_visible.replace(true) {
                    set_body_cursor(&browser_window, last_cursor_css.get());
                }
            },
        ));
    };

    let document_target: &web_sys::EventTarget = document.as_ref();
    let window_target: &web_sys::EventTarget = browser_window.as_ref();

    add_listener(document_target, "mousemove");
    add_listener(document_target, "mouseenter");
    add_listener(window_target, "blur");
    add_listener(document_target, "visibilitychange");

    handles
}

fn show_graphics_unavailable_message(browser_window: &web_sys::Window, error: &anyhow::Error) {
    let Some(document) = browser_window.document() else {
        return;
    };
    let Some(body) = document.body() else {
        return;
    };
    let Ok(message) = document.create_element("p") else {
        return;
    };
    message.set_text_content(Some(&format!(
        "Failed to initialize browser graphics: {error}"
    )));
    body.append_child(&message).ok();
}

fn set_body_cursor(browser_window: &web_sys::Window, css_cursor: &str) {
    if let Some(document) = browser_window.document()
        && let Some(body) = document.body()
        && let Err(error) = body.style().set_property("cursor", css_cursor)
    {
        log::warn!("Failed to set cursor style: {error:?}");
    }
}
