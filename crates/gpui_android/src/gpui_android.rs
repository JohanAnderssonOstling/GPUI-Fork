#![cfg(any(target_os = "android", test))]

//! GPUI backend for Android, driven by `android-activity`'s `GameActivity`
//! glue. The OS owns the activity lifecycle; `AndroidPlatform::run` blocks in
//! the `android_main` thread pumping `AndroidApp::poll_events`. Rendering and
//! text are provided by `gpui_wgpu` (Vulkan/GL + cosmic-text).

#[cfg(target_os = "android")]
mod dispatcher;
mod display;
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod events;
mod frame;
#[cfg(target_os = "android")]
mod keyboard;
mod lifecycle;
#[cfg(target_os = "android")]
mod platform;
#[cfg(target_os = "android")]
mod window;

#[cfg(target_os = "android")]
pub use android_activity::AndroidApp;
#[cfg(target_os = "android")]
pub use wgpu;
#[cfg(target_os = "android")]
pub use platform::{
    AndroidPlatform, AndroidPlatformOptions, complete_directory_prompt, complete_file_prompt, init, reader_chrome_revealed,
    request_all_files_access, selected_directory_file, selected_directory_local_root,
    selected_directory_path, selected_directory_root, selected_file_descriptor,
    volume_button_pressed, window_insets_changed,
};

#[cfg(target_os = "android")]
pub fn init_logging() {
    // Keep the Android backend verbose without enabling noisy dependency logs.
    let filter = android_logger::FilterBuilder::new()
        .parse("info,gpui_android=debug")
        .build();
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_filter(filter)
            .with_tag("gpui"),
    );
}
