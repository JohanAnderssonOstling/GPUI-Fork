#![cfg(target_os = "android")]

//! Minimal GPUI app exercising the Android backend: rendering, tap (counter
//! button), scrolling (message list), and GameActivity packaging.
//!
//! Build and package (requires cargo-ndk, Android SDK/NDK, JDK 17 or 21,
//! and Gradle 8.13 or newer; run from the repo root):
//! ```sh
//! cd crates/gpui_android/examples/hello_android
//! cargo ndk -t arm64-v8a -o jniLibs build --release
//! gradle assembleDebug
//! adb install -r build/outputs/apk/debug/hello_android-debug.apk
//! ```

use android_activity::AndroidApp;
use gpui::{
    App, AppContext, Context, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowOptions, div, rgb,
};

struct HelloAndroid {
    taps: usize,
}

impl Render for HelloAndroid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(
                div()
                    .flex()
                    .justify_center()
                    .p_4()
                    .text_xl()
                    .child("GPUI on Android"),
            )
            .child(
                div()
                    .id("tap-counter")
                    .flex()
                    .justify_center()
                    .m_4()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(0x89b4fa))
                    .text_color(rgb(0x1e1e2e))
                    .child(format!("Taps: {} (tap me)", self.taps))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.taps += 1;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("scroll-area")
                    .flex_1()
                    .m_4()
                    .rounded_lg()
                    .bg(rgb(0x313244))
                    .overflow_y_scroll()
                    .child(div().flex().flex_col().children(
                        (1..=100).map(|i| div().p_3().child(format!("Scrollable row {i}"))),
                    )),
            )
    }
}

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    gpui_platform::android_init(app);
    gpui_platform::application().run(|cx: &mut App| {
        cx.open_window(WindowOptions::default(), |_, cx| {
            cx.new(|_| HelloAndroid { taps: 0 })
        })
        .unwrap();
        cx.activate(true);
    });
}
