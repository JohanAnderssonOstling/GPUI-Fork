use crate::dispatcher::AndroidDispatcher;
use crate::display::AndroidDisplay;
use crate::events::{self, TouchGesture};
use crate::keyboard::AndroidKeyboardLayout;
use crate::window::{
    AndroidPhysicalEdges, AndroidPhysicalInsets, AndroidWindow, AndroidWindowInner,
};
use android_activity::input::KeyCharacterMap;
use android_activity::{AndroidApp, MainEvent, PollEvent};
use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use gpui::{
    Action, AnyWindowHandle, AppLifecyclePhase, BackgroundExecutor, ClipboardItem, CursorStyle,
    DummyKeyboardMapper, FileDialogFilter, ForegroundExecutor, Keymap, Menu, MenuItem,
    PathPromptOptions, Platform, PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper,
    PlatformTextSystem, PlatformWindow, PriorityQueueReceiver, RunnableVariant, SelectedDirectory,
    SelectedDirectoryFile, SelectedDirectoryReader, SelectedFile, Task, ThermalState,
    WindowAppearance, WindowParams,
};
use gpui_wgpu::GpuContext;
use jni::objects::{JObject, JString};
use jni::refs::Global;
use jni::strings::JNIString;
use jni::{JValue, JavaVM, jni_sig, jni_str};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::FromRawFd as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

static ANDROID_APP: OnceLock<Mutex<AndroidApp>> = OnceLock::new();
static NEXT_PATH_PROMPT_ID: AtomicU64 = AtomicU64::new(1);
static PENDING_FILE_PROMPTS: OnceLock<Mutex<HashMap<u64, PendingFilePrompt>>> = OnceLock::new();
static PENDING_DIRECTORY_PROMPTS: OnceLock<Mutex<HashMap<u64, PendingDirectoryPrompt>>> =
    OnceLock::new();
static PENDING_VOLUME_BUTTONS: OnceLock<Mutex<VecDeque<bool>>> = OnceLock::new();
static PENDING_READER_CHROME_REVEALS: OnceLock<Mutex<u32>> = OnceLock::new();
static PENDING_WINDOW_INSETS: OnceLock<Mutex<Option<AndroidPhysicalInsets>>> = OnceLock::new();

struct PendingSelectedFile {
    name: String,
    file: File,
}

struct PendingFilePrompt {
    sender: oneshot::Sender<Result<Option<Vec<SelectedFile>>>>,
    files: Vec<PendingSelectedFile>,
}

struct PendingDirectoryFile {
    size_bytes: Option<u64>,
    path: Vec<String>,
    uri: String,
}

struct PendingDirectoryPrompt {
    sender: oneshot::Sender<Result<Option<Vec<SelectedDirectory>>>>,
    name: Option<String>,
    local_path: Option<PathBuf>,
    directories: Vec<Vec<String>>,
    files: Vec<PendingDirectoryFile>,
}

fn pending_file_prompts() -> &'static Mutex<HashMap<u64, PendingFilePrompt>> {
    PENDING_FILE_PROMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pending_directory_prompts() -> &'static Mutex<HashMap<u64, PendingDirectoryPrompt>> {
    PENDING_DIRECTORY_PROMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pending_volume_buttons() -> &'static Mutex<VecDeque<bool>> {
    PENDING_VOLUME_BUTTONS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn pending_reader_chrome_reveals() -> &'static Mutex<u32> {
    PENDING_READER_CHROME_REVEALS.get_or_init(|| Mutex::new(0))
}


fn pending_window_insets() -> &'static Mutex<Option<AndroidPhysicalInsets>> {
    PENDING_WINDOW_INSETS.get_or_init(|| Mutex::new(None))
}

/// Adds one provider-owned file descriptor to an Android file prompt. The
/// caller transfers ownership of `file` to GPUI.
pub fn selected_file_descriptor(request_id: u64, name: String, file: File) {
    let mut prompts = pending_file_prompts().lock().unwrap();
    if let Some(prompt) = prompts.get_mut(&request_id) {
        prompt.files.push(PendingSelectedFile { name, file });
    } else {
        log::warn!("received a file descriptor for unknown Android file prompt {request_id}");
    }
}

