//! Chrome colors pulled from the active gpui-component theme.

use gpui::{App, Hsla};
use gpui_component::ActiveTheme;

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::{Theme, ThemeMode};

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
