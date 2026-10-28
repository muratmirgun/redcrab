//! Embeds the app icon and version information into the Windows executable,
//! so Explorer, the taskbar and the title bar show the crab.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/logo/icon.ico");

    // `cfg(windows)` is the host here; resources are only compiled when a
    // Windows host builds for Windows (rc.exe from the Windows SDK).
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("../../assets/logo/icon.ico")
            .set("ProductName", "redcrab")
            .set("FileDescription", "redcrab, a fast native Redis GUI")
            .set(
                "LegalCopyright",
                "Copyright (c) 2026 Murat Mirgün Ercan. MIT OR Apache-2.0.",
            );
        resource
            .compile()
            .expect("could not embed the Windows icon and version resources");
    }
}
