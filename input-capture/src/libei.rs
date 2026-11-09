use ashpd::{
    desktop::{
        PersistMode, Session,
        input_capture::{
            Activated, ActivatedBarrier, Barrier, BarrierID, Capabilities, CreateSession2Options,
            CreateSessionOptions, InputCapture, Region, ReleaseOptions, StartOptions, Zones,
        },
    },
    enumflags2::BitFlags,
};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use reis::{
    ei::{self, handshake::ContextType},
    event::{Connection, DeviceCapability, EiEvent},
    tokio::EiConvertEventStream,
};
use std::{
    cell::Cell,
    collections::HashMap,
    env, fs, io,
    num::NonZeroU32,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    sync::{Arc, LazyLock},
    task::{Context, Poll},
};
use tokio::{
    sync::{
        Notify,
        mpsc::{self, Receiver, Sender},
    },
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;

use input_event::Event;

use crate::CaptureEvent;

use super::{
    Capture as LanMouseInputCapture, Position,
    error::{CaptureError, LibeiCaptureCreationError},
};

/// Name of the InputCapture restore token file.
///
/// The token is the user's persisted consent. The portal invalidates it after
/// every successful `Start` and returns a replacement, so it is rewritten on
/// each start.
const CAPTURE_TOKEN_FILE: &str = "input-capture.token";

/// Capabilities requested from the input capture portal.
fn requested_capabilities() -> BitFlags<Capabilities> {
    Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen
}

/// Directory holding portal tokens: `$XDG_STATE_HOME/lan-mouse`, falling back
/// to `~/.local/state/lan-mouse`.
///
/// State, not cache: cache directories are routinely purged by cleanup tools
/// and losing the token makes the compositor ask for permission again.
fn token_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(base.join("lan-mouse"))
}

/// Token location used by builds that stored consent in the cache directory.
fn legacy_token_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("lan-mouse"))
}

fn read_token_file(path: &Path) -> Option<String> {
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Read the saved consent token, migrating a token left in the legacy cache
/// directory so an already-granted consent survives the move.
fn read_token_from(state_dir: Option<&Path>, legacy_dir: Option<&Path>) -> Option<String> {
    let state_dir = state_dir?;
    let path = state_dir.join(CAPTURE_TOKEN_FILE);
    if let Some(token) = read_token_file(&path) {
        return Some(token);
    }
    let legacy_path = legacy_dir?.join(CAPTURE_TOKEN_FILE);
    let token = read_token_file(&legacy_path)?;
    log::info!(
        "migrating input capture restore token from {} to {}",
        legacy_path.display(),
        path.display()
    );
    if let Err(e) = write_token_to(state_dir, CAPTURE_TOKEN_FILE, &token) {
        log::warn!("could not migrate the input capture restore token: {e}");
    } else if let Err(e) = fs::remove_file(&legacy_path) {
        // Do not keep a world-readable copy of a bearer capability around.
        log::debug!("could not remove the legacy restore token: {e}");
    }
    Some(token)
}

fn read_token() -> Option<String> {
    read_token_from(token_dir().as_deref(), legacy_token_dir().as_deref())
}

/// Save the consent token atomically with owner-only permissions.
///
/// Tokens are bearer capabilities: whoever can read one can restore the
/// portal session without a permission dialog, hence 0600 for the file and
/// 0700 for the directory.
fn write_token(token: &str) -> io::Result<()> {
    let dir = token_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "neither XDG_STATE_HOME nor HOME is set",
        )
    })?;
    write_token_to(&dir, CAPTURE_TOKEN_FILE, token)
}

