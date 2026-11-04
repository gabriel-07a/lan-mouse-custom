//! X11 clipboard backend.
//!
//! X11 has no clipboard daemon: the owner of the `CLIPBOARD` selection serves
//! data on request. This backend runs a small X connection on a dedicated
//! thread, polls the selection owner for changes, serves `SelectionRequest`
//! events when it owns the selection, and converts incoming selections into
//! the canonical formats.
//!
//! X11 property transfers are limited in size; images are therefore not
//! propagated by this backend (the portal and native backends handle them).
//! Text and HTML are supported.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver as StdReceiver, Sender as StdSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
            SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window,
            WindowClass,
        },
    },
};

use crate::{Clipboard, ClipboardSnapshot, Error, Format};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        UTF8_STRING,
        TEXT,
        HTML: b"text/html",
        PNG: b"image/png",
        INCR,
        LAN_MOUSE_SELECTION,
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const X11_POLL_SLEEP: Duration = Duration::from_millis(20);
const MAX_X11_PAYLOAD: usize = 1024 * 1024;
/// X11 requests larger than this need the INCR protocol, which this backend
/// does not implement for writes yet.
const MAX_X11_PROPERTY: usize = 256 * 1024;

pub struct X11Clipboard {
    commands: StdSender<Command>,
    changes: UnboundedReceiver<ClipboardSnapshot>,
    last: Arc<Mutex<Option<ClipboardSnapshot>>>,
    thread: Option<thread::JoinHandle<()>>,
}

enum Command {
    Write(ClipboardSnapshot),
    Stop,
}

impl X11Clipboard {
    pub fn new() -> Result<Self, Error> {
        let (command_tx, command_rx) = mpsc::channel();
        let (change_tx, change_rx) = unbounded_channel();
        let last = Arc::new(Mutex::new(None));
        let thread_last = last.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("lan-mouse-x11-clipboard".into())
            .spawn(move || {
                let result = run(command_rx, change_tx, thread_last, ready_tx.clone());
                if let Err(e) = result {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| {
                Error::Backend(format!("could not start the X11 clipboard thread: {e}"))
            })?;

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                commands: command_tx,
                changes: change_rx,
                last,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(Error::Backend(e)),
            Err(_) => Err(Error::Backend(
                "the X11 clipboard thread did not start".into(),
            )),
        }
    }
}

impl Drop for X11Clipboard {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/* The pending read state machine: first ask for TARGETS, then request each
 * supported representation one by one. */
struct Pending {
    awaiting_targets: bool,
    queue: VecDeque<(Atom, Format)>,
    snapshot: ClipboardSnapshot,
}

fn run(
    commands: StdReceiver<Command>,
    changes: UnboundedSender<ClipboardSnapshot>,
    last: Arc<Mutex<Option<ClipboardSnapshot>>>,
    ready: StdSender<Result<(), String>>,
) -> Result<(), String> {
    let (conn, screen_num) =
        x11rb::connect(None).map_err(|e| format!("could not connect to the X server: {e}"))?;
    let screen = &conn.setup().roots[screen_num];
    let window: Window = conn
        .generate_id()
        .map_err(|e| format!("X11 generate_id: {e}"))?;
    conn.create_window(
        0,
        window,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(|e| format!("X11 create_window: {e}"))?
    .check()
    .map_err(|e| format!("X11 create_window: {e}"))?;

    let atoms = Atoms::new(&conn)
        .map_err(|e| format!("X11 intern atoms: {e}"))?
        .reply()
        .map_err(|e| format!("X11 intern atoms: {e}"))?;

    let _ = ready.send(Ok(()));

    let mut pending: Option<Pending> = None;
    let mut serve: Option<ClipboardSnapshot> = None;
    let mut owning = false;
    let mut last_owner: Window = 0;
    let mut last_poll = Instant::now();

    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Write(snapshot)) => {
                    if let Err(e) = set_owner(&conn, window, &atoms) {
                        log::warn!("X11: could not own the clipboard: {e}");
                    } else {
                        *last.lock().expect("lock") = Some(snapshot.clone());
                        serve = Some(snapshot);
                        owning = true;
                        last_owner = window;
                    }
                }
                Ok(Command::Stop) | Err(TryRecvError::Disconnected) => return Ok(()),
                Err(TryRecvError::Empty) => break,
            }
        }

