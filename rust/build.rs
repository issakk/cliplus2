//! Embeds assets/clipplus.ico into the exe as a Windows resource.
//!
//! `winresource` shells out to the platform resource compiler (rc.exe, part of
//! the MSVC toolchain any Windows rustc target already requires). This is what
//! gives the exe its file icon in Explorer, taskbar shortcuts and downloads —
//! the running process never reads it: the tray and the window classes load
//! the same file through `win::app_icon`, which works even if this step were
//! skipped.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let icon = std::path::Path::new(&manifest)
        .join("..")
        .join("assets")
        .join("clipplus.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon.to_str().expect("icon path is not valid Unicode"));
    if let Err(err) = resource.compile() {
        panic!("resource compiler could not embed the icon: {err}");
    }
}