fn write_token_to(dir: &Path, name: &str, token: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

    let path = dir.join(name);
    // Write next to the destination and rename: a crash mid-write must not
    // leave a truncated token that would then be sent back to the portal.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    // The mode above only applies when the file is created; tighten an
    // existing file as well.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/* there is a bug in xdg-remote-desktop-portal-gnome / mutter that
 * prevents receiving further events after a session has been disabled once.
 * Therefore the session needs to be recreated when the barriers are updated */

/* mutter also kills the session whenever ei devices come and go, so there the
 * whole session has to be torn down and recreated on every device change.
 * Elsewhere that is pure overhead: each restart costs a CreateSession +
 * ConnectToEIS round trip, and compositors that keep per-session state around
 * (hyprland leaks a keymap fd per eis session, see hyprwm/Hyprland) can be
 * driven out of file descriptors by the churn.
 * Set LM_RESTART_SESSION_ON_DEVICE_CHANGE=1/0 to override the default. */
static RESTART_SESSION_ON_DEVICE_CHANGE: LazyLock<bool> =
    LazyLock::new(restart_session_on_device_change);

fn restart_session_on_device_change() -> bool {
    match env::var("LM_RESTART_SESSION_ON_DEVICE_CHANGE").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|desktops| desktops.to_uppercase().split(':').any(|d| d == "GNOME")),
    }
}

/// events that necessitate restarting the capture session
#[derive(Clone, Copy, Debug)]
enum LibeiNotifyEvent {
    Create(Position),
    Destroy(Position),
}

pub struct LibeiInputCapture {
    // The portal handle is owned by the capture task (shared as an `Arc`), so
    // the backend itself does not need to keep a raw pointer to it alive.
    capture_task: JoinHandle<Result<(), CaptureError>>,
    event_rx: Receiver<(Position, CaptureEvent)>,
    notify_capture: Sender<LibeiNotifyEvent>,
    notify_release: Arc<Notify>,
    cancellation_token: CancellationToken,
    terminated: bool,
    /// the capture task already reported its final state; polling a finished
    /// JoinHandle again would panic
    finished: bool,
}

/// returns (start pos, end pos), inclusive
fn pos_to_barrier(r: &Region, pos: Position) -> (i32, i32, i32, i32) {
    let (x, y) = (r.x_offset(), r.y_offset());
    let (w, h) = (r.width() as i32, r.height() as i32);
    match pos {
        Position::Left => (x, y, x, y + h - 1),
        Position::Right => (x + w, y, x + w, y + h - 1),
        Position::Top => (x, y, x + w - 1, y),
        Position::Bottom => (x, y + h, x + w - 1, y + h),
    }
}

/// Ashpd does not expose fields
#[derive(Clone, Copy, Debug)]
struct ICBarrier {
    barrier_id: BarrierID,
    position: (i32, i32, i32, i32),
}

impl ICBarrier {
    fn new(barrier_id: BarrierID, position: (i32, i32, i32, i32)) -> Self {
        Self {
            barrier_id,
            position,
        }
    }
}

impl From<ICBarrier> for Barrier {
    fn from(barrier: ICBarrier) -> Self {
        Barrier::new(barrier.barrier_id, barrier.position)
    }
}

fn select_barriers(
    zones: &Zones,
    clients: &[Position],
    next_barrier_id: &mut NonZeroU32,
) -> (Vec<ICBarrier>, HashMap<BarrierID, Position>) {
    let mut pos_for_barrier = HashMap::new();
    let mut barriers: Vec<ICBarrier> = vec![];

    for pos in clients {
        let mut client_barriers = zones
            .regions()
            .iter()
            .map(|r| {
                let id = *next_barrier_id;
                *next_barrier_id = next_barrier_id
                    .checked_add(1)
                    .expect("barrier id out of range");
                let position = pos_to_barrier(r, *pos);
                pos_for_barrier.insert(id, *pos);
                ICBarrier::new(id, position)
            })
            .collect();
        barriers.append(&mut client_barriers);
    }
    (barriers, pos_for_barrier)
}

