//! Captain Zevo desktop app.

mod backend;
mod menus;
mod shell;
mod theme;

use std::sync::Arc;

use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use gpui_kit::*;

use backend::{Backend, DemoBackend, SystemBackend};

actions!(zevo, [Quit, Refresh]);

fn main() {
    // `--demo` shows example projects and touches nothing on this machine.
    let demo = std::env::args().any(|a| a == "--demo");
    let backend: Arc<dyn Backend> = if demo && std::env::args().any(|a| a == "--fresh") {
        // Example data as on a machine where setup has not run yet.
        Arc::new(DemoBackend::fresh())
    } else if demo {
        Arc::new(DemoBackend::default())
    } else {
        match SystemBackend::new() {
            Ok(backend) => Arc::new(backend),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    };

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::apply(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1180.), px(740.)),
                    cx,
                ))),
                window_min_size: Some(size(px(820.), px(520.))),
                ..TitleBar::window_options()
            };
            let (window, shell) = gpui_kit::open_window(options, cx, |window, cx| {
                window.set_window_title("Captain Zevo");
                // ZEVO_THEME=light|dark overrides the system appearance.
                match std::env::var("ZEVO_THEME").as_deref() {
                    Ok("dark") => Theme::change(ThemeMode::Dark, Some(window), cx),
                    Ok("light") => Theme::change(ThemeMode::Light, Some(window), cx),
                    _ => {
                        Theme::sync_system_appearance(Some(window), cx);
                        window
                            .observe_window_appearance(|window, cx| {
                                Theme::sync_system_appearance(Some(window), cx);
                                theme::apply(cx);
                            })
                            .detach();
                    }
                }
                theme::apply(cx);
                cx.new(|cx| shell::Shell::new(backend, demo, window, cx))
            })
            .expect("failed to open the main window");
            menus::install(cx, window, shell);
        });
}
