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
    // One manifest for the whole exe, provided here and nowhere else: gpui's
    // `windows-manifest` feature is off, but everything it declared still has
    // to be declared — per-monitor DPI awareness for the gpui surface and
    // Common Controls v6 so the three Win32 dialogs keep their modern look.
    resource.set_manifest(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
    <asmv3:application>
        <asmv3:windowsSettings>
            <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true</dpiAware>
            <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
        </asmv3:windowsSettings>
    </asmv3:application>
    <dependency>
        <dependentAssembly>
            <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
        </dependentAssembly>
    </dependency>
    <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
        <security>
            <requestedPrivileges>
                <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
            </requestedPrivileges>
        </security>
    </trustInfo>
</assembly>
"#,
    );
    if let Err(err) = resource.compile() {
        panic!("resource compiler could not embed the icon: {err}");
    }
}