async fn update_barriers(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    active_clients: &[Position],
    next_barrier_id: &mut NonZeroU32,
) -> Result<(Vec<ICBarrier>, HashMap<BarrierID, Position>), ashpd::Error> {
    let zones = input_capture
        .zones(session, Default::default())
        .await?
        .response()?;
    log::debug!("zones: {zones:?}");

    let (barriers, id_map) = select_barriers(&zones, active_clients, next_barrier_id);
    log::debug!("barriers: {barriers:?}");
    log::debug!("client for barrier id: {id_map:?}");

    let ashpd_barriers: Vec<Barrier> = barriers.iter().copied().map(|b| b.into()).collect();
    let response = input_capture
        .set_pointer_barriers(
            session,
            &ashpd_barriers,
            zones.zone_set(),
            Default::default(),
        )
        .await?;
    let response = response.response()?;
    log::debug!("{response:?}");
    Ok((barriers, id_map))
}

/// Create (and, on portal v2, start) an input capture session.
///
/// The version 2 flow (`CreateSession2` + `Start`) is preferred because only
/// that flow carries `persist_mode`/`restore_token`: the token persists the
/// user's consent across session restarts instead of prompting again. Portals
/// that only implement version 1 (e.g. the GNOME backend as of GNOME 50)
/// reject the v2 calls with `RequiresVersion`; the legacy `CreateSession` is
/// started implicitly and cannot persist anything.
async fn create_session(
    input_capture: &InputCapture,
) -> std::result::Result<(Session<InputCapture>, BitFlags<Capabilities>), ashpd::Error> {
    log::debug!("creating input capture session");
    match input_capture
        .create_session2(CreateSession2Options::default())
        .await
    {
        Ok(session) => {
            let restore_token = read_token();
            log::debug!(
                "starting input capture session (restore token {})",
                if restore_token.is_some() {
                    "present"
                } else {
                    "missing"
                }
            );
            let options = StartOptions::default()
                .set_capabilities(requested_capabilities())
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(restore_token);
            let response = input_capture
                .start(&session, None, options)
                .await?
                .response()?;
            // The token is single-use: the portal invalidated the one that was
            // just used and returned a replacement, which has to be saved
            // again or the next start prompts.
            if let Some(token) = response.restore_token() {
                if let Err(e) = write_token(token) {
                    log::warn!("could not save the input capture restore token: {e}");
                }
            }
            Ok((session, response.capabilities()))
        }
        Err(ashpd::Error::RequiresVersion(required, current)) => {
            // The compositor implements the original InputCapture interface,
            // which has no way to remember consent: expect a dialog whenever
            // a capture session is created.
            log::debug!(
                "input capture portal v{current} < v{required}: restore tokens are \
                 unsupported, the compositor will ask for permission again whenever \
                 a capture session is created"
            );
            input_capture
                .create_session(
                    None,
                    CreateSessionOptions::default().set_capabilities(requested_capabilities()),
                )
                .await
        }
        Err(e) => Err(e),
    }
}

async fn connect_to_eis(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
) -> Result<(ei::Context, Connection, EiConvertEventStream), CaptureError> {
    log::debug!("connect_to_eis");
    let fd = input_capture
        .connect_to_eis(session, Default::default())
        .await?;

    // create unix stream from fd
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;

    // create ei context
    let context = ei::Context::new(stream)?;
    let (conn, event_stream) = context
        .handshake_tokio("de.feschber.LanMouse", ContextType::Receiver)
        .await?;

    Ok((context, conn, event_stream))
}

async fn libei_event_handler(
    mut ei_event_stream: EiConvertEventStream,
    context: ei::Context,
    event_tx: Sender<(Position, CaptureEvent)>,
    release_session: Arc<Notify>,
    current_pos: Rc<Cell<Option<Position>>>,
) -> Result<(), CaptureError> {
    loop {
        let ei_event = ei_event_stream
            .next()
            .await
            .ok_or(CaptureError::EndOfStream)??;
        log::trace!("from ei: {ei_event:?}");
        let client = current_pos.get();
        handle_ei_event(ei_event, client, &context, &event_tx, &release_session).await?;
    }
}