        while let Some(event) = conn
            .poll_for_event()
            .map_err(|e| format!("X11 poll_for_event: {e}"))?
        {
            match event {
                Event::SelectionRequest(request) => {
                    if let Err(e) =
                        reply_to_request(&conn, window, &atoms, serve.as_ref(), &request)
                    {
                        log::warn!("X11: failed to answer a selection request: {e}");
                    }
                }
                Event::SelectionNotify(notify) => {
                    if let Some(mut current) = pending.take() {
                        match handle_notify(&conn, window, &atoms, &mut current, notify) {
                            Ok(NotifyOutcome::Continue) => pending = Some(current),
                            Ok(NotifyOutcome::Done) => {
                                *last.lock().expect("lock") = Some(current.snapshot.clone());
                                let _ = changes.send(current.snapshot);
                            }
                            Ok(NotifyOutcome::Failed) => {}
                            Err(e) => log::warn!("X11: clipboard read failed: {e}"),
                        }
                    }
                }
                Event::SelectionClear(clear) if clear.owner == window => {
                    owning = false;
                    serve = None;
                }
                _ => {}
            }
        }

        if last_poll.elapsed() >= POLL_INTERVAL {
            last_poll = Instant::now();
            if !owning {
                let owner = selection_owner(&conn, &atoms)?;
                if owner != last_owner {
                    last_owner = owner;
                    if owner != 0 && owner != window {
                        pending = Some(start_read(&conn, window, &atoms, owner)?);
                    }
                }
            }
        }

        let _ = conn.flush();
        thread::sleep(X11_POLL_SLEEP);
    }
}

fn set_owner(
    conn: &x11rb::rust_connection::RustConnection,
    window: Window,
    atoms: &Atoms,
) -> Result<(), String> {
    conn.set_selection_owner(window, atoms.CLIPBOARD, CURRENT_TIME)
        .map_err(|e| format!("set_selection_owner: {e}"))?
        .check()
        .map_err(|e| format!("set_selection_owner: {e}"))
}

fn selection_owner(
    conn: &x11rb::rust_connection::RustConnection,
    atoms: &Atoms,
) -> Result<Window, String> {
    let reply = conn
        .get_selection_owner(atoms.CLIPBOARD)
        .map_err(|e| format!("get_selection_owner: {e}"))?
        .reply()
        .map_err(|e| format!("get_selection_owner: {e}"))?;
    Ok(reply.owner)
}

fn start_read(
    conn: &x11rb::rust_connection::RustConnection,
    window: Window,
    atoms: &Atoms,
    _owner: Window,
) -> Result<Pending, String> {
    conn.convert_selection(
        window,
        atoms.CLIPBOARD,
        atoms.TARGETS,
        atoms.LAN_MOUSE_SELECTION,
        CURRENT_TIME,
    )
    .map_err(|e| format!("convert_selection: {e}"))?
    .check()
    .map_err(|e| format!("convert_selection: {e}"))?;
    Ok(Pending {
        awaiting_targets: true,
        queue: VecDeque::new(),
        snapshot: ClipboardSnapshot::new(),
    })
}

enum NotifyOutcome {
    Continue,
    Done,
    Failed,
}

