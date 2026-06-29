//! Embed the Windows application manifest (Common-Controls v6) into the binary.
//!
//! gpui calls `TaskDialogIndirect`, which only exists in comctl32 v6; that
//! version is selected by an app manifest declaring a dependency on
//! Microsoft.Windows.Common-Controls 6.0. gpui embeds such a manifest only when
//! its build script runs on Windows, so cross-compiling from another host skips
//! it and the resulting exe fails to start. We embed our own here.

fn main() {
    // Re-embed (and rebuild) whenever the icon / manifest / resource script change,
    // so updating zdb.ico actually replaces the icon baked into the exe.
    println!("cargo:rerun-if-changed=resources/zdb.rc");
    println!("cargo:rerun-if-changed=resources/zdb.ico");
    println!("cargo:rerun-if-changed=resources/zdb.manifest");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("resources/zdb.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
