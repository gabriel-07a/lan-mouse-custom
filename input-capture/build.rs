fn main() {
    // Build scripts run on the host: use Cargo's target metadata rather than
    // cfg!(unix)/cfg!(target_os), so cross-compiling (for example a Linux
    // host building the Windows binary) enables the correct backends.
    let unix = std::env::var_os("CARGO_CFG_UNIX").is_some();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let macos = target_os == "macos";
    let feature = |name: &str| std::env::var_os(format!("CARGO_FEATURE_{name}")).is_some();

    let libei = unix && !macos && feature("LIBEI");
    let layer_shell = unix && !macos && feature("LAYER_SHELL");
    let x11 = unix && !macos && feature("X11");

    println!("cargo::rustc-check-cfg=cfg(layer_shell)");
    println!("cargo::rustc-check-cfg=cfg(libei)");
    println!("cargo::rustc-check-cfg=cfg(x11)");

    if layer_shell {
        println!("cargo::rustc-cfg=layer_shell");
    }
    if libei {
        println!("cargo::rustc-cfg=libei");
    }
    if x11 {
        println!("cargo::rustc-cfg=x11");
    }
}