impl LibeiInputCapture {
    pub async fn new() -> std::result::Result<Self, LibeiCaptureCreationError> {
        let input_capture = Arc::new(InputCapture::new().await?);

        // No session is created here on purpose: creating one makes the
        // compositor show its input-capture permission dialog. Portals that
        // cannot persist consent (InputCapture v1, e.g. GNOME) would then ask
        // on every lan-mouse start, even when no client is configured for
        // outgoing input. `do_capture` creates the session lazily once a
        // capture is actually requested.

        // One slot made the reader task suspend on every single event; a
        // small queue lets it batch while still bounding memory.
        let (event_tx, event_rx) = mpsc::channel(256);
        let (notify_capture, notify_rx) = mpsc::channel(1);
        let notify_release = Arc::new(Notify::new());

        let cancellation_token = CancellationToken::new();

        let capture = do_capture(
            Arc::clone(&input_capture),
            notify_rx,
            notify_release.clone(),
            event_tx,
            cancellation_token.clone(),
        );
        let capture_task = tokio::task::spawn_local(capture);

        let producer = Self {
            event_rx,
            capture_task,
            notify_capture,
            notify_release,
            cancellation_token,
            terminated: false,
            finished: false,
        };

        Ok(producer)
    }
}

async fn do_capture(
    input_capture: Arc<InputCapture>,
    mut capture_event: Receiver<LibeiNotifyEvent>,
    notify_release: Arc<Notify>,
    event_tx: Sender<(Position, CaptureEvent)>,
    cancellation_token: CancellationToken,
) -> Result<(), CaptureError> {
    let input_capture: &InputCapture = &input_capture;
    let mut active_clients: Vec<Position> = vec![];
    let mut next_barrier_id = NonZeroU32::new(1).expect("id must be non-zero");

    // Portal calls can block indefinitely (a permission dialog the user never
    // answers). Shutdown must not wait for them, so both the initial signal
    // subscription and the session creation below observe the token.
    let mut zones_changed = tokio::select! {
        zones = input_capture.receive_zones_changed() => zones?,
        _ = cancellation_token.cancelled() => return Ok(()),
    };

    loop {
        // do capture session
        let cancel_session = CancellationToken::new();
        let cancel_update = CancellationToken::new();

        let mut capture_event_occured: Option<LibeiNotifyEvent> = None;
        let mut zones_have_changed = false;

        // kill session if clients need to be updated
        let handle_session_update_request = async {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    log::debug!("cancelled")
                }, /* exit requested */
                _ = cancel_update.cancelled() => {
                    log::debug!("update task cancelled");
                }, /* session exited */
                _ = zones_changed.next() => {
                    log::debug!("zones changed!");
                    zones_have_changed = true
                }, /* zones have changed */
                e = capture_event.recv() => if let Some(e) = e { /* clients changed */
                    log::debug!("capture event: {e:?}");
                    capture_event_occured.replace(e);
                },
            }
            // kill session (might already be dead!)
            log::debug!("=> cancelling session");
            cancel_session.cancel();
        };

        if !active_clients.is_empty() {
            // Session creation is deferred to this point: a machine that only
            // receives input never triggers the compositor's capture dialog.
            let mut session = tokio::select! {
                session = create_session(input_capture) => session?.0,
                _ = cancellation_token.cancelled() => {
                    log::debug!("cancelled while creating the capture session");
                    break Ok(());
                }
            };

            let capture_session = do_capture_session(
                input_capture,
                &mut session,
                &event_tx,
                &active_clients,
                &mut next_barrier_id,
                &notify_release,
                (cancel_session.clone(), cancel_update.clone()),
            );

            let (capture_result, ()) = tokio::join!(capture_session, handle_session_update_request);
            log::debug!("capture session + session_update task done!");

            // disable capture
            log::debug!("disabling input capture");
            if let Err(e) = input_capture.disable(&session, Default::default()).await {
                log::warn!("input_capture.disable(&session) {e}");
            }
            if let Err(e) = session.close().await {
                log::warn!("session.close(): {e}");
            }

            // propagate error from capture session
            capture_result?;
        } else {
            handle_session_update_request.await;
        }

        // update clients if requested
        if let Some(event) = capture_event_occured.take() {
            match event {
                LibeiNotifyEvent::Create(p) => active_clients.push(p),
                LibeiNotifyEvent::Destroy(p) => active_clients.retain(|&pos| pos != p),
            }
        }

        // break
        if cancellation_token.is_cancelled() {
            break Ok(());
        }
    }
}

