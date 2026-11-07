//! Standalone launcher for the GPUI frontend prototype.
//!
//! The shipping `lan-mouse` binary will call `lan_mouse_gpui::run()` once the
//! GTK frontend is replaced; this binary is useful for UI development.

fn main() {
    if let Err(error) = lan_mouse_gpui::run(*b"devgpui\0") {
        eprintln!("GPUI frontend failed: {error}");
        std::process::exit(1);
    }
}
