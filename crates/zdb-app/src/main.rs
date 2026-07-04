//! zdb — Zed-like PostgreSQL client. Application entry point.

// On Windows, build as a GUI app (no console window). Keyed on the target, not
// on debug_assertions, because our release profile enables debug-assertions to
// make gpui cross-compile (it precompiles HLSL shaders only on a Windows host;
// the debug_assertions path compiles them at runtime instead).
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod assets;
mod lsp;
mod terminal;
mod workspace;

use assets::Assets;
use gpui::{px, size, AppContext, Application, KeyBinding, WindowOptions};
use gpui_component::TitleBar;
use workspace::{
    ClosePalette, RunQuery, ToggleConnections, TogglePalette, ToggleScratch, ToggleSettings,
    ToggleTerminal, Workspace,
};
use zdb_db::{ConnectionConfig, DbHandle, SslMode};

fn main() {
    // WSLg ships a Wayland compositor whose protocol version gpui 0.2.2 rejects
    // (panics with `UnsupportedVersion`). WSLg also provides XWayland, and gpui's
    // X11 backend works there, so under WSL we drop WAYLAND_DISPLAY to force X11.
    // Real Linux Wayland sessions are left untouched.
    if is_wsl() && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        // SAFETY: set before any window system / threads are initialized.
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
    }

    // Diagnostic: verify the OS keychain round-trips, then exit. (ZDB_KEYRING_TEST)
    if std::env::var_os("ZDB_KEYRING_TEST").is_some() {
        let set = zdb_config::secret::set_password("zdbtest", "hunter2");
        let got = zdb_config::secret::get_password("zdbtest");
        eprintln!("[zdb] keychain set={set} get={got:?}");
        zdb_config::secret::delete_password("zdbtest");
        eprintln!(
            "[zdb] keychain after delete={:?}",
            zdb_config::secret::get_password("zdbtest")
        );
        return;
    }

    let settings = zdb_config::Settings::load().unwrap_or_default();
    let theme = settings.theme;
    let db = DbHandle::spawn();
    // Auto-connect only from explicit env (dev). Otherwise the connection
    // manager opens with the saved list for the user to pick.
    let auto = config_from_env();

    let app = Application::new().with_assets(Assets);
    app.run(move |cx| {
        // Must run before using any gpui-component features.
        gpui_component::init(cx);
        // White scheme by default; `theme: "dark"` in settings.json switches it.
        gpui_component::Theme::change(theme_mode(theme), None, cx);

        cx.bind_keys([
            KeyBinding::new("ctrl-enter", RunQuery, None),
            KeyBinding::new("cmd-enter", RunQuery, None),
            KeyBinding::new("ctrl-shift-p", TogglePalette, None),
            KeyBinding::new("cmd-shift-p", TogglePalette, None),
            KeyBinding::new("escape", ClosePalette, None),
            KeyBinding::new("ctrl-`", ToggleTerminal, None),
            KeyBinding::new("cmd-`", ToggleTerminal, None),
            KeyBinding::new("ctrl-shift-o", ToggleConnections, None),
            KeyBinding::new("cmd-shift-o", ToggleConnections, None),
            KeyBinding::new("ctrl-shift-e", ToggleScratch, None),
            KeyBinding::new("cmd-shift-e", ToggleScratch, None),
            KeyBinding::new("ctrl-,", ToggleSettings, None),
            KeyBinding::new("cmd-,", ToggleSettings, None),
        ]);

        // Use a Zed-style custom title bar instead of the OS default. The
        // `appears_transparent` option (set by `TitleBar::title_bar_options`)
        // makes gpui hide the native title bar on Windows/Linux and lets the app
        // paint the whole window; `gpui_component::TitleBar` (rendered in the
        // workspace) draws the drag region + min/restore/maximize/close controls.
        let window_options = WindowOptions {
            titlebar: Some(TitleBar::title_bar_options()),
            window_min_size: Some(size(px(640.), px(480.))),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                let view =
                    cx.new(|cx| Workspace::new(db.clone(), settings.clone(), auto.clone(), window, cx));
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            })
            .expect("failed to open zdb window");
        })
        .detach();
    });
}

/// Build a connection config from `ZDB_*` env vars, for development
/// auto-connect. Returns `None` if `ZDB_HOST` is unset.
fn config_from_env() -> Option<ConnectionConfig> {
    let host = std::env::var("ZDB_HOST").ok()?;
    let user = std::env::var("ZDB_USER").unwrap_or_else(|_| "postgres".into());
    let dbname = std::env::var("ZDB_DB").unwrap_or_else(|_| user.clone());
    let mut cfg = ConnectionConfig::new("dev", host, dbname, user);
    cfg.password = std::env::var("ZDB_PASSWORD").ok();
    if let Ok(p) = std::env::var("ZDB_PORT") {
        if let Ok(p) = p.parse() {
            cfg.port = p;
        }
    }
    if std::env::var("ZDB_SSL_DISABLE").is_ok() {
        cfg.ssl_mode = SslMode::Disable;
    }
    Some(cfg)
}

fn theme_mode(t: zdb_config::Theme) -> gpui_component::ThemeMode {
    match t {
        zdb_config::Theme::Light => gpui_component::ThemeMode::Light,
        zdb_config::Theme::Dark => gpui_component::ThemeMode::Dark,
    }
}

/// Detect Microsoft WSL so we can force the X11 backend (see `main`).
fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.to_lowercase().contains("microsoft"))
            .unwrap_or(false)
}