/// Completes an Android file prompt after all selected descriptors have been
/// transferred. The consumer owns the descriptors and decides when to read.
pub fn complete_file_prompt(request_id: u64, error: Option<anyhow::Error>, cancelled: bool) {
    let prompt = pending_file_prompts().lock().unwrap().remove(&request_id);
    let Some(prompt) = prompt else {
        log::warn!("received completion for unknown Android file prompt {request_id}");
        return;
    };

    if cancelled {
        prompt.sender.send(Ok(None)).ok();
        return;
    }
    if let Some(error) = error {
        prompt.sender.send(Err(error)).ok();
        return;
    }

    let files = prompt.files.into_iter().map(|selected| SelectedFile {
        name: selected.name,
        source: SelectedDirectoryFile::new(Vec::new(), move || Box::pin(async move {
            Ok(Box::new(selected.file) as SelectedDirectoryReader)
        })),
    }).collect();
    prompt.sender.send(Ok(Some(files))).ok();
}

pub fn selected_directory_root(request_id: u64, name: String) {
    let mut prompts = pending_directory_prompts().lock().unwrap();
    if let Some(prompt) = prompts.get_mut(&request_id) {
        prompt.name = Some(name);
    } else {
        log::warn!("received a root for unknown Android directory prompt {request_id}");
    }
}

pub fn selected_directory_local_root(request_id: u64, path: PathBuf) {
    if let Some(prompt) = pending_directory_prompts().lock().unwrap().get_mut(&request_id) {
        prompt.local_path = Some(path);
    }
}

pub fn selected_directory_path(request_id: u64, path: Vec<String>) {
    let mut prompts = pending_directory_prompts().lock().unwrap();
    if let Some(prompt) = prompts.get_mut(&request_id) {
        prompt.directories.push(path);
    } else {
        log::warn!("received a path for unknown Android directory prompt {request_id}");
    }
}

pub fn selected_directory_file(request_id: u64, path: Vec<String>, uri: String, size_bytes: Option<u64>) {
    let mut prompts = pending_directory_prompts().lock().unwrap();
    if let Some(prompt) = prompts.get_mut(&request_id) {
        prompt.files.push(PendingDirectoryFile { path, uri, size_bytes });
    } else {
        log::warn!("received a file for unknown Android directory prompt {request_id}");
    }
}

pub fn complete_directory_prompt(request_id: u64, error: Option<anyhow::Error>, cancelled: bool) {
    let prompt = pending_directory_prompts()
        .lock()
        .unwrap()
        .remove(&request_id);
    let Some(prompt) = prompt else {
        log::warn!("received completion for unknown Android directory prompt {request_id}");
        return;
    };
    if cancelled {
        prompt.sender.send(Ok(None)).ok();
        return;
    }
    if let Some(error) = error {
        prompt.sender.send(Err(error)).ok();
        return;
    }
    let Some(name) = prompt.name else {
        prompt
            .sender
            .send(Err(anyhow::anyhow!(
                "Android directory picker returned no root"
            )))
            .ok();
        return;
    };
    let files = prompt
        .files
        .into_iter()
        .map(|file| {
            let uri = file.uri;
            SelectedDirectoryFile::new(file.path, move || {
                Box::pin(async move { open_android_document(&uri) })
            }).with_size_bytes(file.size_bytes)
        })
        .collect();
    prompt
        .sender
        .send(Ok(Some(vec![SelectedDirectory {
            local_path: prompt.local_path,
            name,
            directories: prompt.directories,
            files,
        }])))
        .ok();
}

