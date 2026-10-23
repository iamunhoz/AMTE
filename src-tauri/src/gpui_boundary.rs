use gpui::{
    App, Application, Bounds, Context, Render, SharedString, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size,
};

struct GpuiBoundary {
    message: SharedString,
}

impl Render for GpuiBoundary {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .gap_4()
            .bg(rgb(0x272822))
            .text_color(rgb(0xf8f8f2))
            .child(
                div()
                    .text_2xl()
                    .text_color(rgb(0x66d9ef))
                    .child("amte / GPUI"),
            )
            .child(
                div()
                    .max_w(px(420.0))
                    .text_center()
                    .text_color(rgb(0xa6e22e))
                    .child(self.message.clone()),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(560.0), px(360.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| GpuiBoundary {
                    message: "A native GPUI renderer has its own event loop and window. The single amte MVP editor is therefore hosted by Tauri instead of faking an unsupported shared window.".into(),
                })
            },
        )
        .expect("could not open the GPUI boundary window");
    });
}
