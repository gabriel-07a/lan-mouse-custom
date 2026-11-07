fn main() {
    let unix = std::env::var_os("CARGO_CFG_UNIX").is_some();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let macos = target_os == "macos";
    let feature = |name: &str| std::env::var_os(format!("CARGO_FEATURE_{name}")).is_some();

    let libei = unix && !macos && feature("LIBEI");
    let wlroots = unix && !macos && feature("WLROOTS");
    let x11 = unix && !macos && feature("X11");
    let rdp = unix && !macos && feature("REMOTE_DESKTOP_PORTAL");

    println!("cargo::rustc-check-cfg=cfg(wlroots)");
    println!("cargo::rustc-check-cfg=cfg(libei)");
    println!("cargo::rustc-check-cfg=cfg(x11)");
    println!("cargo::rustc-check-cfg=cfg(rdp)");

    if libei {
        println!("cargo::rustc-cfg=libei");
    }
    if x11 {
        println!("cargo::rustc-cfg=x11");
    }
    if wlroots {
        println!("cargo::rustc-cfg=wlroots");
    }
    if rdp {
        println!("cargo::rustc-cfg=rdp");
    }
}
