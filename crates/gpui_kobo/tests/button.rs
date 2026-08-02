use std::sync::Arc;

use gpui::{
    App, Context, HeadlessAppContext, IntoElement, Render, Window, div, px, rgb, size,
};
use gpui_kobo::KoboRenderer;
use gpui_wgpu::CosmicTextSystem;

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
fn renders_one_button_with_the_cpu_kobo_renderer() {
    let text_system = Arc::new(CosmicTextSystem::new("DejaVu Sans"));
    let mut app = HeadlessAppContext::with_platform(text_system, Arc::new(()), || {
        Some(Box::new(KoboRenderer::new()))
    });
    let window = app
        .open_window(size(px(300.0), px(160.0)), |_window, cx: &mut App| {
            cx.new(|_| ButtonDemo)
        })
        .expect("headless Kobo test window should open");
    app.run_until_parked();

    let image = app
        .capture_screenshot(window.into())
        .expect("minimal GPUI button scene should render on the CPU");
    assert_eq!(image.dimensions(), (600, 320));

    let dark_pixels = image.pixels().filter(|pixel| pixel.0[0] < 64).count();
    assert!(dark_pixels > 10_000, "button background was not rendered");

    let light_button_pixels = image
        .enumerate_pixels()
        .filter(|(x, y, pixel)| {
            (140..460).contains(x) && (104..216).contains(y) && pixel.0[0] > 224
        })
        .count();
    assert!(light_button_pixels > 20, "button label glyphs were not rendered");
}
