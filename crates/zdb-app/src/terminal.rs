//! Embedded terminal: the shell picked in Settings (else the platform's
//! default, `default_shell`) in a PTY, shown by `TerminalView` in the app's
//! theme. Generic terminal — the user can run anything (e.g. `claude`, `psql`);
//! no DB context is injected.

use crate::terminal_view::TerminalView;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use anyhow::{bail, Result};
use futures_channel::oneshot;
use gpui::{AppContext, Context, Entity, Hsla, Rgba, SharedString};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A live terminal: the renderable view plus the PTY handles that must outlive
/// it (dropping the master would close the shell's console). Dropping it
/// kills the shell.
pub struct Terminal {
    pub view: Entity<TerminalView>,
    /// The command line it runs, for the panel header.
    pub command: String,
    killer: Box<dyn ChildKiller + Send + Sync>,
    _master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

impl Terminal {
    /// Repaint in another palette / font (the theme was switched).
    pub fn restyle(&self, style: TerminalStyle, cx: &mut gpui::App) {
        self.view.update(cx, |view, cx| view.set_style(style, cx));
    }
}

/// Answers the "where is the cursor?" query (`ESC [ 6 n`) in the shell's
/// output on its way to the view. portable-pty opens ConPTY with
/// `PSEUDOCONSOLE_INHERIT_CURSOR`, so conhost asks that first and draws
/// NOTHING until it hears back — PowerShell sat on a blank terminal. alacritty
/// does produce the reply, as an `Event::PtyWrite`, but gpui-terminal drops
/// those. Windows only: there conhost answers its programs' own queries, so
/// this one (sent while the screen is still empty — top left) is the only one
/// that reaches us; elsewhere a shell's query wants the real position, which
/// only the view knows.
struct CursorReport<R> {
    inner: R,
    input: SharedWriter,
    /// Bytes of the query matched so far (it may span two reads).
    matched: usize,
}

const CURSOR_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_AT_TOP_LEFT: &[u8] = b"\x1b[1;1R";

impl<R> CursorReport<R> {
    fn new(inner: R, input: SharedWriter) -> Self {
        Self {
            inner,
            input,
            matched: 0,
        }
    }
}

impl<R: Read> Read for CursorReport<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        for &b in &buf[..n] {
            self.matched = if b == CURSOR_QUERY[self.matched] {
                self.matched + 1
            } else {
                usize::from(b == CURSOR_QUERY[0])
            };
            if self.matched == CURSOR_QUERY.len() {
                self.matched = 0;
                let _ = self.input.write_all(CURSOR_AT_TOP_LEFT);
                let _ = self.input.flush();
            }
        }
        Ok(n)
    }
}

/// The PTY's writer behind a shared lock (it can only be taken once): the view
/// types into it, `CursorReport` answers through it.
#[derive(Clone)]
struct SharedWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().map_err(|_| poisoned())?.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().map_err(|_| poisoned())?.flush()
    }
}

fn poisoned() -> std::io::Error {
    std::io::Error::other("terminal input lock poisoned")
}

/// How the terminal looks: colors from the app theme, the theme's code font.
#[derive(Clone)]
pub struct TerminalStyle {
    pub colors: Palette,
    pub font_family: SharedString,
    pub font_size: gpui::Pixels,
    /// What's behind selected text.
    pub selection: Hsla,
}

impl TerminalStyle {
    /// The terminal's look for the active theme.
    pub fn from_theme(cx: &gpui::App) -> Self {
        use gpui_component::ActiveTheme;
        let t = cx.theme();
        Self {
            colors: Palette::new(t.mode.is_dark(), t.sidebar, t.foreground, t.caret),
            font_family: t.mono_font_family.clone(),
            font_size: t.mono_font_size,
            selection: t.selection,
        }
    }
}

/// The terminal's colors. The 16 ANSI ones read on the theme's background: One
/// Dark / One Light (Zed's defaults). In the light one "white" is a mid grey and
/// "bright white" near-black — real whites would vanish on a light ground — and
/// the yellows are darkened for the same reason.
#[derive(Clone)]
pub struct Palette {
    pub background: Hsla,
    pub foreground: Hsla,
    pub cursor: Hsla,
    ansi: [Hsla; 16],
}

impl Palette {
    fn new(dark: bool, background: Hsla, foreground: Hsla, cursor: Hsla) -> Self {
        let ansi: [u32; 16] = if dark {
            [
                0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xdcdfe4,
                0x5c6370, 0xea858b, 0xaad581, 0xffd885, 0x85c1ff, 0xd398eb, 0x6ed5de, 0xfafafa,
            ]
        } else {
            [
                0x383a42, 0xca1243, 0x50a14f, 0xc18401, 0x4078f2, 0xa626a4, 0x0184bc, 0x7f848e,
                0xa0a1a7, 0xe45649, 0x3d8b3c, 0x986801, 0x2a62d8, 0x8c1f8a, 0x0b6e99, 0x2b2d33,
            ]
        };
        Self {
            background,
            foreground,
            cursor,
            ansi: ansi.map(|c| gpui::rgb(c).into()),
        }
    }

