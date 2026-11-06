use gpui::{
    App, Application, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, prelude::*,
    px, rgb, size,
};

struct Root;

impl Render for Root {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x101418))
            .text_color(rgb(0xffffff))
            .child("Lan Mouse · GPUI spike")
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(900.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Root),
        )
        .expect("open window");
        cx.activate(true);
    });
}
