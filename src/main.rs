// Hide the console window when the executable is started from Explorer or a
// shortcut. Debug builds keep it so `println!`/backtraces stay visible.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use lan_mouse::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    doctor, emulation_test,
    service::{Service, ServiceError},
};
use lan_mouse_cli::CliError;

#[cfg(feature = "gtk")]
use lan_mouse_gtk::GtkError;
use lan_mouse_ipc::{IpcError, IpcListenerCreationError};
#[cfg(any(feature = "gtk", feature = "gpui"))]
use std::process::Child;
use std::{future::Future, io, process};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum LanMouseError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[cfg(feature = "gtk")]
    #[error(transparent)]
    Gtk(#[from] GtkError),
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error(transparent)]
    Doctor(#[from] doctor::DoctorError),
}

fn main() {
    // Must happen before the first window is created: the input backends rely
    // on physical pixel coordinates and per-monitor DPI queries, and GDK (when
    // the GUI starts) detects the awareness we set here instead of setting its
    // own. Daemon and CLI modes never open a GDK display, so without this they
    // would stay DPI-unaware and could not tell the displays' scale apart.
    enable_per_monitor_dpi_awareness();

    // A GUI executable has no console; when the binary is invoked from a
    // terminal, reattach to the parent's console so `--help`/`cli` output is
    // still visible.
    attach_parent_console();

    // init logging
    let env = Env::default().filter_or("LAN_MOUSE_LOG_LEVEL", "info");
    env_logger::init_from_env(env);

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

fn run() -> Result<(), LanMouseError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(lan_mouse_cli::run(cli_args))?,
            Command::Doctor(args) => run_async(doctor::run(config, args))?,
            Command::Daemon => {
                // if daemon is specified we run the service
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        },
        None => {
            //  otherwise start the service as a child process and
            //  run a frontend
            #[cfg(all(feature = "gpui", not(feature = "gtk")))]
            {
                let mut service = if lan_mouse_ipc::is_service_running() {
                    log::info!("daemon already running, connecting to existing instance");
                    None
                } else {
                    Some(start_service()?)
                };
                let res = run_gpui_frontend();
                match &res {
                    Ok(FrontendExit::KeepService) => {
                        // The window was closed, not the application: leave the
                        // daemon (and the LAN connection) running. The child is
                        // deliberately not waited for, so it keeps running after
                        // this process exits and the next launch reuses it.
                        log::info!("frontend window closed; keeping the Lan Mouse service running");
                    }
                    _ => {
                        // Explicit quit, launch failure or a killed frontend:
                        // the daemon this process started must be stopped and
                        // reaped before this process exits. Cleanup is
                        // infallible and bounded: a stuck daemon must never
                        // hang the window close.
                        shutdown_service(service.as_mut());
                    }
                }
                res.map(|_| ())?;
            }
            #[cfg(feature = "gtk")]
            {
                // Only spawn a new daemon if one isn't already running
                let mut service = if lan_mouse_ipc::is_service_running() {
                    log::info!("daemon already running, connecting to existing instance");
                    None
                } else {
                    Some(start_service()?)
                };
                let use_gpui =
                    std::env::var_os("LAN_MOUSE_FRONTEND").is_some_and(|value| value == "gpui");
                let res = if use_gpui {
                    log::info!("LAN_MOUSE_FRONTEND=gpui selected");
                    run_gpui_frontend()
                } else {
                    lan_mouse_gtk::run(config::local_commit())
                        .map_err(LanMouseError::from)
                        .map(|()| FrontendExit::Quit)
                };
                match &res {
                    Ok(FrontendExit::KeepService) => {
                        log::info!("frontend window closed; keeping the Lan Mouse service running");
                    }
                    _ => {
                        // Give the service a chance to terminate gracefully
                        // (SIGINT), but never block the window close on a stuck
                        // daemon and always reap the child.
                        shutdown_service(service.as_mut());
                    }
                }
                res.map(|_| ())?;
            }
            #[cfg(all(not(feature = "gtk"), not(feature = "gpui")))]
            {
                // Headless build: run the daemon directly. With `gpui` or
                // `gtk` enabled the frontend path above owns the lifecycle;
                // falling through to this arm after the GUI exits would leave
                // the process running as a daemon forever.
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        }
    }

    Ok(())
}

