//! The application menu bar and its keyboard shortcuts.

use gpui_kit::*;

use crate::shell::{MenuCommand, Shell};
use crate::{Quit, Refresh};

const WEBSITE: &str = "https://github.com/bessonette/captain-zevo";

actions!(
    zevo,
    [
        About,
        ShowSettings,
        Hide,
        HideOthers,
        ShowAll,
        AddProject,
        CloseWindow,
        ShowProjects,
        ShowTraefik,
        ShowHosts,
        Minimize,
        Zoom,
        OpenWebsite
    ]
);

/// Installs the menu bar, shortcuts and their handlers for the main window.
pub fn install(cx: &mut App, window: AnyWindowHandle, shell: Entity<Shell>) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-r", Refresh, None),
        KeyBinding::new("ctrl-r", Refresh, None),
        KeyBinding::new("cmd-n", AddProject, None),
        KeyBinding::new("ctrl-n", AddProject, None),
        KeyBinding::new("cmd-,", ShowSettings, None),
        KeyBinding::new("f1", About, None),
        KeyBinding::new("cmd-1", ShowProjects, None),
        KeyBinding::new("cmd-2", ShowTraefik, None),
        KeyBinding::new("cmd-3", ShowHosts, None),
        KeyBinding::new("ctrl-1", ShowProjects, None),
        KeyBinding::new("ctrl-2", ShowTraefik, None),
        KeyBinding::new("ctrl-3", ShowHosts, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
    ]);

    cx.set_menus([
        Menu::new("Captain Zevo").items([
            MenuItem::action("About Captain Zevo", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", ShowSettings),
            MenuItem::separator(),
            MenuItem::action("Hide Captain Zevo", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Captain Zevo", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Add Project…", AddProject),
            MenuItem::action("Refresh", Refresh),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        Menu::new("View").items([
            MenuItem::action("Projects", ShowProjects),
            MenuItem::action("Traefik", ShowTraefik),
            MenuItem::action("Hosts", ShowHosts),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
        Menu::new("Help").items([MenuItem::action("Captain Zevo on GitHub", OpenWebsite)]),
    ]);

    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &OpenWebsite, cx| cx.open_url(WEBSITE));

    // Commands for the main window. Handled at the application level, so
    // they work wherever the keyboard focus is. The work is deferred: the
    // handler runs while the window is still processing the key press or
    // menu click, and a window cannot be updated from inside its own update.
    fn in_window(
        cx: &mut App,
        window: AnyWindowHandle,
        work: impl FnOnce(&mut Window, &mut App) + 'static,
    ) {
        cx.defer(move |cx| {
            _ = window.update(cx, |_, window, cx| work(window, cx));
        });
    }

    fn route<A: Action>(
        cx: &mut App,
        window: AnyWindowHandle,
        shell: &Entity<Shell>,
        command: MenuCommand,
    ) {
        let shell = shell.clone();
        cx.on_action(move |_: &A, cx| {
            let shell = shell.clone();
            in_window(cx, window, move |window, cx| {
                shell.update(cx, |shell, cx| shell.menu_command(command, window, cx));
            });
        });
    }
    route::<About>(cx, window, &shell, MenuCommand::About);
    route::<ShowSettings>(cx, window, &shell, MenuCommand::Settings);
    route::<AddProject>(cx, window, &shell, MenuCommand::AddProject);
    route::<Refresh>(cx, window, &shell, MenuCommand::Refresh);
    route::<ShowProjects>(cx, window, &shell, MenuCommand::Projects);
    route::<ShowTraefik>(cx, window, &shell, MenuCommand::Traefik);
    route::<ShowHosts>(cx, window, &shell, MenuCommand::Hosts);

    cx.on_action(move |_: &CloseWindow, cx| {
        in_window(cx, window, |window, _| window.remove_window())
    });
    cx.on_action(move |_: &Minimize, cx| {
        in_window(cx, window, |window, _| window.minimize_window())
    });
    cx.on_action(move |_: &Zoom, cx| in_window(cx, window, |window, _| window.zoom_window()));
}
