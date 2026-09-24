use std::time::Duration;

use gpui::{
    AppContext, Application, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window,
    WindowBounds, WindowOptions, div, point, px, rgb, size,
};
use gpui_kobo::{KoboPlatform, KoboPlatformOptions, PageButtonBehavior};

struct ButtonDemo;

impl Render for ButtonDemo {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .w_full()
            .h_full()
            .bg(rgb(0xffffff))
            .font_family("Lilex")
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(160.0))
                    .h(px(56.0))
                    .bg(rgb(0x202020))
                    .text_color(rgb(0xffffff))
                    .child("Test button"),
            )
    }
}

#[test]
fn production_platform_renders_one_button() {
    let logical_size = size(px(300.0), px(160.0));
    let platform = KoboPlatform::new(KoboPlatformOptions {
        logical_size,
        scale_factor: 2.0,
        display: false,
        interactive: false,
        timeout: Duration::from_secs(1),
        page_buttons: PageButtonBehavior::Scroll,
        ..Default::default()
    })
    .expect("production Kobo platform should initialize");

    Application::new_inaccessible(platform.clone()).run(move |cx| {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.0), px(0.0)),
                    size: logical_size,
                })),
                focus: true,
                show: true,
                ..Default::default()
            },
            |_window, cx| cx.new(|_| ButtonDemo),
        )
        .expect("production Kobo test window should open");
    });

    assert!(
        platform.take_error().is_none(),
        "Kobo platform reported an error"
    );
    let image = platform
        .last_frame()
        .expect("production Kobo platform should render a framebuffer");
    assert_eq!(platform.render_count(), 1);
    assert_eq!(image.dimensions(), (600, 320));

    let dark_pixels = image.pixels().filter(|pixel| pixel.0[0] < 64).count();
    assert!(dark_pixels > 10_000, "button background was not rendered");

    let light_button_pixels = image
        .enumerate_pixels()
        .filter(|(x, y, pixel)| {
            (140..460).contains(x) && (104..216).contains(y) && pixel.0[0] > 224
        })
        .count();
    assert!(
        light_button_pixels > 20,
        "button label glyphs were not rendered"
    );
}