async fn do_capture_session(
    input_capture: &InputCapture,
    session: &mut Session<InputCapture>,
    event_tx: &Sender<(Position, CaptureEvent)>,
    active_clients: &[Position],
    next_barrier_id: &mut NonZeroU32,
    notify_release: &Notify,
    cancel: (CancellationToken, CancellationToken),
) -> Result<(), CaptureError> {
    let (cancel_session, cancel_update) = cancel;
    // current client
    let current_pos = Rc::new(Cell::new(None));

    // connect to eis server
    let (context, _conn, ei_event_stream) = connect_to_eis(input_capture, session).await?;

    // set barriers
    let (barriers, pos_for_barrier_id) =
        update_barriers(input_capture, session, active_clients, next_barrier_id).await?;

    log::debug!("enabling session");
    input_capture.enable(session, Default::default()).await?;

    // cancellation token to release session
    let release_session = Arc::new(Notify::new());

    // async event task
    let cancel_ei_handler = CancellationToken::new();
    let event_chan = event_tx.clone();
    let pos = current_pos.clone();
    let cancel_session_clone = cancel_session.clone();
    let release_session_clone = release_session.clone();
    let cancel_ei_handler_clone = cancel_ei_handler.clone();
    let ei_task = async move {
        tokio::select! {
            r = libei_event_handler(
                ei_event_stream,
                context,
                event_chan,
                release_session_clone,
                pos,
            ) => {
                log::debug!("libei exited: {r:?} cancelling session task");
                cancel_session_clone.cancel();
            }
            _ = cancel_ei_handler_clone.cancelled() => {},
        }
        Ok::<(), CaptureError>(())
    };

    let capture_session_task = async {
        // receiver for activation tokens
        let mut activated = input_capture.receive_activated().await?;
        let mut ei_devices_changed = false;
        loop {
            tokio::select! {
                activated = activated.next() => {
                    let activated = activated.ok_or(CaptureError::ActivationClosed)?;
                    log::debug!("activated: {activated:?}");

                    // The cursor position is optional in the protocol (KDE does
                    // not report barrier ids, some compositors do not report the
                    // position). An activation we cannot map to a client is
                    // dropped instead of panicking inside the capture task.
                    let barrier_id = match activated.barrier_id() {
                        Some(ActivatedBarrier::Barrier(id)) => id,
                        // workaround for KDE plasma not reporting barrier ids
                        Some(ActivatedBarrier::UnknownBarrier) | None => {
                            let Some(cursor_position) = activated.cursor_position() else {
                                log::warn!(
                                    "ignoring activation without a barrier id or cursor position"
                                );
                                continue;
                            };
                            let Some(id) = find_corresponding_client(&barriers, cursor_position)
                            else {
                                log::warn!("activation does not match any registered barrier");
                                continue;
                            };
                            id
                        }
                    };

                    // find client corresponding to barrier
                    let pos = match pos_for_barrier_id.get(&barrier_id) {
                        Some(id) => *id,
                        None => {
                            log::warn!("INVALID BARRIER ID: Id {barrier_id} does not exist!");
                            let Some(cursor_position) = activated.cursor_position() else {
                                log::warn!(
                                    "ignoring activation with an unknown barrier id and no cursor position"
                                );
                                continue;
                            };
                            let Some(id) = find_corresponding_client(&barriers, cursor_position)
                            else {
                                log::warn!("activation does not match any registered barrier");
                                continue;
                            };
                            match pos_for_barrier_id.get(&id) {
                                Some(pos) => *pos,
                                None => {
                                    log::warn!("barrier {id} has no registered client");
                                    continue;
                                }
                            }
                        },
                    };
                    current_pos.replace(Some(pos));

                    // client entered => send event
                    if event_tx
                        .send((pos, CaptureEvent::Begin))
                        .await
                        .is_err()
                    {
                        return Err(CaptureError::EndOfStream);
                    }

                    tokio::select! {
                        _ = notify_release.notified() => { /* capture release */
                            log::debug!("release session requested");
                        },
                        _ = release_session.notified() => { /* release session */
                            log::debug!("ei devices changed");
                            ei_devices_changed = true;
                        },
                        _ = cancel_session.cancelled() => { /* kill session notify */
                            log::debug!("session cancel requested");
                            break
                        },
                    }

                    release_capture(input_capture, session, activated, pos).await?;
                    // No client is being driven anymore: input events that
                    // arrive before the next activation must not be forwarded
                    // with the previous client's position.
                    current_pos.replace(None);
                }
                _ = notify_release.notified() => { /* capture release -> we are not capturing anyway, so ignore */
                    log::debug!("release session requested");
                },
                _ = release_session.notified() => { /* release session */
                    log::debug!("ei devices changed");
                    ei_devices_changed = true;
                },
                _ = cancel_session.cancelled() => { /* kill session notify */
                    log::debug!("session cancel requested");
                    break
                },
            }
            if ei_devices_changed {
                /* for whatever reason, GNOME seems to kill the session
                 * as soon as devices are added or removed, so we need
                 * to cancel */
                break;
            }
        }
        // cancel libei task
        log::debug!("session exited: killing libei task");
        cancel_ei_handler.cancel();
        Ok::<(), CaptureError>(())
    };

    let (a, b) = tokio::join!(ei_task, capture_session_task);

    cancel_update.cancel();

    log::debug!("both session and ei task finished!");
    a?;
    b?;

    Ok(())
}

