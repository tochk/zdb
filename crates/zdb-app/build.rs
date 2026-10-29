//! Build metadata (version, commit, dates, target) + the Windows resources.
//!
//! Build info: exported as `ZDB_BUILD_*` env vars for `env!` in
//! `src/build_info.rs` (Settings → About, `zdb --version`). The version is
//! `$ZDB_VERSION` when set (the release workflow passes the version it is
//! cutting), else `git describe --tags --dirty` (`0.5.0-3-gabc1234-dirty`), else
//! the Cargo version. No git (a source tarball) → commit "unknown".
//!
//! Windows resources: icon + VERSIONINFO (Explorer → Properties → Details) in an
//! `.rc` generated into OUT_DIR. gpui calls `TaskDialogIndirect`, which needs
//! comctl32 v6, selected by a manifest declaring a dependency on
//! Microsoft.Windows.Common-Controls 6.0. gpui embeds such a manifest, but only
//! when ITS build script runs on a Windows host. So the manifest source differs:
//!   - cross-compiling from a non-Windows host: gpui skips it, so we embed our
//!     own `resources/zdb.manifest`.
//!   - native Windows build: gpui already provides the manifest, so we must NOT
//!     add one — MSVC's CVTRES rejects a duplicate MANIFEST resource (CVT1100 ->
//!     LNK1123). (`lld` merges duplicates, which is why the cross build never
//!     hit this.)

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Re-embed whenever the icon / manifest change, so updating zdb.ico actually
    // replaces the baked-in icon.
    println!("cargo:rerun-if-changed=resources/zdb.ico");
    println!("cargo:rerun-if-changed=resources/zdb.manifest");
    println!("cargo:rerun-if-env-changed=ZDB_VERSION");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let info = BuildInfo::collect();
    info.export();
    windows_resources(&info);
}

struct BuildInfo {
    version: String,
    /// Full hash; empty when unknown.
    commit: String,
    dirty: bool,
    commit_date: String,
    build_date: String,
    target: String,
    profile: String,
    rustc: String,
}

impl BuildInfo {
    fn collect() -> Self {
        let git_ok = git(&["rev-parse", "--git-dir"]).is_some();
        if git_ok {
            watch_git();
        }
        let commit = git(&["rev-parse", "HEAD"]).unwrap_or_default();
        let dirty =
            git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        let version = std::env::var("ZDB_VERSION")
            .ok()
            .map(|v| v.trim().trim_start_matches('v').to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                git(&["describe", "--tags", "--dirty", "--match", "v[0-9]*"])
                    .map(|d| d.trim_start_matches('v').to_string())
            })
            .unwrap_or_else(|| env("CARGO_PKG_VERSION"));
        let commit_date = git(&["log", "-1", "--format=%ct"])
            .and_then(|s| s.parse().ok())
            .map(format_utc)
            .unwrap_or_else(|| "unknown".into());
        let build_epoch = std::env::var("SOURCE_DATE_EPOCH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            });
        let rustc = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
            .arg("-V")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".into());
        BuildInfo {
            version,
            commit,
            dirty,
            commit_date,
            build_date: format_utc(build_epoch),
            target: env("TARGET"),
            profile: env("PROFILE"),
            rustc,
        }
    }

    fn export(&self) {
        let vars = [
            ("ZDB_BUILD_VERSION", self.version.as_str()),
            ("ZDB_BUILD_COMMIT", self.commit.as_str()),
            ("ZDB_BUILD_DIRTY", if self.dirty { "1" } else { "" }),
            ("ZDB_BUILD_COMMIT_DATE", self.commit_date.as_str()),
            ("ZDB_BUILD_DATE", self.build_date.as_str()),
            ("ZDB_BUILD_TARGET", self.target.as_str()),
            ("ZDB_BUILD_PROFILE", self.profile.as_str()),
            ("ZDB_BUILD_RUSTC", self.rustc.as_str()),
        ];
        for (k, v) in vars {
            println!("cargo:rustc-env={k}={v}");
        }
    }
}

/// Rerun when HEAD moves, a commit/tag lands, or the index changes (the dirty
/// flag follows `git add`, not every unsaved edit). Only existing paths: a
/// missing one makes Cargo rerun the script on every build.
fn watch_git() {
    let mut paths = vec![
        "HEAD".to_string(),
        "index".into(),
        "packed-refs".into(),
        "refs/tags".into(),
    ];
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        paths.push(head_ref);
    }
    for p in paths {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", &p]) {
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// `YYYY-MM-DD HH:MM UTC` for a unix timestamp (Howard Hinnant's
/// days-to-civil; no date crate in a build script).
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

fn windows_resources(info: &BuildInfo) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let host_windows = env("HOST").contains("windows");
    // Forward slashes: rc treats `\` in a quoted string as an escape.
    let res = Path::new(&env("CARGO_MANIFEST_DIR"))
        .join("resources")
        .display()
        .to_string()
        .replace('\\', "/");
    let rc_path = Path::new(&env("OUT_DIR")).join("zdb.rc");
    std::fs::write(&rc_path, rc_source(info, &res, !host_windows)).unwrap();
    let compiled = embed_resource::compile(&rc_path, embed_resource::NONE);
    if host_windows {
        compiled.manifest_optional().unwrap();
    } else {
        compiled.manifest_required().unwrap();
    }
}

/// The `.rc` script. 1 = CREATEPROCESS_MANIFEST_RESOURCE_ID, 24 = RT_MANIFEST;
/// numeric forms (also in VERSIONINFO) avoid needing Windows headers under
/// windres.
fn rc_source(info: &BuildInfo, res: &str, manifest: bool) -> String {
    let base = info.version.split(['-', '+']).next().unwrap_or("");
    let mut nums = base.split('.').map(|n| n.parse::<u16>().unwrap_or(0));
    let (a, b, c) = (
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
    );
    let short = info.commit.get(..7).unwrap_or("unknown");
    let product = format!("{} ({short})", info.version).replace('"', "\"\"");
    let file_version = info.version.replace('"', "\"\"");
    // VS_FF_DEBUG
    let flags = if info.profile == "debug" {
        "0x1L"
    } else {
        "0x0L"
    };
    let mut rc = String::new();
    if manifest {
        rc.push_str(&format!("1 24 \"{res}/zdb.manifest\"\n"));
    }
    rc.push_str(&format!(
        r#"1 ICON "{res}/zdb.ico"

1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEFLAGSMASK 0x3fL
FILEFLAGS {flags}
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "zdb"
      VALUE "FileVersion", "{file_version}"
      VALUE "InternalName", "zdb"
      VALUE "LegalCopyright", "Licensed under {license}"
      VALUE "OriginalFilename", "zdb.exe"
      VALUE "ProductName", "zdb"
      VALUE "ProductVersion", "{product}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        license = env("CARGO_PKG_LICENSE"),
    ));
    rc
}