    /// A cell's color: what a program set with OSC 4 / 10 / 11 first, then ours.
    pub fn resolve(&self, color: Color, colors: &Colors) -> Hsla {
        match color {
            Color::Spec(rgb) => rgb_hsla(rgb),
            Color::Indexed(ix) => match colors[ix as usize] {
                Some(rgb) => rgb_hsla(rgb),
                None => self.indexed(ix),
            },
            Color::Named(named) => match colors[named] {
                Some(rgb) => rgb_hsla(rgb),
                None => self.named(named),
            },
        }
    }

    fn named(&self, named: NamedColor) -> Hsla {
        let dim = |mut c: Hsla| {
            c.l *= 0.7;
            c
        };
        match named as usize {
            ix @ 0..=15 => self.ansi[ix],
            _ => match named {
                NamedColor::Background => self.background,
                NamedColor::Cursor => self.cursor,
                NamedColor::DimForeground => dim(self.foreground),
                NamedColor::DimBlack => dim(self.ansi[0]),
                NamedColor::DimRed => dim(self.ansi[1]),
                NamedColor::DimGreen => dim(self.ansi[2]),
                NamedColor::DimYellow => dim(self.ansi[3]),
                NamedColor::DimBlue => dim(self.ansi[4]),
                NamedColor::DimMagenta => dim(self.ansi[5]),
                NamedColor::DimCyan => dim(self.ansi[6]),
                NamedColor::DimWhite => dim(self.ansi[7]),
                _ => self.foreground,
            },
        }
    }

    /// The 256-color table: ANSI, then a 6×6×6 cube, then 24 greys.
    fn indexed(&self, ix: u8) -> Hsla {
        let level = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
        match ix {
            0..=15 => self.ansi[ix as usize],
            16..=231 => {
                let n = ix - 16;
                rgb_hsla(Rgb {
                    r: level(n / 36),
                    g: level(n / 6 % 6),
                    b: level(n % 6),
                })
            }
            232..=255 => {
                let v = 8 + (ix - 232) * 10;
                rgb_hsla(Rgb { r: v, g: v, b: v })
            }
        }
    }
}

fn rgb_hsla(c: Rgb) -> Hsla {
    Rgba {
        r: f32::from(c.r) / 255.,
        g: f32::from(c.g) / 255.,
        b: f32::from(c.b) / 255.,
        a: 1.,
    }
    .into()
}

/// Spawn `command` (a command line; empty = `default_shell()`) in a new PTY
/// and build its view. The receiver resolves once the shell process exits.
pub fn spawn<V: 'static>(
    command: &str,
    style: TerminalStyle,
    cx: &mut Context<V>,
) -> Result<(Terminal, oneshot::Receiver<()>)> {
    let command = match command.trim() {
        "" => default_shell(),
        c => c.to_string(),
    };
    let argv = split_command(&command);
    if argv.is_empty() {
        bail!("no shell command");
    }

    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // portable-pty looks the program up on PATH and starts in the home
    // directory (USERPROFILE on Windows).
    let mut cmd = CommandBuilder::from_argv(argv.iter().map(Into::into).collect());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let killer = child.clone_killer();
    // Watch for the shell exiting. On Windows the console's output doesn't
    // end when its process does (only when the console is closed), so the
    // view would never notice by itself.
    let (exited_tx, exited) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = child.wait();
        let _ = exited_tx.send(());
    });

    let input = SharedWriter(Arc::new(Mutex::new(pair.master.take_writer()?)));
    let reader = pair.master.try_clone_reader()?;
    // See `CursorReport`: without it nothing ever shows on Windows.
    let reader: Box<dyn Read + Send> = if cfg!(windows) {
        Box::new(CursorReport::new(reader, input.clone()))
    } else {
        reader
    };
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

    let view = cx.new(|cx| TerminalView::new(input, reader, resize, style, cx));

    Ok((
        Terminal {
            view,
            command,
            killer,
            _master: master,
        },
        exited,
    ))
}

/// The shell used when Settings names none: PowerShell on Windows (7 if it's
/// installed, else the built-in 5.1), the user's login shell on macOS, their
/// `$SHELL` elsewhere.
pub fn default_shell() -> String {
    if cfg!(windows) {
        if find_on_path("pwsh.exe").is_some() {
            "pwsh.exe".into()
        } else {
            "powershell.exe".into()
        }
    } else {
        let fallback = if cfg!(target_os = "macos") {
            "/bin/zsh"
        } else if Path::new("/bin/bash").exists() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| fallback.into());
        // A Finder-launched app has a bare PATH; a login shell reads the
        // profile that sets it up, like Terminal.app does.
        if cfg!(target_os = "macos") {
            format!("{} -l", quote(&shell))
        } else {
            quote(&shell)
        }
    }
}