async fn release_capture(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    activated: Activated,
    current_pos: Position,
) -> Result<(), CaptureError> {
    if let Some(activation_id) = activated.activation_id() {
        log::debug!("releasing input capture {activation_id}");
    }
    let mut release_options =
        ReleaseOptions::default().set_activation_id(activated.activation_id());
    // The cursor position is optional in the protocol. Without it the portal
    // releases the capture at the current position (the cursor may re-enter
    // once, which the next activation/release cycle handles) instead of
    // aborting the capture task with a panic.
    match activated.cursor_position() {
        Some((x, y)) => {
            log::debug!("client entered @ ({x}, {y})");
            let (dx, dy) = match current_pos {
                // offset cursor position to not enter again immediately
                Position::Left => (1., 0.),
                Position::Right => (-1., 0.),
                Position::Top => (0., 1.),
                Position::Bottom => (0., -1.),
            };
            // release 1px to the right of the entered zone
            release_options =
                release_options.set_cursor_position(Some((x as f64 + dx, y as f64 + dy)));
        }
        None => log::warn!("compositor did not report a cursor position for the capture release"),
    }
    input_capture.release(session, release_options).await?;
    Ok(())
}

fn find_corresponding_client(barriers: &[ICBarrier], pos: (f32, f32)) -> Option<BarrierID> {
    barriers
        .iter()
        .copied()
        .min_by_key(|b| {
            let (x1, y1, x2, y2) = b.position;
            let (x1, y1, x2, y2) = (x1 as f32, y1 as f32, x2 as f32, y2 as f32);
            distance_to_line(((x1, y1), (x2, y2)), pos) as i32
        })
        .map(|b| b.barrier_id)
}

