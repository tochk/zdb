//! Chrome colors pulled from the active gpui-component theme.

use gpui::{rgb, rgba, App, Hsla};
use gpui_component::{ActiveTheme, Theme, ThemeMode};

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Colors {
    pub(crate) sidebar: Hsla,
    pub(crate) center: Hsla,
    pub(crate) messages: Hsla,
    pub(crate) header: Hsla,
    pub(crate) border: Hsla,
    pub(crate) fg: Hsla,
    pub(crate) fg_dim: Hsla,
    pub(crate) fg_null: Hsla,
    pub(crate) panel: Hsla,
    pub(crate) accent: Hsla,
    /// Row/list hover background.
    pub(crate) hover: Hsla,
    /// Selected/active row background.
    pub(crate) active: Hsla,
    /// Error/destructive foreground (readable in both theme modes).
    pub(crate) danger: Hsla,
    /// Result-grid rule; same token the table widget uses for its row lines, so
    /// our column separators match them.
    pub(crate) grid: Hsla,
}

/// Switch the app's theme. The stock themes' `secondary` is too light for our
/// chrome (it's the title bar, toolbars and panel headers, and the default
/// button's fill): buttons would stand out as boxes, so it stays one step off the
/// background.
pub(crate) fn change_theme(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    let (base, hover, active) = if mode.is_dark() {
        (0x171717, 0x1a1a1a, 0x262626)
    } else {
        (0xf5f5f5, 0xf0f0f0, 0xe5e5e5)
    };
    Theme::update(cx, |theme| {
        theme.secondary = rgb(base).into();
        theme.secondary_hover = rgb(hover).into();
        theme.secondary_active = rgb(active).into();
        theme.button_secondary = rgb(base).into();
        theme.button_secondary_hover = rgb(hover).into();
        theme.button_secondary_active = rgb(active).into();
        // Zebra rows: the stock dark theme no longer has them.
        if mode.is_dark() {
            theme.table_even = rgba(0x171717cc).into();
        }
    });
}

pub(crate) fn palette(cx: &App) -> Colors {
    let t = cx.theme();
    Colors {
        sidebar: t.sidebar,
        center: t.background,
        messages: t.sidebar,
        header: t.secondary,
        border: t.border,
        fg: t.foreground,
        fg_dim: t.muted_foreground,
        fg_null: t.accent,
        panel: t.popover,
        accent: t.accent,
        hover: t.list_hover,
        active: t.list_active,
        danger: t.danger,
        grid: t.table_row_border,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn theme_switch_changes_palette(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
        });
        let light = cx.read(palette);
        cx.update(|cx| Theme::change(ThemeMode::Dark, None, cx));
        let dark = cx.read(palette);
        assert_ne!(light.center, dark.center);
        assert_ne!(light.fg, dark.fg);
    }
}
