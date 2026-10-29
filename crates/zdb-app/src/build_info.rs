//! Version + build metadata baked in at compile time by `build.rs` (see there
//! for where each value comes from). Shown in Settings → About and printed by
//! `zdb --version`.

/// `0.5.0` for a release, `0.5.0-3-gabc1234[-dirty]` for a dev build.
pub const VERSION: &str = env!("ZDB_BUILD_VERSION");
/// Full commit hash; empty when built outside a git checkout.
pub const COMMIT: &str = env!("ZDB_BUILD_COMMIT");
/// Tracked files differed from the commit at build time.
pub const DIRTY: bool = !env!("ZDB_BUILD_DIRTY").is_empty();
pub const COMMIT_DATE: &str = env!("ZDB_BUILD_COMMIT_DATE");
pub const BUILD_DATE: &str = env!("ZDB_BUILD_DATE");
pub const TARGET: &str = env!("ZDB_BUILD_TARGET");
/// `release` / `debug`.
pub const PROFILE: &str = env!("ZDB_BUILD_PROFILE");
pub const RUSTC: &str = env!("ZDB_BUILD_RUSTC");
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// `abc1234`, `abc1234 (dirty)`, or `unknown`.
pub fn commit_label() -> String {
    match COMMIT.get(..7) {
        Some(short) if DIRTY => format!("{short} (dirty)"),
        Some(short) => short.to_string(),
        None => "unknown".into(),
    }
}

/// The commit on GitHub, when the commit is known.
pub fn commit_url() -> Option<String> {
    (!COMMIT.is_empty()).then(|| format!("{REPOSITORY}/commit/{COMMIT}"))
}

/// (label, value) pairs, in display order.
pub fn rows() -> Vec<(&'static str, String)> {
    vec![
        ("Version", VERSION.to_string()),
        ("Commit", commit_label()),
        ("Committed", COMMIT_DATE.to_string()),
        ("Built", BUILD_DATE.to_string()),
        ("Target", format!("{TARGET} ({PROFILE})")),
        ("Compiler", RUSTC.to_string()),
    ]
}

/// Plain-text summary for `--version` and the About section's Copy button.
pub fn summary() -> String {
    let mut out = format!("zdb {VERSION}\n");
    for (label, value) in rows().into_iter().skip(1) {
        out.push_str(&format!(
            "{:<11}{value}\n",
            format!("{}:", label.to_lowercase())
        ));
    }
    out
}

/// `zdb --version`. The Windows build is a GUI-subsystem exe with no console of
/// its own, so it attaches to the launching one first (without that the output
/// goes nowhere; redirected output works either way). cmd doesn't wait for GUI
/// apps, so there the text lands after the next prompt.
pub fn print_version() {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn AttachConsole(process_id: u32) -> i32;
        }
        const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
        // SAFETY: plain Win32 call; failure (no parent console) is harmless.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
    print!("{}", summary());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_lists_every_field() {
        let s = summary();
        assert!(s.starts_with(&format!("zdb {VERSION}\n")));
        for label in ["commit:", "committed:", "built:", "target:", "compiler:"] {
            assert!(s.contains(label), "{label} missing from {s}");
        }
        assert!(!VERSION.is_empty());
        assert!(s.contains(TARGET));
    }

    #[test]
    fn commit_url_points_at_the_repo() {
        match commit_url() {
            Some(url) => assert_eq!(url, format!("https://github.com/tochk/zdb/commit/{COMMIT}")),
            None => assert!(COMMIT.is_empty()),
        }
    }
}
