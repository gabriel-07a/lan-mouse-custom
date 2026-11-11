//! Standalone launcher for the GPUI frontend prototype.
//!
//! The shipping `lan-mouse` binary will call `lan_mouse_gpui::run()` once the
//! GTK frontend is replaced; this binary is useful for UI development.

// The launcher spawns this binary on Windows; without the GUI subsystem the
// frontend allocates a console window (the reported "terminal reappeared on
// Windows"). Debug builds keep the console for backtraces.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn local_commit() -> [u8; 8] {
    let mut commit = *b"devgpui\0";
    let Ok(text) = std::env::var("LAN_MOUSE_LOCAL_COMMIT") else {
        return commit;
    };
    for (slot, byte) in commit.iter_mut().zip(text.as_bytes()) {
        *slot = *byte;
    }
    commit
}

fn main() {
    match lan_mouse_gpui::run(local_commit()) {
        Ok(lan_mouse_gpui::FrontendExit::Quit) => {
            // Explicit quit: the Dashboard already asked the service to shut
            // down over IPC, and the launcher stops a daemon it spawned itself.
            // Use a dedicated code so a crash/minimize exit (0 or other) can
            // never be mistaken for a deliberate quit.
            std::process::exit(lan_mouse_ipc::FRONTEND_EXIT_QUIT);
        }
        Ok(lan_mouse_gpui::FrontendExit::WindowClosed) => {
            // The window was closed, not the application: exit with the code
            // that tells the launcher to leave the service (and therefore the
            // LAN connection) running. Starting Lan Mouse again reopens the
            // window against the still-running service.
            std::process::exit(lan_mouse_ipc::FRONTEND_EXIT_KEEP_SERVICE);
        }
        Err(error) => {
            eprintln!("GPUI frontend failed: {error}");
            std::process::exit(1);
        }
    }
}
