//! Captain Zevo's colours, layered over the component library's theme.
//!
//! One palette per mode, cool blue-grey neutrals with a teal accent. Screens
//! read colours from the theme by role, so this file is the only place the
//! look is defined.

use gpui_kit::component::Theme;
use gpui_kit::{App, Hsla, px, rgb};

struct Palette {
    /// Main surface.
    window: u32,
    /// Sidebar, title bar, code blocks and other recessed surfaces.
    panel: u32,
    /// Hovered rows and subtle fills.
    hover: u32,
    line: u32,
    ink: u32,
    ink_muted: u32,
    accent: u32,
    accent_hover: u32,
    accent_active: u32,
    /// Text on the accent colour.
    on_accent: u32,
    ok: u32,
    warn: u32,
    bad: u32,
}

const LIGHT: Palette = Palette {
    window: 0xfbfcfd,
    panel: 0xf1f4f7,
    hover: 0xe8edf2,
    line: 0xd9dfe6,
    ink: 0x16202b,
    ink_muted: 0x5c6b7a,
    accent: 0x0f6e8c,
    accent_hover: 0x0c5f79,
    accent_active: 0x0a5066,
    on_accent: 0xffffff,
    ok: 0x1f8a4c,
    warn: 0xb07a12,
    bad: 0xb93a32,
};

const DARK: Palette = Palette {
    window: 0x141c24,
    panel: 0x10171e,
    hover: 0x1e2a35,
    line: 0x2a3743,
    ink: 0xe6edf3,
    ink_muted: 0x93a2b0,
    accent: 0x4fb3d1,
    accent_hover: 0x66c0db,
    accent_active: 0x7ccbe3,
    on_accent: 0x06222c,
    ok: 0x5fc98a,
    warn: 0xe0a94a,
    bad: 0xef8279,
};

/// Applies the palette for the theme's current mode. Call after every mode
/// change, since switching modes reloads the library's own colours.
pub fn apply(cx: &mut App) {
    Theme::update(cx, |theme| {
        let p = if theme.mode.is_dark() { &DARK } else { &LIGHT };
        let c = |hex: u32| -> Hsla { rgb(hex).into() };

        theme.font_size = px(14.);

        theme.background = c(p.window);
        theme.foreground = c(p.ink);
        theme.muted = c(p.panel);
        theme.muted_foreground = c(p.ink_muted);
        theme.border = c(p.line);
        theme.ring = c(p.accent);
        theme.link = c(p.accent);
        theme.link_hover = c(p.accent_hover);
        theme.link_active = c(p.accent_active);

        theme.primary = c(p.accent);
        theme.primary_hover = c(p.accent_hover);
        theme.primary_active = c(p.accent_active);
        theme.primary_foreground = c(p.on_accent);
        theme.button_primary = c(p.accent);
        theme.button_primary_hover = c(p.accent_hover);
        theme.button_primary_active = c(p.accent_active);
        theme.button_primary_foreground = c(p.on_accent);

        theme.secondary = c(p.panel);
        theme.secondary_hover = c(p.hover);
        theme.secondary_active = c(p.line);
        theme.secondary_foreground = c(p.ink);
        theme.accent = c(p.hover);
        theme.accent_foreground = c(p.ink);
        theme.list_hover = c(p.hover);
        theme.list_active = c(p.hover);

        theme.sidebar = c(p.panel);
        theme.sidebar_foreground = c(p.ink);
        theme.sidebar_border = c(p.line);
        theme.sidebar_accent = c(p.hover);
        theme.sidebar_accent_foreground = c(p.ink);
        theme.title_bar = c(p.panel);
        theme.title_bar_border = c(p.line);

        theme.popover = c(p.window);
        theme.popover_foreground = c(p.ink);
        theme.tab_active_foreground = c(p.ink);
        theme.tab_foreground = c(p.ink_muted);

        theme.success = c(p.ok);
        theme.warning = c(p.warn);
        theme.danger = c(p.bad);
    });
}