fn distance_to_line(line: ((f32, f32), (f32, f32)), p: (f32, f32)) -> f32 {
    let ((x1, y1), (x2, y2)) = line;
    let (x0, y0) = p;
    /*
     * we use the fact that for the triangle spanned by the line and p,
     * the height of the triangle is the desired distance and can be calculated by
     * h = 2A / b with b being the line_length and
     */
    let double_triangle_area = ((y2 - y1) * x0 - (x2 - x1) * y0 + x2 * y1 - y2 * x1).abs();
    let line_length = ((y2 - y1).powf(2.0) + (x2 - x1).powf(2.0)).sqrt();
    if line_length == 0.0 {
        // Degenerate barrier (a portal is free to report empty regions): fall
        // back to the distance from the point instead of dividing by zero.
        return ((x0 - x1).powf(2.0) + (y0 - y1).powf(2.0)).sqrt();
    }
    let distance = double_triangle_area / line_length;
    log::debug!("distance to line({line:?}, {p:?}) = {distance}");
    distance
}

async fn handle_ei_event(
    ei_event: EiEvent,
    current_client: Option<Position>,
    context: &ei::Context,
    event_tx: &Sender<(Position, CaptureEvent)>,
    release_session: &Notify,
) -> Result<(), CaptureError> {
    let all_capabilities = DeviceCapability::Pointer
        | DeviceCapability::PointerAbsolute
        | DeviceCapability::Keyboard
        | DeviceCapability::Touch
        | DeviceCapability::Scroll
        | DeviceCapability::Button;
    match ei_event {
        EiEvent::SeatAdded(s) => {
            s.seat.bind_capabilities(all_capabilities);
            context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
        }
        EiEvent::SeatRemoved(_) => {
            log::debug!("releasing session: {ei_event:?}");
            release_session.notify_waiters();
        }
        /* EiEvent::DeviceAdded(_) | */
        EiEvent::DeviceRemoved(_) => {
            if *RESTART_SESSION_ON_DEVICE_CHANGE {
                log::debug!("releasing session: {ei_event:?}");
                release_session.notify_waiters();
            } else {
                log::debug!("ignoring device change: {ei_event:?}");
            }
        }
        EiEvent::DevicePaused(_) | EiEvent::DeviceResumed(_) => {}
        EiEvent::DeviceStartEmulating(_) => log::debug!("START EMULATING"),
        EiEvent::DeviceStopEmulating(_) => log::debug!("STOP EMULATING"),
        EiEvent::Disconnected(d) => {
            return Err(CaptureError::Disconnected(format!("{:?}", d.reason)));
        }
        _ => {
            if let Some(pos) = current_client {
                for event in Event::from_ei_event(ei_event) {
                    if event_tx
                        .send((pos, CaptureEvent::Input(event)))
                        .await
                        .is_err()
                    {
                        // the consumer is gone (capture being torn down)
                        return Err(CaptureError::EndOfStream);
                    }
                }
            }
        }
    }
    Ok(())
}

#[async_trait]
impl LanMouseInputCapture for LibeiInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Create(pos))
            .await;
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Destroy(pos))
            .await;
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.notify_release.notify_waiters();
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.cancellation_token.cancel();
        log::debug!("waiting for capture to terminate...");
        // Never propagate a panic out of the task: `terminate` runs on error
        // and shutdown paths, where unwinding would abort the process (the
        // release profile uses panic = "abort") while the capture is dropped.
        let res = if self.capture_task.is_finished() {
            Ok(())
        } else {
            match (&mut self.capture_task).await {
                Ok(res) => res,
                Err(e) => Err(CaptureError::Io(io::Error::other(format!(
                    "libei capture task failed: {e}"
                )))),
            }
        };
        self.terminated = true;
        log::debug!("done!");
        res
    }
}

impl Drop for LibeiInputCapture {
    fn drop(&mut self) {
        if !self.terminated {
            // This is a bug in the owner (terminate() must be awaited), but
            // panicking in Drop turns it into a process abort, including
            // during unwinding. Cancel and abort the task instead; the task
            // owns its own Rc of the portal handle, so it cannot dangle.
            log::error!(
                "LibeiInputCapture dropped without being terminated; aborting the capture task"
            );
            self.cancellation_token.cancel();
            self.capture_task.abort();
        }
    }
}

