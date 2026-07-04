//! Embed the Windows application manifest (Common-Controls v6) + app icon.
//!
//! gpui calls `TaskDialogIndirect`, which needs comctl32 v6, selected by a
//! manifest declaring a dependency on Microsoft.Windows.Common-Controls 6.0.
//! gpui embeds such a manifest, but only when ITS build script runs on a Windows
//! host. So the manifest source differs by host:
//!   - cross-compiling from a non-Windows host: gpui skips it, so we embed our
//!     own manifest (+ icon) via `zdb.rc`.
//!   - native Windows build: gpui already provides the manifest, so we embed the
//!     ICON ONLY (`zdb-icon.rc`). A second manifest collides — MSVC's CVTRES
//!     rejects a duplicate MANIFEST resource (CVT1100 -> LNK1123). (`lld` merges
//!     duplicates, which is why the cross build never hit this.)

fn main() {
    // Re-embed (and rebuild) whenever the icon / manifest / resource scripts
    // change, so updating zdb.ico actually replaces the baked-in icon.
    println!("cargo:rerun-if-changed=resources/zdb.rc");
    println!("cargo:rerun-if-changed=resources/zdb-icon.rc");
    println!("cargo:rerun-if-changed=resources/zdb.ico");
    println!("cargo:rerun-if-changed=resources/zdb.manifest");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let host_windows = std::env::var("HOST")
        .map(|h| h.contains("windows"))
        .unwrap_or(false);
    if host_windows {
        // gpui embeds the manifest here; we add only the icon.
        embed_resource::compile("resources/zdb-icon.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    } else {
        // Cross build: gpui skipped the manifest, so we embed manifest + icon.
        embed_resource::compile("resources/zdb.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