fn handle_notify(
    conn: &x11rb::rust_connection::RustConnection,
    window: Window,
    atoms: &Atoms,
    pending: &mut Pending,
    notify: SelectionNotifyEvent,
) -> Result<NotifyOutcome, String> {
    if notify.requestor != window || notify.selection != atoms.CLIPBOARD {
        return Ok(NotifyOutcome::Continue);
    }

    if pending.awaiting_targets {
        if notify.property == 0 {
            return Ok(NotifyOutcome::Failed);
        }
        let targets = read_atoms(conn, window, notify.property)?;
        if targets.contains(&atoms.UTF8_STRING) {
            pending.queue.push_back((atoms.UTF8_STRING, Format::Text));
        } else if targets.contains(&atoms.TEXT) {
            pending.queue.push_back((atoms.TEXT, Format::Text));
        }
        if targets.contains(&atoms.HTML) {
            pending.queue.push_back((atoms.HTML, Format::Html));
        }
        if pending.queue.is_empty() {
            return Ok(NotifyOutcome::Failed);
        }
        pending.awaiting_targets = false;
        return advance(conn, window, atoms, pending);
    }

    if notify.property == 0 {
        // This representation was not available; try the next one.
        return advance(conn, window, atoms, pending);
    }
    let (type_atom, bytes) = read_bytes(conn, window, notify.property)?;
    if type_atom == atoms.INCR {
        // The owner offers the data through the INCR protocol. Skip this
        // representation instead of storing the size placeholder as data.
        log::debug!("X11: skipping an INCR clipboard representation");
        return advance(conn, window, atoms, pending);
    }
    if let Some(format) = format_for_target(notify.target, atoms) {
        if !bytes.is_empty() {
            pending.snapshot.insert(format, bytes);
        }
    }
    advance(conn, window, atoms, pending)
}

fn advance(
    conn: &x11rb::rust_connection::RustConnection,
    window: Window,
    atoms: &Atoms,
    pending: &mut Pending,
) -> Result<NotifyOutcome, String> {
    let Some((target, _)) = pending.queue.pop_front() else {
        return Ok(NotifyOutcome::Done);
    };
    conn.convert_selection(
        window,
        atoms.CLIPBOARD,
        target,
        atoms.LAN_MOUSE_SELECTION,
        CURRENT_TIME,
    )
    .map_err(|e| format!("convert_selection: {e}"))?
    .check()
    .map_err(|e| format!("convert_selection: {e}"))?;
    Ok(NotifyOutcome::Continue)
}

fn read_atoms(
    conn: &x11rb::rust_connection::RustConnection,
    window: Window,
    property: Atom,
) -> Result<Vec<Atom>, String> {
    let reply = conn
        .get_property(true, window, property, AtomEnum::ATOM, 0, 1024)
        .map_err(|e| format!("get_property(TARGETS): {e}"))?
        .reply()
        .map_err(|e| format!("get_property(TARGETS): {e}"))?;
    Ok(reply
        .value32()
        .map(|values| values.collect())
        .unwrap_or_default())
}

fn read_bytes(
    conn: &x11rb::rust_connection::RustConnection,
    window: Window,
    property: Atom,
) -> Result<(Atom, Vec<u8>), String> {
    let mut data = Vec::new();
    let mut offset = 0u32;
    let mut too_large = false;
    let mut type_atom = 0;
    loop {
        let reply = conn
            .get_property(false, window, property, AtomEnum::ANY, offset, 65536)
            .map_err(|e| format!("get_property: {e}"))?
            .reply()
            .map_err(|e| format!("get_property: {e}"))?;
        if offset == 0 {
            type_atom = reply.type_;
        }
        if reply.value.is_empty() {
            if reply.bytes_after == 0 {
                break;
            }
            return Err("X11 clipboard returned an empty property chunk".into());
        }
        data.extend_from_slice(&reply.value);
        if data.len() > MAX_X11_PAYLOAD {
            too_large = true;
            break;
        }
        if reply.bytes_after == 0 {
            break;
        }
        offset += (reply.value.len() as u32).div_ceil(4);
    }
    let _ = conn
        .delete_property(window, property)
        .map_err(|e| format!("delete_property: {e}"))?
        .check();
    if too_large {
        return Err("X11 clipboard payload exceeds the size limit".into());
    }
    Ok((type_atom, data))
}

fn format_for_target(target: Atom, atoms: &Atoms) -> Option<Format> {
    format_for_target_ids(target, atoms.UTF8_STRING, atoms.TEXT, atoms.HTML, atoms.PNG)
}

fn format_for_target_ids(
    target: Atom,
    utf8: Atom,
    text: Atom,
    html: Atom,
    png: Atom,
) -> Option<Format> {
    if target == utf8 || target == text {
        Some(Format::Text)
    } else if target == html {
        Some(Format::Html)
    } else if target == png {
        Some(Format::Png)
    } else {
        None
    }
}