fn open_android_document(uri: &str) -> Result<SelectedDirectoryReader> {
    let app = ANDROID_APP
        .get()
        .ok_or_else(|| anyhow::anyhow!("Android application is not initialized"))?
        .lock()
        .unwrap()
        .clone();
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let descriptor = vm.attach_current_thread(|env| {
        let raw_activity = app.activity_as_ptr() as jni::sys::jobject;
        let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw_activity)? };
        let uri = JString::from_str(env, uri)?;
        env.call_method(
            activity.as_ref(),
            jni_str!("openDocumentDescriptor"),
            jni_sig!("(Ljava/lang/String;)I"),
            &[JValue::Object(uri.as_ref())],
        )?
        .i()
    })?;
    if descriptor < 0 {
        anyhow::bail!("Android document provider could not open {uri}");
    }
    // SAFETY: MainActivity detaches this descriptor from its
    // ParcelFileDescriptor and transfers ownership to the native caller.
    let file = unsafe { File::from_raw_fd(descriptor) };
    Ok(Box::new(file))
}

pub fn request_all_files_access() {
    let Some(app) = ANDROID_APP.get() else { return };
    let app = app.lock().unwrap().clone();
    let activity_app = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        let vm = unsafe { JavaVM::from_raw(activity_app.vm_as_ptr().cast()) };
        let result = vm.attach_current_thread(|env| {
            let raw = activity_app.activity_as_ptr() as jni::sys::jobject;
            let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw)? };
            env.call_method(activity.as_ref(), jni_str!("requestAllFilesAccess"), jni_sig!("()V"), &[])?;
            Ok::<_, jni::errors::Error>(())
        });
        if let Err(error) = result {
            log::error!("Could not open Android file access settings: {error}");
        }
    }));
}

/// Queues a captured Android volume button as reader navigation.
/// `next` maps to Page Down; `false` maps to Page Up.
pub fn volume_button_pressed(next: bool) {
    pending_volume_buttons().lock().unwrap().push_back(next);
    if let Some(app) = ANDROID_APP.get() {
        app.lock().unwrap().create_waker().wake();
    }
}

pub fn reader_chrome_revealed() {
    *pending_reader_chrome_reveals().lock().unwrap() += 1;
    if let Some(app) = ANDROID_APP.get() {
        app.lock().unwrap().create_waker().wake();
    }
}


/// Publishes physical Android system/cutout and IME insets. The latest value
/// is consumed on GPUI's Android event-loop thread.
pub fn window_insets_changed(
    safe_left: i32,
    safe_top: i32,
    safe_right: i32,
    safe_bottom: i32,
    ime_left: i32,
    ime_top: i32,
    ime_right: i32,
    ime_bottom: i32,
) {
    *pending_window_insets().lock().unwrap() = Some(AndroidPhysicalInsets {
        safe_area: AndroidPhysicalEdges::from_android(safe_left, safe_top, safe_right, safe_bottom),
        ime: AndroidPhysicalEdges::from_android(ime_left, ime_top, ime_right, ime_bottom),
    });
    if let Some(app) = ANDROID_APP.get() {
        app.lock().unwrap().create_waker().wake();
    }
}

/// Stores the `AndroidApp` handed to `android_main` so that
/// `gpui_platform::current_platform` (which takes no arguments) can reach it.
/// Must be called before constructing the platform.
pub fn init(app: AndroidApp) {
    match ANDROID_APP.get() {
        Some(current) => *current.lock().unwrap() = app,
        None => {
            let _ = ANDROID_APP.set(Mutex::new(app));
        }
    }
}

fn call_activity_boolean_method(app: AndroidApp, method: &'static str, value: bool) {
    let app_for_call = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        let result = (|| -> jni::errors::Result<()> {
            let vm = unsafe { JavaVM::from_raw(app_for_call.vm_as_ptr().cast()) };
            vm.attach_current_thread(|env| {
                let raw_activity = app_for_call.activity_as_ptr() as jni::sys::jobject;
                let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw_activity)? };
                env.call_method(
                    activity.as_ref(),
                    JNIString::new(method),
                    jni_sig!("(Z)V"),
                    &[JValue::Bool(value as jni::sys::jboolean)],
                )?;
                Ok(())
            })
        })();
        if let Err(error) = result {
            log::error!("failed to call Android activity method {method}: {error}");
        }
    }));
}