/// Why the frontend process exited, as decided by its exit code.
#[cfg(any(feature = "gtk", feature = "gpui"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrontendExit {
    /// Explicit quit: the service was asked to shut down and the daemon this
    /// process started must be stopped.
    Quit,
    /// The user closed the window: keep the service (and the LAN connection)
    /// running; the next launch reuses it.
    KeepService,
}

/// Map the frontend exit code to what the launcher must do with the daemon.
///
/// Only the explicit-quit code stops the service. Every other outcome —
/// including a zero/unknown code, a signal, or a crash while the window is
/// minimized — keeps the daemon and the LAN connection alive. This used to be
/// the opposite for unexpected codes: minimizing the Windows frontend ended
/// the frontend process, the launcher then killed its daemon child, and the
/// peer connection dropped.
#[cfg(any(feature = "gtk", feature = "gpui"))]
fn classify_frontend_exit(code: Option<i32>) -> FrontendExit {
    match code {
        Some(lan_mouse_ipc::FRONTEND_EXIT_QUIT) => FrontendExit::Quit,
        Some(lan_mouse_ipc::FRONTEND_EXIT_KEEP_SERVICE) => FrontendExit::KeepService,
        Some(other) => {
            log::warn!(
                "frontend exited with unexpected code {other}; keeping the service and connection running"
            );
            FrontendExit::KeepService
        }
        None => {
            log::warn!(
                "frontend terminated without an exit code; keeping the service and connection running"
            );
            FrontendExit::KeepService
        }
    }
}

#[cfg(any(feature = "gtk", feature = "gpui"))]
fn run_gpui_frontend() -> Result<FrontendExit, LanMouseError> {
    let frontend = gpui_frontend_path()?;
    log::info!("launching GPUI frontend: {frontend:?}");
    let status = std::process::Command::new(&frontend)
        .env(
            "LAN_MOUSE_LOCAL_COMMIT",
            String::from_utf8_lossy(&config::local_commit()).to_string(),
        )
        .status()?;
    Ok(classify_frontend_exit(status.code()))
}

#[cfg(any(feature = "gtk", feature = "gpui"))]
fn gpui_frontend_path() -> Result<std::path::PathBuf, std::io::Error> {
    if let Some(path) = std::env::var_os("LAN_MOUSE_GPUI") {
        let path = std::path::PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }
    let current = std::env::current_exe()?;
    let name = if cfg!(windows) {
        "lan-mouse-gpui.exe"
    } else {
        "lan-mouse-gpui"
    };
    let path = current.with_file_name(name);
    if path.exists() {
        Ok(path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "GPUI frontend binary not found at {path:?}; build lan-mouse-gpui or set LAN_MOUSE_GPUI"
            ),
        ))
    }
}

fn run_async<F, E>(f: F) -> Result<(), LanMouseError>
where
    F: Future<Output = Result<(), E>>,
    LanMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

#[cfg(any(feature = "gtk", feature = "gpui"))]
fn start_service() -> Result<Child, io::Error> {
    let mut command = process::Command::new(std::env::current_exe()?);
    command.args(std::env::args().skip(1)).arg("daemon");
    // The daemon must never flash a console window on Windows.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command.spawn()
}

/// Sets the process DPI awareness to per-monitor v2 on Windows.
///
/// GDK does the same when it opens a display, but `lan-mouse --daemon` and the
/// CLI commands never do. `GetDpiForMonitor` reports the real scale only for a
/// per-monitor aware process, and the input backends need it to convert
/// between physical pixels and the logical motion carried on the wire.
fn enable_per_monitor_dpi_awareness() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        // Fails with ERROR_ACCESS_DENIED when the process is already aware
        // (manifest, compatibility settings or a previous call); harmless.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    }
}

