#![cfg_attr(target_family = "wasm", no_main)]

use gpui::{
    App, ApplicationDrag, Bounds, Context, ExternalDragPayload, Half, Hsla, Pixels, Point, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use gpui_platform::application;

#[derive(Clone, Copy)]
struct DragInfo {
    ix: usize,
    color: Hsla,
    position: Point<Pixels>,
}

impl DragInfo {
    fn new(ix: usize, color: Hsla) -> Self {
        Self {
            ix,
            color,
            position: Point::default(),
        }
    }

    fn position(mut self, pos: Point<Pixels>) -> Self {
        self.position = pos;
        self
    }
}

impl Render for DragInfo {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let size = gpui::size(px(120.), px(50.));

        div()
            .pl(self.position.x - size.width.half())
            .pt(self.position.y - size.height.half())
            .child(
                div()
                    .flex()
                    .justify_center()
                    .items_center()
                    .w(size.width)
                    .h(size.height)
                    .bg(self.color.opacity(0.5))
                    .text_color(gpui::white())
                    .text_xs()
                    .shadow_md()
                    .child(format!("Item {}", self.ix)),
            )
    }
}

struct DragDrop {
    drop_on: Option<DragInfo>,
}

impl DragDrop {
    fn new() -> Self {
        Self { drop_on: None }
    }
}

impl Render for DragDrop {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items = [gpui::blue(), gpui::red(), gpui::green()];

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_5()
            .bg(gpui::white())
            .justify_center()
            .items_center()
            .text_color(rgb(0x333333))
            .child(
                div()
                    .text_xl()
                    .text_center()
                    .child("Drag between these windows"),
            )
            .child(
                div()
                    .w_full()
                    .mb_10()
                    .justify_center()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .items_center()
                    .children(items.into_iter().enumerate().map(|(ix, color)| {
                        let drag_info = DragInfo::new(ix, color);

                        div()
                            .id(("item", ix))
                            .size_32()
                            .flex()
                            .justify_center()
                            .items_center()
                            .border_2()
                            .border_color(color)
                            .text_color(color)
                            .cursor_move()
                            .hover(|this| this.bg(color.opacity(0.2)))
                            .child(format!("Item ({})", ix))
                            .on_drag(drag_info, |info: &DragInfo, position, _, cx| {
                                cx.new(|_| info.position(position))
                            })
                            .external_drag_payload(|_: &DragInfo, _, _| {
                                Some(ExternalDragPayload::Application(ApplicationDrag::new()))
                            })
                    })),
            )
            .child(
                div()
                    .id("drop-target")
                    .w_128()
                    .h_32()
                    .flex()
                    .justify_center()
                    .items_center()
                    .border_3()
                    .border_color(self.drop_on.map(|info| info.color).unwrap_or(gpui::black()))
                    .when_some(self.drop_on, |this, info| this.bg(info.color.opacity(0.5)))
                    .on_drop(cx.listener(|this, info: &DragInfo, window, cx| {
                        println!(
                            "Dropped item {} in {:?}",
                            info.ix,
                            window.window_handle().window_id()
                        );
                        this.drop_on = Some(*info);
                        cx.notify();
                    }))
                    .child("Drop items here"),
            )
    }
}

fn run_example() {
    #[cfg(not(target_family = "wasm"))]
    env_logger::init();
    application().run(|cx: &mut App| {
        let count = if cfg!(target_family = "wasm") { 1 } else { 2 };
        for index in 0..count {
            let mut bounds = Bounds::centered(None, size(px(620.), px(540.)), cx);
            if count == 2 {
                bounds.origin.x += px(if index == 0 { -320. } else { 320. });
            }
            if let Err(error) = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    window.set_window_title(&format!("GPUI drag window {}", index + 1));
                    cx.new(|_| DragDrop::new())
                },
            ) {
                eprintln!("Could not open drag example window: {error}");
            }
        }

        cx.activate(true);
    });
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    run_example();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run_example();
}
