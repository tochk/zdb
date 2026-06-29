//! Embedded terminal: spawn the user's login shell in a PTY and wrap it in a
//! `gpui_terminal::TerminalView`. Generic terminal — the user can run anything
//! (e.g. `claude`, `psql`); no DB context is injected.

use anyhow::Result;
use gpui::{px, AppContext, Context, Edges, Entity};
use gpui_terminal::{TerminalConfig, TerminalView};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::sync::{Arc, Mutex};

/// A live terminal: the renderable view plus the PTY handles that must outlive
/// it (dropping the child or master would close the shell).
pub struct Terminal {
    pub view: Entity<TerminalView>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
}

/// Spawn a login shell in a new PTY and build its terminal view.
pub fn spawn<V: 'static>(cx: &mut Context<V>) -> Result<Terminal> {
    let shell = if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    };

    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;
    let master = Arc::new(Mutex::new(pair.master));

    let resize_master = master.clone();
    let resize = move |cols: usize, rows: usize| {
        if let Ok(m) = resize_master.lock() {
            let _ = m.resize(PtySize {
                cols: cols as u16,
                rows: rows as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    };

    let config = TerminalConfig {
        font_size: px(13.0),
        line_height_multiplier: 1.2,
        padding: Edges::all(px(6.0)),
        ..Default::default()
    };

    let view = cx.new(|cx| {
        TerminalView::new(writer, reader, config, cx).with_resize_callback(resize)
    });

    Ok(Terminal {
        view,
        _child: child,
        _master: master,
    })
}