/// Reattach to the parent console when started from a shell.
///
/// The release build is linked as a GUI subsystem executable, which has no
/// console of its own; without this, running `lan-mouse cli` from a terminal
/// would print nothing.
fn attach_parent_console() {
    #[cfg(windows)]
    {
        if std::env::args_os().len() > 1 {
            use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
            unsafe {
                // Fails when there is no parent console (Explorer, service,
                // redirected stdio); there is nothing to restore then.
                if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
                    restore_console_std_handles();
                }
            }
        }
    }
}

/// Point the process standard handles at the console we just attached to.
///
/// `AttachConsole` attaches the Win32 console, but this GUI-subsystem process
/// can still have absent std handles, so writes through `std::io::stdout`
/// (and the CLI's `println!`) would be discarded. Reopening `CONOUT$` and
/// installing it with `SetStdHandle` makes `lan-mouse doctor --json` and
/// `lan-mouse cli` work when run from a terminal or a pipeline.
///
/// A handle that is already valid is left untouched: when the process was
/// started with its stdout/stderr redirected (`> file`, a pipe,
/// `-RedirectStandardOutput`), `AttachConsole` still succeeds because the
/// parent has a console, and overwriting those handles would throw the
/// redirection away (the file would stay empty and the output would go to the
/// console instead).
#[cfg(windows)]
fn restore_console_std_handles() {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    unsafe fn handle_is_absent(handle: windows::core::Result<HANDLE>) -> bool {
        match handle {
            Ok(handle) => handle.is_invalid() || handle.0.is_null(),
            Err(_) => true,
        }
    }

    // The opened device files must stay alive for the whole process lifetime:
    // they become the std handles.
    let stdout_absent = unsafe { handle_is_absent(GetStdHandle(STD_OUTPUT_HANDLE)) };
    let stderr_absent = unsafe { handle_is_absent(GetStdHandle(STD_ERROR_HANDLE)) };
    if stdout_absent || stderr_absent {
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            let handle = HANDLE(file.as_raw_handle() as *mut _);
            std::mem::forget(file);
            unsafe {
                if stdout_absent {
                    let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
                }
                if stderr_absent {
                    let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
                }
            }
        }
    }
    if unsafe { handle_is_absent(GetStdHandle(STD_INPUT_HANDLE)) } {
        if let Ok(file) = std::fs::OpenOptions::new().read(true).open("CONIN$") {
            let handle = HANDLE(file.as_raw_handle() as *mut _);
            std::mem::forget(file);
            unsafe {
                let _ = SetStdHandle(STD_INPUT_HANDLE, handle);
            }
        }
    }
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}

/// How long the frontend waits for the daemon to exit after SIGINT before
/// killing it.
#[cfg(any(feature = "gtk", feature = "gpui"))]
const SERVICE_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Stop the daemon child this process started (if any) without ever blocking
/// the frontend's exit.
///
/// A window close must not hang on a daemon that ignores SIGINT, and the child
/// must be reaped so it does not linger as an orphan (and so its pid cannot be
/// recycled while this process still holds it).
#[cfg(any(feature = "gtk", feature = "gpui"))]
fn shutdown_service(service: Option<&mut Child>) {
    let Some(service) = service else { return };
    shutdown_service_with_grace(service, SERVICE_SHUTDOWN_GRACE);
}