impl Stream for LibeiInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        let polled = match self.capture_task.poll_unpin(cx) {
            Poll::Ready(Ok(Ok(()))) => Poll::Ready(None),
            Poll::Ready(Ok(Err(e))) => Poll::Ready(Some(Err(e))),
            // A panic inside the task is reported as an error instead of
            // panicking the whole process from the stream poll.
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(CaptureError::Io(io::Error::other(
                format!("libei capture task failed: {e}"),
            ))))),
            Poll::Pending => return self.event_rx.poll_recv(cx).map(|e| e.map(Result::Ok)),
        };
        self.finished = true;
        polled
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "lan-mouse-capture-token-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn tokens_are_written_atomically_with_owner_only_permissions() {
        let dir = scratch_dir("write");
        write_token_to(&dir, CAPTURE_TOKEN_FILE, "token-value").unwrap();

        let path = dir.join(CAPTURE_TOKEN_FILE);
        assert_eq!(read_token_file(&path).as_deref(), Some("token-value"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // The temporary file is renamed, not left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_tokens_are_treated_as_missing() {
        let dir = scratch_dir("empty");
        write_token_to(&dir, CAPTURE_TOKEN_FILE, " \n\t").unwrap();
        assert_eq!(read_token_file(&dir.join(CAPTURE_TOKEN_FILE)), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn closest_barrier_wins_over_the_others() {
        let barrier = |id, position| ICBarrier::new(BarrierID::new(id).expect("id"), position);
        let barriers = [
            barrier(1, (0, 0, 0, 100)),     // left edge
            barrier(2, (100, 0, 100, 100)), // right edge
            barrier(3, (0, 0, 100, 0)),     // top edge
        ];

        assert_eq!(
            find_corresponding_client(&barriers, (-5.0, 50.0)),
            Some(BarrierID::new(1).expect("id"))
        );
        assert_eq!(
            find_corresponding_client(&barriers, (105.0, 50.0)),
            Some(BarrierID::new(2).expect("id"))
        );
        assert_eq!(
            find_corresponding_client(&barriers, (50.0, -5.0)),
            Some(BarrierID::new(3).expect("id"))
        );
    }

    #[test]
    fn barrier_selection_without_barriers_is_none() {
        assert_eq!(find_corresponding_client(&[], (3.0, 4.0)), None);
    }

    #[test]
    fn degenerate_barrier_does_not_divide_by_zero() {
        let distance = distance_to_line(((5.0, 5.0), (5.0, 5.0)), (5.0, 5.0));
        assert_eq!(distance, 0.0);
        let distance = distance_to_line(((5.0, 5.0), (5.0, 5.0)), (8.0, 9.0));
        assert_eq!(distance, 5.0);
    }

    #[test]
    fn legacy_cache_token_is_migrated_once() {
        let state = scratch_dir("state");
        let legacy = scratch_dir("legacy");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(CAPTURE_TOKEN_FILE), "legacy-token").unwrap();

        assert_eq!(
            read_token_from(Some(&state), Some(&legacy)).as_deref(),
            Some("legacy-token")
        );
        assert_eq!(
            read_token_file(&state.join(CAPTURE_TOKEN_FILE)).as_deref(),
            Some("legacy-token")
        );
        // The legacy copy is removed once migration succeeded.
        assert!(!legacy.join(CAPTURE_TOKEN_FILE).exists());

        // The migrated (state) token wins over a stale legacy one.
        fs::write(state.join(CAPTURE_TOKEN_FILE), "current-token").unwrap();
        fs::write(legacy.join(CAPTURE_TOKEN_FILE), "legacy-token").unwrap();
        assert_eq!(
            read_token_from(Some(&state), Some(&legacy)).as_deref(),
            Some("current-token")
        );

        fs::remove_dir_all(&state).unwrap();
        fs::remove_dir_all(&legacy).unwrap();
    }
}