const POLL_TIMEOUT: Duration = Duration::from_millis(8);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

pub struct AndroidPlatform {
    app: AndroidApp,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    main_receiver: PriorityQueueReceiver<RunnableVariant>,
    text_system: Arc<dyn PlatformTextSystem>,
    gpu_context: GpuContext,
    display: Rc<AndroidDisplay>,
    active_window: RefCell<Option<(AnyWindowHandle, Rc<AndroidWindowInner>)>>,
    callbacks: RefCell<AndroidPlatformCallbacks>,
    pending_launch: RefCell<Option<Box<dyn 'static + FnOnce()>>>,
    quit_requested: Cell<bool>,
    key_maps: RefCell<HashMap<i32, KeyCharacterMap>>,
    touch_gesture: RefCell<TouchGesture>,
    last_frame: Cell<Instant>,
    backgrounded: Cell<bool>,
}

#[derive(Default)]
struct AndroidPlatformCallbacks {
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    quit: Option<Box<dyn FnMut()>>,
    reopen: Option<Box<dyn FnMut()>>,
    app_menu_action: Option<Box<dyn FnMut(&dyn Action)>>,
    will_open_app_menu: Option<Box<dyn FnMut()>>,
    validate_app_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
    keyboard_layout_change: Option<Box<dyn FnMut()>>,
    thermal_state_change: Option<Box<dyn FnMut()>>,
    app_lifecycle: Option<Box<dyn FnMut(AppLifecyclePhase)>>,
    memory_warning: Option<Box<dyn FnMut()>>,
}

impl AndroidPlatform {
    pub fn new(_headless: bool) -> Self {
        let app = ANDROID_APP
            .get()
            .expect("gpui_android::init(app) must be called from android_main before building the platform")
            .lock()
            .unwrap()
            .clone();

        let (main_sender, main_receiver) = PriorityQueueReceiver::new();
        let dispatcher = Arc::new(AndroidDispatcher::new(main_sender, app.create_waker()));
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher);

        let text_system = Arc::new(gpui_wgpu::CosmicTextSystem::new_without_system_fonts(
            "Roboto",
        ));
        if let Err(error) = text_system.add_fonts(system_fonts()) {
            log::error!("failed to load Android system fonts: {error:#}");
        }

