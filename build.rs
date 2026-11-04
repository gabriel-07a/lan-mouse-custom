use shadow_rs::ShadowBuilder;

fn main() {
    embed_windows_icon();

    ShadowBuilder::builder()
        .deny_const(Default::default())
        .build()
        .expect("shadow build");

    let unix = cfg!(unix);
    let macos = cfg!(target_os = "macos");

    let layer_shell_capture = cfg!(feature = "layer_shell_capture");
    let libei_capture = cfg!(feature = "libei_capture");
    let x11_capture = cfg!(feature = "x11_capture");

    let libei_emulation = cfg!(feature = "libei_emulation");
    let x11_emulation = cfg!(feature = "x11_emulation");
    let wlroots_emulation = cfg!(feature = "wlroots_emulation");
    let rdp_emulation = cfg!(feature = "rdp_emulation");

    let layer_shell_capture = unix && !macos && layer_shell_capture;
    let libei_capture = unix && !macos && libei_capture;
    let x11_capture = unix && !macos && x11_capture;

    let libei_emulation = unix && !macos && libei_emulation;
    let rdp_emulation = unix && !macos && rdp_emulation;
    let wlroots_emulation = unix && !macos && wlroots_emulation;
    let x11_emulation = unix && !macos && x11_emulation;

    println!("cargo::rustc-check-cfg=cfg(layer_shell_capture)");
    println!("cargo::rustc-check-cfg=cfg(libei_capture)");
    println!("cargo::rustc-check-cfg=cfg(x11_capture)");

    println!("cargo::rustc-check-cfg=cfg(libei_emulation)");
    println!("cargo::rustc-check-cfg=cfg(rdp_emulation)");
    println!("cargo::rustc-check-cfg=cfg(wlroots_emulation)");
    println!("cargo::rustc-check-cfg=cfg(x11_emulation)");

    if layer_shell_capture {
        println!("cargo::rustc-cfg=layer_shell_capture");
    }
    if libei_capture {
        println!("cargo::rustc-cfg=libei_capture");
    }
    if x11_capture {
        println!("cargo::rustc-cfg=x11_capture");
    }

    if libei_emulation {
        println!("cargo::rustc-cfg=libei_emulation");
    }
    if rdp_emulation {
        println!("cargo::rustc-cfg=rdp_emulation");
    }
    if wlroots_emulation {
        println!("cargo::rustc-cfg=wlroots_emulation");
    }
    if x11_emulation {
        println!("cargo::rustc-cfg=x11_emulation");
    }
}

/// Embed `assets/icon.ico` into the executable so Explorer, the taskbar, the
/// tray and the shortcuts created by the installer all show the app icon.
fn embed_windows_icon() {
    // Build scripts run on the host, so check the *target* instead of cfg!.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo::rerun-if-changed=assets/icon.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        resource.set("ProductName", "Lan Mouse");
        resource.set("FileDescription", "Lan Mouse - mouse and keyboard sharing");
        if let Err(e) = resource.compile() {
            println!("cargo::warning=could not embed the Windows icon: {e}");
        }
    }
}
