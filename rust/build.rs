//! Embeds assets/clipplus.ico and the application manifest into the exe as
//! Windows resources.
//!
//! `winresource` shells out to the platform resource compiler (rc.exe, part of
//! the MSVC toolchain any Windows rustc target already requires). This is what
//! gives the exe its file icon in Explorer, taskbar shortcuts and downloads —
//! the running process never reads the icon: the tray and the window classes
//! load the same file through `win::app_icon`, which works even if this step
//! were skipped.

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
    // The Common-Controls v6 dependency is what switches visual styles on for
    // the process. Without it comctl32 loads 5.82 and every BUTTON renders in
    // the classic light style, which turns the `DarkMode_Explorer` switch in
    // `win::dark_theme` into a no-op — the settings window then shows dark
    // fields under classic light buttons and checkboxes.
    resource.set_manifest(
        r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df"
        language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>"#,
    );
    if let Err(err) = resource.compile() {
        panic!("resource compiler could not embed the exe resources: {err}");
    }
}