        Self {
            app,
            background_executor,
            foreground_executor,
            main_receiver,
            text_system,
            gpu_context: GpuContext::default(),
            display: Rc::new(AndroidDisplay::new()),
            active_window: RefCell::new(None),
            callbacks: RefCell::new(AndroidPlatformCallbacks::default()),
            pending_launch: RefCell::new(None),
            quit_requested: Cell::new(false),
            key_maps: RefCell::new(HashMap::new()),
            touch_gesture: RefCell::new(TouchGesture::default()),
            last_frame: Cell::new(Instant::now()),
            backgrounded: Cell::new(false),
        }
    }

    fn window(&self) -> Option<Rc<AndroidWindowInner>> {
        self.active_window
            .borrow()
            .as_ref()
            .map(|(_, inner)| Rc::clone(inner))
    }

    fn handle_main_event(&self, event: MainEvent<'_>) {
        match event {
            MainEvent::InitWindow { .. } => {
                if let Some(window) = self.window() {
                    window.handle_surface_created();
                }
                let launch = self.pending_launch.borrow_mut().take();
                if let Some(launch) = launch {
                    launch();
                }
            }
            MainEvent::TerminateWindow { .. } => {
                if let Some(window) = self.window() {
                    window.handle_surface_destroyed();
                }
            }
            MainEvent::WindowResized { .. } | MainEvent::ContentRectChanged { .. } => {
                if let Some(window) = self.window() {
                    window.update_size();
                }
            }
            MainEvent::InsetsChanged { .. } => {}
            MainEvent::ConfigChanged { .. } => {
                if let Some(window) = self.window() {
                    window.update_size();
                    window.set_appearance(self.window_appearance());
                }
            }
            MainEvent::RedrawNeeded { .. } => {
                if let Some(window) = self.window() {
                    window.request_frame(true);
                    self.last_frame.set(Instant::now());
                }
            }
            MainEvent::InputAvailable => self.process_input(),
            MainEvent::GainedFocus => {
                if let Some(window) = self.window() {
                    window.set_active(true);
                }
            }
            MainEvent::LostFocus => {
                if let Some(window) = self.window() {
                    window.set_active(false);
                }
            }
            MainEvent::Start => {
                self.backgrounded.set(false);
                if let Some(callback) = self.callbacks.borrow_mut().app_lifecycle.as_mut() {
                    callback(AppLifecyclePhase::Foreground);
                }
            }
            MainEvent::Resume { .. } => {
                self.backgrounded.set(false);
                if let Some(callback) = self.callbacks.borrow_mut().app_lifecycle.as_mut() {
                    callback(AppLifecyclePhase::Active);
                }
            }
            MainEvent::Pause => {
                if let Some(callback) = self.callbacks.borrow_mut().app_lifecycle.as_mut() {
                    callback(AppLifecyclePhase::Inactive);
                }
            }
            MainEvent::Stop => {
                self.backgrounded.set(true);
                if let Some(callback) = self.callbacks.borrow_mut().app_lifecycle.as_mut() {
                    callback(AppLifecyclePhase::Background);
                }
            }
            MainEvent::LowMemory => {
                if let Some(callback) = self.callbacks.borrow_mut().memory_warning.as_mut() {
                    callback();
                }
            }
            MainEvent::Destroy => self.quit_requested.set(true),
            _ => {}
        }
    }

    fn process_input(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let mut iter = match self.app.input_events_iter() {
            Ok(iter) => iter,
            Err(error) => {
                log::error!("failed to get input events iterator: {error:?}");
                return;
            }
        };
        let mut key_maps = self.key_maps.borrow_mut();
        let mut gesture = self.touch_gesture.borrow_mut();
        loop {
            let mut finish_activity = false;
            let more = iter.next(|event| {
                events::handle_input_event(
                    event,
                    &window,
                    &mut gesture,
                    &mut key_maps,
                    &self.app,
                    &mut finish_activity,
                )
            });
            if finish_activity {
                self.quit_requested.set(true);
                return;
            }
            if !more {
                break;
            }
        }
    }

    fn drain_main_runnables(&self) {
        let receiver = self.main_receiver.clone();
        for runnable in receiver.try_iter() {
            match runnable {
                Ok(runnable) => {
                    runnable.run();
                }
                Err(_) => break,
            }
        }
    }

    fn drain_volume_buttons(&self) {
        let Some(window) = self.window() else {
            pending_volume_buttons().lock().unwrap().clear();
            return;
        };
        let buttons = pending_volume_buttons()
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        for next in buttons {
            events::dispatch_reader_page_key(&window, next);
        }
    }

    fn drain_reader_chrome_reveals(&self) {
        let Some(window) = self.window() else {
            *pending_reader_chrome_reveals().lock().unwrap() = 0;
            return;
        };
        let count = std::mem::take(&mut *pending_reader_chrome_reveals().lock().unwrap());
        for _ in 0..count {
            events::dispatch_reader_chrome_reveal(&window);
        }
    }

    fn drain_window_insets(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let insets = pending_window_insets().lock().unwrap().take();
        if let Some(insets) = insets {
            window.update_physical_insets(insets);
        }
    }

    fn maybe_request_frame(&self) {
        if self.backgrounded.get() || self.last_frame.get().elapsed() < FRAME_INTERVAL {
            return;
        }
        if let Some(window) = self.window() {
            self.last_frame.set(Instant::now());
            window.request_frame(false);
        }
    }
}