#[cfg(all(unix, any(feature = "gtk", feature = "gpui")))]
fn shutdown_service_with_grace(service: &mut Child, grace: std::time::Duration) {
    use std::time::Instant;

    // The daemon may have exited on its own already (e.g. it failed to bind
    // because another instance owns the IPC socket, or the frontend failed to
    // launch). Never signal a pid we have not reaped yet: a dead child's pid
    // may already belong to an unrelated process.
    match service.try_wait() {
        Ok(Some(_)) | Err(_) => return,
        Ok(None) => {}
    }

    // SAFETY: kill(2) only takes a pid and a signal. The pid is our own child
    // and has not been reaped, so it cannot have been recycled.
    let pid = service.id() as libc::pid_t;
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }

    let start = Instant::now();
    while start.elapsed() < grace {
        match service.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(e) => {
                log::warn!("could not check daemon state: {e}");
                break;
            }
        }
    }
    log::warn!("daemon did not exit within {grace:?}; killing it");
    stop_child(service);
}

#[cfg(all(not(unix), any(feature = "gtk", feature = "gpui")))]
fn shutdown_service_with_grace(service: &mut Child, _grace: std::time::Duration) {
    // A Windows console-less child cannot receive our Ctrl-C; terminate it.
    match service.try_wait() {
        Ok(Some(_)) | Err(_) => return,
        Ok(None) => {}
    }
    stop_child(service);
}

#[cfg(any(feature = "gtk", feature = "gpui"))]
fn stop_child(service: &mut Child) {
    if let Err(e) = service.kill() {
        log::warn!("could not kill daemon: {e}");
    }
    if let Err(e) = service.wait() {
        log::warn!("could not reap daemon: {e}");
    }
}

#[cfg(all(test, any(feature = "gtk", feature = "gpui")))]
mod frontend_exit_tests {
    use super::*;

    #[test]
    fn a_closed_window_keeps_the_service_running() {
        assert_eq!(
            classify_frontend_exit(Some(lan_mouse_ipc::FRONTEND_EXIT_KEEP_SERVICE)),
            FrontendExit::KeepService,
            "the window-close exit code must leave the daemon alone"
        );
    }

    #[test]
    fn an_explicit_quit_stops_the_service() {
        assert_eq!(
            classify_frontend_exit(Some(lan_mouse_ipc::FRONTEND_EXIT_QUIT)),
            FrontendExit::Quit,
            "only the explicit-quit code may stop the daemon"
        );
    }

    #[test]
    fn a_minimized_or_killed_frontend_keeps_the_service_running() {
        for code in [None, Some(0), Some(1), Some(2)] {
            assert_eq!(
                classify_frontend_exit(code),
                FrontendExit::KeepService,
                "frontend exit {code:?} must not cut the LAN connection"
            );
        }
    }
}

#[cfg(all(test, unix, any(feature = "gtk", feature = "gpui")))]
mod shutdown_tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn spawn_sleep() -> Child {
        Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep")
    }

    #[test]
    fn already_exited_child_is_reaped_without_signalling() {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(5));
        }
        shutdown_service(Some(&mut child));
        assert!(
            matches!(child.try_wait(), Ok(Some(_))),
            "the child must be reaped"
        );
    }

    #[test]
    fn cooperative_daemon_is_stopped_quickly() {
        let mut child = spawn_sleep();
        let start = Instant::now();
        shutdown_service(Some(&mut child));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "cooperative child took {:?}",
            start.elapsed()
        );
        assert!(matches!(child.try_wait(), Ok(Some(_))));
    }

    #[test]
    fn stubborn_daemon_is_killed_after_the_grace_period() {
        // SIGINT ignored: the bounded wait must give up and SIGKILL it.
        let mut child = Command::new("sh")
            .args(["-c", "trap '' INT; exec sleep 30"])
            .spawn()
            .expect("spawn sh");
        // Let the shell install the ignored-SIGINT disposition before we
        // signal it; otherwise the test races the exec and would pass for the
        // wrong reason.
        std::thread::sleep(Duration::from_millis(200));
        let grace = Duration::from_millis(150);
        let start = Instant::now();
        shutdown_service_with_grace(&mut child, grace);
        assert!(
            start.elapsed() >= grace,
            "should have waited the grace period, took {:?}",
            start.elapsed()
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "took too long: {:?}",
            start.elapsed()
        );
        assert!(matches!(child.try_wait(), Ok(Some(_))));
    }
}