fn reply_to_request(
    conn: &x11rb::rust_connection::RustConnection,
    window: Window,
    atoms: &Atoms,
    snapshot: Option<&ClipboardSnapshot>,
    request: &SelectionRequestEvent,
) -> Result<(), String> {
    if request.selection != atoms.CLIPBOARD || request.owner != window {
        return Ok(());
    }
    let property = if request.property == 0 {
        request.target
    } else {
        request.property
    };
    let (type_atom, format, data) = match (snapshot, request.target) {
        (Some(snapshot), target) if target == atoms.TARGETS => {
            let mut targets = vec![atoms.TARGETS];
            if snapshot.get(Format::Text).is_some() {
                targets.push(atoms.UTF8_STRING);
                targets.push(atoms.TEXT);
            }
            if snapshot.get(Format::Html).is_some() {
                targets.push(atoms.HTML);
            }
            if snapshot.get(Format::Png).is_some() {
                targets.push(atoms.PNG);
            }
            let bytes = targets
                .iter()
                .flat_map(|atom| atom.to_ne_bytes())
                .collect::<Vec<_>>();
            (Atom::from(AtomEnum::ATOM), 32, bytes)
        }
        (Some(snapshot), target) if target == atoms.UTF8_STRING || target == atoms.TEXT => {
            match snapshot.get(Format::Text) {
                Some(bytes) if bytes.len() <= MAX_X11_PROPERTY => {
                    (atoms.UTF8_STRING, 8, bytes.to_vec())
                }
                _ => return send_notify(conn, request, 0),
            }
        }
        (Some(snapshot), target) if target == atoms.HTML => match snapshot.get(Format::Html) {
            Some(bytes) if bytes.len() <= MAX_X11_PROPERTY => (atoms.HTML, 8, bytes.to_vec()),
            _ => return send_notify(conn, request, 0),
        },
        (Some(snapshot), target) if target == atoms.PNG => match snapshot.get(Format::Png) {
            Some(bytes) if bytes.len() <= MAX_X11_PROPERTY => (atoms.PNG, 8, bytes.to_vec()),
            _ => return send_notify(conn, request, 0),
        },
        _ => return send_notify(conn, request, 0),
    };

    let units = if format == 32 {
        (data.len() / 4) as u32
    } else {
        data.len() as u32
    };
    conn.change_property(
        PropMode::REPLACE,
        request.requestor,
        property,
        type_atom,
        format,
        units,
        &data,
    )
    .map_err(|e| format!("change_property: {e}"))?
    .check()
    .map_err(|e| format!("change_property: {e}"))?;
    send_notify(conn, request, property)
}

fn send_notify(
    conn: &x11rb::rust_connection::RustConnection,
    request: &SelectionRequestEvent,
    property: Atom,
) -> Result<(), String> {
    let event = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property,
    };
    conn.send_event(false, request.requestor, EventMask::NO_EVENT, event)
        .map_err(|e| format!("send_event: {e}"))?
        .check()
        .map_err(|e| format!("send_event: {e}"))
}

#[async_trait(?Send)]
impl Clipboard for X11Clipboard {
    fn backend_name(&self) -> &'static str {
        "x11"
    }

    async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
        Ok(self.last.lock().expect("lock").clone().unwrap_or_default())
    }

    async fn write(&mut self, snapshot: &ClipboardSnapshot) -> Result<(), Error> {
        self.commands
            .send(Command::Write(snapshot.clone()))
            .map_err(|_| Error::Backend("the X11 clipboard thread stopped".into()))?;
        *self.last.lock().expect("lock") = Some(snapshot.clone());
        Ok(())
    }

    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
        Ok(self.changes.recv().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an X11 display"]
    fn connects_to_the_x_server() {
        let _backend = X11Clipboard::new().expect("X11 backend");
    }

    #[test]
    fn target_mapping_is_canonical() {
        assert_eq!(format_for_target_ids(3, 3, 4, 5, 6), Some(Format::Text));
        assert_eq!(format_for_target_ids(4, 3, 4, 5, 6), Some(Format::Text));
        assert_eq!(format_for_target_ids(5, 3, 4, 5, 6), Some(Format::Html));
        assert_eq!(format_for_target_ids(99, 3, 4, 5, 6), None);
        assert_eq!(format_for_target_ids(6, 3, 4, 5, 6), Some(Format::Png));
    }
}