fn system_fonts() -> Vec<Cow<'static, [u8]>> {
    let mut fonts = Vec::new();
    let Ok(entries) = std::fs::read_dir("/system/fonts") else {
        log::warn!("/system/fonts is not readable");
        return fonts;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let wanted = name.starts_with("Roboto-")
            || name.starts_with("RobotoStatic-")
            || name == "NotoColorEmoji.ttf"
            || name.starts_with("NotoSansSymbols-");
        if !wanted {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => fonts.push(Cow::Owned(bytes)),
            Err(error) => log::warn!("failed to read font {path:?}: {error}"),
        }
    }
    if fonts.is_empty() {
        log::warn!(
            "no Roboto fonts found in /system/fonts; text rendering will fail unless the app bundles fonts"
        );
    }
    fonts
}

impl Platform for AndroidPlatform {
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
        *self.pending_launch.borrow_mut() = Some(on_finish_launching);
        let app = self.app.clone();
        while !self.quit_requested.get() {
            let timeout = if self.backgrounded.get() {
                None
            } else {
                Some(POLL_TIMEOUT)
            };
            app.poll_events(timeout, |event| {
                match event {
                    PollEvent::Wake | PollEvent::Timeout => {}
                    PollEvent::Main(main_event) => self.handle_main_event(main_event),
                    _ => {}
                }
                self.drain_main_runnables();
                self.drain_volume_buttons();
                self.drain_reader_chrome_reveals();
                self.drain_window_insets();
                self.maybe_request_frame();
            });
        }
        let mut callbacks = self.callbacks.borrow_mut();
        if let Some(ref mut quit) = callbacks.quit {
            quit();
        }
    }

    fn quit(&self) {
        self.quit_requested.set(true);
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<OsString>) {}

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
        self.active_window
            .borrow()
            .as_ref()
            .map(|(handle, _)| *handle)
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        params: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        let window = AndroidWindow::new(
            handle,
            params,
            self.app.clone(),
            self.gpu_context.clone(),
            self.display.clone(),
            self.window_appearance(),
        )?;
        self.display
            .set_size(window.inner.state.borrow().bounds.size);
        *self.active_window.borrow_mut() = Some((handle, Rc::clone(&window.inner)));
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        match self.app.config().ui_mode_night() {
            android_activity::ndk::configuration::UiModeNight::Yes => WindowAppearance::Dark,
            _ => WindowAppearance::Light,
        }
    }

    fn open_url(&self, url: &str) {
        log::warn!("AndroidPlatform::open_url is not implemented (url: {url})");
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
        _filters: Vec<FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow::anyhow!("Android selections do not expose filesystem paths; use prompt_for_files or prompt_for_directories"))).ok();
        rx
    }

    fn prompt_for_files(
        &self,
        options: PathPromptOptions,
        filters: Vec<FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<Vec<SelectedFile>>>> {
        let (tx, rx) = oneshot::channel();
        if !options.files || options.directories {
            tx.send(Err(anyhow::anyhow!(
                "Android file prompts select files only"
            )))
            .ok();
            return rx;
        }

        let request_id = NEXT_PATH_PROMPT_ID.fetch_add(1, Ordering::Relaxed);
        let extensions = filters
            .into_iter()
            .flat_map(|filter| filter.extensions)
            .map(|extension| extension.trim_start_matches('.').to_ascii_lowercase())
            .filter(|extension| !extension.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join("\n");
        pending_file_prompts().lock().unwrap().insert(
            request_id,
            PendingFilePrompt {
                sender: tx,
                files: Vec::new(),
            },
        );

        let app = self.app.clone();
        let app_for_call = app.clone();
        app.run_on_java_main_thread(Box::new(move || {
            let result = (|| -> jni::errors::Result<()> {
                let vm = unsafe { JavaVM::from_raw(app_for_call.vm_as_ptr().cast()) };
                vm.attach_current_thread(|env| {
                    let raw_activity = app_for_call.activity_as_ptr() as jni::sys::jobject;
                    let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw_activity)? };
                    let extensions = JString::from_str(env, &extensions)?;
                    env.call_method(
                        activity.as_ref(),
                        jni_str!("openFilePrompt"),
                        jni_sig!("(JZLjava/lang/String;)V"),
                        &[
                            JValue::Long(request_id as i64),
                            JValue::Bool(options.multiple as jni::sys::jboolean),
                            JValue::Object(extensions.as_ref()),
                        ],
                    )?;
                    Ok(())
                })
            })();
            if let Err(error) = result {
                complete_file_prompt(
                    request_id,
                    Some(anyhow::anyhow!(
                        "failed to open Android file prompt: {error}"
                    )),
                    false,
                );
            }
        }));
        rx
    }

    fn prompt_for_directories(
        &self,
        options: PathPromptOptions,
        filters: Vec<FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<Vec<SelectedDirectory>>>> {
        let (tx, rx) = oneshot::channel();
        if options.files || !options.directories {
            tx.send(Err(anyhow::anyhow!(
                "Android directory prompts select directories only"
            )))
            .ok();
            return rx;
        }
        let request_id = NEXT_PATH_PROMPT_ID.fetch_add(1, Ordering::Relaxed);
        let extensions = filters
            .into_iter()
            .flat_map(|filter| filter.extensions)
            .map(|extension| extension.trim_start_matches('.').to_ascii_lowercase())
            .filter(|extension| !extension.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join("\n");
        pending_directory_prompts().lock().unwrap().insert(
            request_id,
            PendingDirectoryPrompt {
                sender: tx,
                name: None,
                local_path: None,
                directories: Vec::new(),
                files: Vec::new(),
            },
        );

        let app = self.app.clone();
        let app_for_call = app.clone();
        app.run_on_java_main_thread(Box::new(move || {
            let result = (|| -> jni::errors::Result<()> {
                let vm = unsafe { JavaVM::from_raw(app_for_call.vm_as_ptr().cast()) };
                vm.attach_current_thread(|env| {
                    let raw_activity = app_for_call.activity_as_ptr() as jni::sys::jobject;
                    let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw_activity)? };
                    let extensions = JString::from_str(env, &extensions)?;
                    env.call_method(
                        activity.as_ref(),
                        jni_str!("openDirectoryPrompt"),
                        jni_sig!("(JLjava/lang/String;)V"),
                        &[
                            JValue::Long(request_id as i64),
                            JValue::Object(extensions.as_ref()),
                        ],
                    )?;
                    Ok(())
                })
            })();
            if let Err(error) = result {
                complete_directory_prompt(
                    request_id,
                    Some(anyhow::anyhow!(
                        "failed to open Android directory prompt: {error}"
                    )),
                    false,
                );
            }
        }));
        rx
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
        _filters: Vec<FileDialogFilter>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow::anyhow!(
            "prompt_for_new_path is not supported on Android"
        )))
        .ok();
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn set_volume_button_capture(&self, enabled: bool) {
        call_activity_boolean_method(self.app.clone(), "setVolumeButtonCapture", enabled);
    }

    fn supports_touch_input(&self) -> bool {
        true
    }

    fn set_system_bars_visible(&self, visible: bool) {
        call_activity_boolean_method(self.app.clone(), "setSystemBarsVisible", visible);
    }

    fn set_keep_screen_awake(&self, awake: bool) {
        call_activity_boolean_method(self.app.clone(), "setKeepScreenAwake", awake);
    }

    fn reveal_path(&self, _path: &Path) {}

    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().quit = Some(callback);
    }

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().reopen = Some(callback);
    }

    fn on_app_lifecycle(&self, callback: Box<dyn FnMut(AppLifecyclePhase)>) {
        self.callbacks.borrow_mut().app_lifecycle = Some(callback);
    }

    fn on_memory_warning(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().memory_warning = Some(callback);
    }

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
        "Android"
    }

    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow::anyhow!("app_path is not available on Android"))
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow::anyhow!(
            "path_for_auxiliary_executable is not available on Android"
        ))
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
        None
    }

    fn write_to_clipboard(&self, _item: ClipboardItem) {}

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "credential storage is not implemented on Android"
        )))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "credential storage is not implemented on Android"
        )))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(AndroidKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().keyboard_layout_change = Some(callback);
    }
}
