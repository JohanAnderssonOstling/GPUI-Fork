#![cfg(target_os = "android")]

//! GPUI backend for Android, driven by `android-activity`'s `GameActivity`
//! glue. The OS owns the activity lifecycle; `AndroidPlatform::run` blocks in
//! the `android_main` thread pumping `AndroidApp::poll_events`. Rendering and
//! text are provided by `gpui_wgpu` (Vulkan/GL + cosmic-text).

mod dispatcher;
mod display;
mod events;
mod keyboard;
mod platform;
mod window;

pub use android_activity::AndroidApp;
pub use platform::{
    AndroidPlatform, complete_directory_prompt, complete_file_prompt, init,
    selected_directory_file, selected_directory_path, selected_directory_root, selected_directory_local_root, request_all_files_access,
    reader_chrome_revealed, selected_file_descriptor, volume_button_pressed, window_insets_changed,
};

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
