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
use std::{
    future::Future,
    io,
    process::{self, Child},
};
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
    #[cfg(any(feature = "gtk", feature = "gpui"))]
    #[error("GPUI frontend failed: {0}")]
    GpuiFrontend(String),
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
                if let Some(ref mut service) = service {
                    #[cfg(unix)]
                    {
                        let pid = service.id() as libc::pid_t;
                        unsafe {
                            libc::kill(pid, libc::SIGINT);
                        }
                        service.wait()?;
                    }
                    service.kill()?;
                }
                res?;
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
                    lan_mouse_gtk::run(config::local_commit()).map_err(LanMouseError::from)
                };
                if let Some(ref mut service) = service {
                    #[cfg(unix)]
                    {
                        // on unix we give the service a chance to terminate gracefully
                        let pid = service.id() as libc::pid_t;
                        unsafe {
                            libc::kill(pid, libc::SIGINT);
                        }
                        service.wait()?;
                    }
                    service.kill()?;
                }
                res?;
            }
            #[cfg(not(feature = "gtk"))]
            {
                // run daemon if gtk is diabled
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

#[cfg(any(feature = "gtk", feature = "gpui"))]
fn run_gpui_frontend() -> Result<(), LanMouseError> {
    let frontend = gpui_frontend_path()?;
    log::info!("launching GPUI frontend: {frontend:?}");
    let status = std::process::Command::new(&frontend)
        .env(
            "LAN_MOUSE_LOCAL_COMMIT",
            String::from_utf8_lossy(&config::local_commit()).to_string(),
        )
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(LanMouseError::GpuiFrontend(format!(
            "{frontend:?} exited with {status}"
        )))
    }
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
                let _ = AttachConsole(ATTACH_PARENT_PROCESS);
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
