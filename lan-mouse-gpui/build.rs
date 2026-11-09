//! Embeds the Lan Mouse icon (and version metadata) into the Windows
//! frontend executable.
//!
//! The frontend owns the visible window: GPUI loads the window/taskbar icon
//! from resource id 1 of *this* executable. Without it, a shortcut pinned to
//! the frontend shows the generic Windows icon even though the launcher
//! (lan-mouse.exe) has one.

fn main() {
    embed_windows_icon();
}

fn embed_windows_icon() {
    // Build scripts run on the host, so check the *target* instead of cfg!.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo::rerun-if-changed=../assets/icon.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("../assets/icon.ico");
        resource.set("ProductName", "Lan Mouse");
        resource.set("FileDescription", "Lan Mouse - mouse and keyboard sharing");
        if let Err(e) = resource.compile() {
            println!("cargo::warning=could not embed the Windows icon: {e}");
        }
    }
}