/// Shells found on this machine, for the Settings picker (default first).
pub fn shell_choices() -> Vec<String> {
    let mut out = vec![default_shell()];
    let mut push = |c: String| {
        if !out.contains(&c) {
            out.push(c);
        }
    };
    if cfg!(windows) {
        for exe in ["pwsh.exe", "powershell.exe", "cmd.exe", "wsl.exe"] {
            if find_on_path(exe).is_some() {
                push(exe.into());
            }
        }
        for git in [
            std::env::var_os("ProgramFiles").map(PathBuf::from),
            std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Programs")),
        ]
        .into_iter()
        .flatten()
        {
            let bash = git.join("Git").join("bin").join("bash.exe");
            if bash.is_file() {
                push(format!("{} -l", quote(&bash.to_string_lossy())));
            }
        }
    } else {
        let login = if cfg!(target_os = "macos") { " -l" } else { "" };
        let listed = std::fs::read_to_string("/etc/shells").unwrap_or_default();
        for line in listed.lines().map(str::trim) {
            if line.starts_with('/') && Path::new(line).is_file() {
                push(format!("{}{login}", quote(line)));
            }
        }
    }
    out
}

/// `exe` in a directory on PATH.
fn find_on_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|p| p.is_file())
}

/// A program path as one command-line word: quoted when it has a space.
pub fn quote(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

/// Split a command line into words: whitespace separates, double or single
/// quotes group (and are dropped). No backslash escapes — they're Windows
/// path separators.
pub fn split_command(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote = None;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_split_into_words() {
        assert_eq!(split_command("pwsh.exe"), ["pwsh.exe"]);
        assert_eq!(split_command("  /bin/zsh   -l "), ["/bin/zsh", "-l"]);
        assert_eq!(
            split_command(r#""C:\Program Files\Git\bin\bash.exe" -l"#),
            [r"C:\Program Files\Git\bin\bash.exe", "-l"]
        );
        assert_eq!(
            split_command("wsl.exe -d 'Ubuntu 24.04'"),
            ["wsl.exe", "-d", "Ubuntu 24.04"]
        );
        // An empty quoted argument is still an argument.
        assert_eq!(split_command(r#"sh -c """#), ["sh", "-c", ""]);
        assert!(split_command("   ").is_empty());
    }

    #[test]
    fn paths_with_spaces_round_trip() {
        let path = r"C:\Program Files\PowerShell\7\pwsh.exe";
        assert_eq!(split_command(&quote(path)), [path]);
        assert_eq!(quote("/bin/bash"), "/bin/bash");
    }

    #[test]
    fn there_is_always_a_default_shell() {
        let shell = default_shell();
        assert!(!split_command(&shell).is_empty());
        assert_eq!(shell_choices().first(), Some(&shell));
    }

    /// The cursor query is answered once per query, also when a read splits
    /// it, and the output itself goes through untouched.
    #[test]
    fn cursor_queries_are_answered() {
        let replies = Arc::new(Mutex::new(Vec::<u8>::new()));
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let input = SharedWriter(Arc::new(Mutex::new(Box::new(Sink(replies.clone())))));
        // Reads of at most 3 bytes: the query arrives in pieces.
        struct Chunks(std::collections::VecDeque<u8>);
        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = buf.len().min(3).min(self.0.len());
                for b in buf.iter_mut().take(n) {
                    *b = self.0.pop_front().unwrap();
                }
                Ok(n)
            }
        }
        let output = b"\x1b[?25l\x1b[6nPS C:\\> \x1b\x1b[6n".to_vec();
        let mut report = CursorReport::new(Chunks(output.clone().into()), input);
        let mut seen = Vec::new();
        report.read_to_end(&mut seen).unwrap();
        assert_eq!(seen, output);
        assert_eq!(*replies.lock().unwrap(), b"\x1b[1;1R\x1b[1;1R");
    }

    #[test]
    fn colors_resolve() {
        let bg: Hsla = gpui::rgb(0x102030).into();
        let dark = Palette::new(true, bg, gpui::white(), gpui::white());
        let colors = Colors::default();
        assert_eq!(
            dark.resolve(Color::Named(NamedColor::Background), &colors),
            bg
        );
        let spec = Rgb { r: 1, g: 2, b: 3 };
        assert_eq!(dark.resolve(Color::Spec(spec), &colors), rgb_hsla(spec));
        // The cube's corners and the grey ramp's ends.
        let at = |ix| dark.resolve(Color::Indexed(ix), &colors);
        assert_eq!(at(16), rgb_hsla(Rgb { r: 0, g: 0, b: 0 }));
        assert_eq!(
            at(231),
            rgb_hsla(Rgb {
                r: 255,
                g: 255,
                b: 255
            })
        );
        assert_eq!(at(232), rgb_hsla(Rgb { r: 8, g: 8, b: 8 }));
        assert_eq!(
            at(255),
            rgb_hsla(Rgb {
                r: 238,
                g: 238,
                b: 238
            })
        );
        // Light and dark differ where it matters for legibility.
        let light = Palette::new(false, gpui::white(), gpui::black(), gpui::black());
        assert_ne!(light.ansi[7], dark.ansi[7]);
    }
}
