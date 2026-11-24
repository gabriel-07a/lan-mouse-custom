//! Desk simulator: two (or more) real lan-mouse services on virtual screens.
//!
//! Every PC of a [`Desk`] runs the **real** [`Service`] (capture task,
//! emulation/listen task, connection, hand-off logic, DTLS over loopback),
//! inside the test process. Only the parts that touch the physical world are
//! replaced:
//!
//! | real machine            | simulator                                  |
//! |-------------------------|--------------------------------------------|
//! | the user's mouse        | [`Desk::move_mouse`] (paced like a hand)   |
//! | the monitor and cursor  | [`ScreenState`]: size + cursor position     |
//! | OS capture backend      | [`SimCapture`] (barriers on the screen)    |
//! | OS emulation backend    | [`SimEmulation`] (moves the same cursor)   |
//!
//! The mouse is physically attached to the first PC added to the desk (the
//! "hand" PC). [`Desk::focus`] answers the question a person answers by
//! looking at the monitors: on which screen is the cursor the user is
//! moving?
//!
//! Isolation: each PC gets its own temporary config/certificate/audit/IPC
//! socket, no discovery beacon and no cursor-hide state file (see
//! [`ServiceOptions::isolated`]). Ports are free loopback ports. The clock is
//! the real one (DTLS needs it), so waits are deadlines, never fixed sleeps.
//!
//! See `docs/testing/desk-simulator.md` for the guide.

mod screen;

#[cfg(test)]
mod clipboard_scenarios;
#[cfg(test)]
mod lock_scenarios;
#[cfg(test)]
mod network_scenarios;
#[cfg(test)]
mod os_scenarios;
#[cfg(test)]
mod scenarios;

use std::{
    fmt::Write as _,
    net::{Ipv4Addr, UdpSocket},
    rc::Rc,
    time::{Duration, Instant},
};

use input_capture::{InputCapture, Position};
use input_clipboard::{Clipboard, ClipboardSnapshot, Format, SharedClipboard};
use input_emulation::InputEmulation;
use lan_mouse_test_support::env::TestEnv;
use tokio::task::{JoinHandle, spawn_local};

use crate::{
    client::ClientManager,
    config::Config,
    service::{Service, ServiceOptions},
};

pub(crate) use screen::{
    Personality, ScreenEvent, ScreenState, SharedScreen, SimCapture, SimEmulation, lock,
};

/// How the simulated hand moves the mouse.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Hand {
    /// largest delta of one motion event, in px
    pub(crate) step_px: f64,
    /// time between two motion events (1 ms = a 1000 Hz mouse)
    pub(crate) interval: Duration,
}

impl Default for Hand {
    /// A fast but ordinary swipe: 4 px per event at 1000 Hz (~4000 px/s).
    fn default() -> Self {
        Self {
            step_px: 4.0,
            interval: Duration::from_millis(1),
        }
    }
}

/// Where the cursor the user is moving is visible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    /// on this PC's screen
    On(&'static str),
    /// the hand PC is captured but no PC is being driven: the user sees no
    /// cursor (the "cursor got stuck / disappeared" class of bugs)
    Lost,
}

/// One virtual PC on the desk.
pub(crate) struct Pc {
    pub(crate) name: &'static str,
    pub(crate) screen: SharedScreen,
    /// this PC's system clipboard (what Ctrl+C writes and Ctrl+V reads)
    pub(crate) clipboard: SharedClipboard,
    pub(crate) clients: ClientManager,
    /// DTLS listen port
    pub(crate) port: u16,
    /// discovery beacon port, when the desk is [`DeskBuilder::discoverable`]
    pub(crate) discovery_port: Option<u16>,
    env: TestEnv,
    task: JoinHandle<()>,
}

impl Pc {
    /// Frontend IPC socket of this PC's service (what the GUI connects to).
    pub(crate) fn ipc_socket(&self) -> std::path::PathBuf {
        self.env.runtime_dir().join("lan-mouse-socket.sock")
    }

    /// Caps Lock / Num Lock state of this PC ([`lan_mouse_proto::lock_bits`]).
    pub(crate) fn locks(&self) -> u8 {
        self.state().locks
    }

    /// Snapshot of this PC's screen.
    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, ScreenState> {
        lock(&self.screen)
    }

    /// Number of times the local mouse crossed into a peer.
    pub(crate) fn capture_begins(&self) -> usize {
        self.count(|e| matches!(e, ScreenEvent::CaptureBegin { .. }))
    }

    /// Number of times a peer started driving this screen.
    pub(crate) fn remote_begins(&self) -> usize {
        self.count(|e| matches!(e, ScreenEvent::RemoteBegin { .. }))
    }

    /// Where the last capture release put the local cursor back
    /// (`None`: no release yet, or a release without return coordinate).
    pub(crate) fn last_return(&self) -> Option<(Position, f32)> {
        self.state().log.iter().rev().find_map(|l| match l.event {
            ScreenEvent::CaptureReleased { at } => Some(at),
            _ => None,
        })?
    }

    /// Normalised coordinate of the last edge-return request this screen sent.
    pub(crate) fn last_edge_return(&self) -> Option<f32> {
        self.state().log.iter().rev().find_map(|l| match l.event {
            ScreenEvent::EdgeReturn { along, .. } => Some(along),
            _ => None,
        })
    }

    fn count(&self, predicate: impl Fn(&ScreenEvent) -> bool) -> usize {
        self.state()
            .log
            .iter()
            .filter(|l| predicate(&l.event))
            .count()
    }
}

/// Declares the PCs and how they are arranged.
#[derive(Default)]
pub(crate) struct DeskBuilder {
    pcs: Vec<(&'static str, u32, u32)>,
    /// `(of, side, target)`: `of` has `target` configured at `side`
    links: Vec<(&'static str, Position, &'static str)>,
    /// extra `config.toml` text per PC
    extra_config: Vec<(&'static str, String)>,
    personalities: Vec<(&'static str, Personality)>,
    hand: Hand,
    /// top-level `config.toml` keys per PC (before the `[[clients]]` tables)
    top_config: Vec<(&'static str, String)>,
    /// loopback peers need pairing approval like real machines
    untrusted: bool,
    /// every PC answers discovery probes on its own port
    discoverable: bool,
    /// Caps Lock / Num Lock state per PC before its service starts
    initial_locks: Vec<(&'static str, u8)>,
}

impl DeskBuilder {
    /// Add a PC with a single monitor of `width` x `height` px. The first PC
    /// added holds the physical mouse.
    pub(crate) fn pc(mut self, name: &'static str, width: u32, height: u32) -> Self {
        assert!(
            self.pcs.iter().all(|(n, ..)| *n != name),
            "duplicate PC {name}"
        );
        self.pcs.push((name, width, height));
        self
    }

    /// `target` sits at `side` of `of`, i.e. `of` gets a client for `target`
    /// at `side` (what the user configures in the GUI).
    pub(crate) fn place(mut self, target: &'static str, side: Position, of: &'static str) -> Self {
        self.links.push((of, side, target));
        self
    }

    /// Append raw TOML to `name`'s `config.toml`. It goes after the
    /// `[[clients]]` tables, so it must be whole tables (e.g. `[clipboard]`).
    pub(crate) fn config(mut self, name: &'static str, toml: &str) -> Self {
        self.extra_config.push((name, toml.to_string()));
        self
    }

    /// Top-level `config.toml` keys for `name` (e.g. `sync_num_lock = false`).
    pub(crate) fn top_config(mut self, name: &'static str, toml: &str) -> Self {
        self.top_config.push((name, toml.to_string()));
        self
    }

    /// Caps Lock / Num Lock of `name` before its service starts
    /// ([`lan_mouse_proto::lock_bits`]); the service's first reading is a
    /// baseline, not a change.
    pub(crate) fn locks(mut self, name: &'static str, bits: u8) -> Self {
        self.initial_locks.push((name, bits));
        self
    }

    /// Do not pre-trust loopback: a new peer must be approved, as between two
    /// real machines.
    pub(crate) fn untrusted(mut self) -> Self {
        self.untrusted = true;
        self
    }

    /// Every PC answers discovery probes on its own port (never announces on
    /// the real network).
    pub(crate) fn discoverable(mut self) -> Self {
        self.discoverable = true;
        self
    }

    /// Give `name` the quirks of an OS (default: [`Personality::baseline`]).
    pub(crate) fn personality(mut self, name: &'static str, personality: Personality) -> Self {
        self.personalities.push((name, personality));
        self
    }

    pub(crate) fn hand(mut self, hand: Hand) -> Self {
        self.hand = hand;
        self
    }

    /// Start every service and wait until every configured link is up.
    ///
    /// Must run inside a `tokio::task::LocalSet` (the services use
    /// `spawn_local`).
    pub(crate) async fn start(self) -> Desk {
        assert!(!self.pcs.is_empty(), "a desk needs at least one PC");
        let ports: Vec<u16> = self.pcs.iter().map(|_| free_udp_port()).collect();
        let port_of = |name: &str| {
            let index = self
                .pcs
                .iter()
                .position(|(n, ..)| *n == name)
                .unwrap_or_else(|| panic!("unknown PC {name}"));
            ports[index]
        };

        let mut pcs = Vec::new();
        for (&(name, width, height), &port) in self.pcs.iter().zip(&ports) {
            let env = TestEnv::new(format!("desk-sim-{name}"));
            let mut toml = format!(
                "port = {port}\n\
                 # replaced by the simulator backends; keeps the config valid\n\
                 capture_backend = \"dummy\"\n\
                 emulation_backend = \"dummy\"\n"
            );
            if !self.untrusted {
                toml.push_str("# loopback peers are pre-trusted: no pairing prompt\n");
                toml.push_str("trusted_ips = [\"127.0.0.1\"]\n");
            }
            let discovery_port = self.discoverable.then(free_udp_port);
            if let Some(discovery_port) = discovery_port {
                writeln!(toml, "discovery_port = {discovery_port}").expect("write to string");
            }
            for (_, top) in self.top_config.iter().filter(|(n, _)| *n == name) {
                toml.push_str(top);
                toml.push('\n');
            }
            for &(_, side, target) in self.links.iter().filter(|(of, ..)| *of == name) {
                write!(
                    toml,
                    "\n[[clients]]\n\
                     position = \"{}\"\n\
                     ips = [\"127.0.0.1\"]\n\
                     port = {}\n\
                     activate_on_startup = true\n",
                    position_name(side),
                    port_of(target),
                )
                .expect("write to string");
            }
            for (_, extra) in self.extra_config.iter().filter(|(n, _)| *n == name) {
                toml.push('\n');
                toml.push_str(extra);
                toml.push('\n');
            }
            let config_path = env.write("config/lan-mouse/config.toml", &toml);
            let config = Config::load_for_test(&config_path).expect("desk-sim config");

            let mut state = ScreenState::new(name, width, height);
            if let Some((_, bits)) = self.initial_locks.iter().find(|(n, _)| *n == name) {
                state.locks = *bits;
            }
            if let Some((_, personality)) = self.personalities.iter().find(|(n, _)| *n == name) {
                state.personality = personality.clone();
            }
            let screen = state.shared();
            let capture_screen = screen.clone();
            let emulation_screen = screen.clone();
            let clipboard = SharedClipboard::new();
            let service_clipboard = clipboard.clone();
            let lock_screen = screen.clone();
            let options = ServiceOptions {
                discovery_beacon: self.discoverable,
                // the PC's own lock state, as the OS would report it
                lock_reader: Some(Rc::new(move || {
                    let mut state = lock(&lock_screen);
                    state.lock_reads += 1;
                    Some(crate::lock_sync::Locks {
                        known: lan_mouse_proto::lock_bits::CAPS | lan_mouse_proto::lock_bits::NUM,
                        on: state.locks,
                    })
                })),
                ipc_socket: Some(env.runtime_dir().join("lan-mouse-socket.sock")),
                audit_path: Some(env.state_dir().join("audit.jsonl")),
                isolated: true,
                capture_backend: Some(Rc::new(move || {
                    InputCapture::from_backend(Box::new(SimCapture::new(capture_screen.clone())))
                })),
                emulation_backend: Some(Rc::new(move || {
                    InputEmulation::from_backend(Box::new(SimEmulation::new(
                        emulation_screen.clone(),
                    )))
                })),
                // in-memory clipboard: the OS one would open the GNOME
                // RemoteDesktop portal dialog for every simulated PC
                clipboard_backend: Some(Rc::new(move || {
                    Box::new(service_clipboard.clone()) as Box<dyn Clipboard>
                })),
            };
            let mut service = Service::with_options(config, options)
                .await
                .unwrap_or_else(|e| panic!("desk-sim: service {name} failed to start: {e}"));
            let clients = service.client_manager();
            let task = spawn_local(async move {
                if let Err(e) = service.run().await {
                    log::error!("desk-sim: service {name} stopped: {e}");
                }
            });
            pcs.push(Pc {
                name,
                screen,
                clipboard,
                clients,
                port,
                discovery_port,
                env,
                task,
            });
        }

        let links = self
            .links
            .iter()
            .map(|&(of, _, target)| (of, port_of(target)))
            .collect();
        let desk = Desk {
            pcs,
            links,
            layout: self.links,
            hand: self.hand,
            started: Instant::now(),
        };
        desk.wait_for_links().await;
        desk
    }
}

/// A running set of virtual PCs.
pub(crate) struct Desk {
    pcs: Vec<Pc>,
    /// `(of, target port)` for every configured client
    links: Vec<(&'static str, u16)>,
    /// `(of, side, target)` as declared with [`DeskBuilder::place`]
    layout: Vec<(&'static str, Position, &'static str)>,
    hand: Hand,
    started: Instant,
}

impl Drop for Desk {
    fn drop(&mut self) {
        for pc in &self.pcs {
            pc.task.abort();
        }
    }
}

impl Desk {
    pub(crate) fn builder() -> DeskBuilder {
        DeskBuilder::default()
    }

    pub(crate) fn pc(&self, name: &str) -> &Pc {
        self.pcs
            .iter()
            .find(|pc| pc.name == name)
            .unwrap_or_else(|| panic!("unknown PC {name}"))
    }

    fn hand_pc(&self) -> &Pc {
        &self.pcs[0]
    }

    /// Move the physical mouse of the hand PC (the first one added).
    pub(crate) async fn move_mouse(&self, dx: f64, dy: f64) {
        self.move_mouse_on(self.hand_pc().name, dx, dy).await;
    }

    /// Move the physical mouse attached to `name` by (`dx`, `dy`), split into
    /// hand-sized motion events paced by [`Hand::interval`].
    pub(crate) async fn move_mouse_on(&self, name: &str, dx: f64, dy: f64) {
        let pc = self.pc(name);
        let distance = dx.abs().max(dy.abs());
        let steps = (distance / self.hand.step_px).ceil().max(1.0) as u32;
        let (sx, sy) = (dx / f64::from(steps), dy / f64::from(steps));
        let mut tick = tokio::time::interval(self.hand.interval);
        for _ in 0..steps {
            tick.tick().await;
            lock(&pc.screen).local_motion(sx, sy);
        }
        // let the services process the last events before the caller looks
        tokio::task::yield_now().await;
    }

    /// The user taps `key` on the keyboard attached to `name` (Linux key
    /// code). While `name` captures for a peer the key goes to that peer.
    pub(crate) async fn key_tap_on(&self, name: &str, key: u32) {
        lock(&self.pc(name).screen).key_tap(key);
        tokio::task::yield_now().await;
    }

    /// Ctrl+C on `name`: the user copies `snapshot` there.
    pub(crate) fn copy(&self, name: &str, snapshot: ClipboardSnapshot) {
        self.pc(name).clipboard.set_local(snapshot);
    }

    /// Ctrl+C of a piece of text on `name`.
    pub(crate) fn copy_text(&self, name: &str, text: &str) {
        self.copy(
            name,
            ClipboardSnapshot::from_items([(Format::Text, text.as_bytes().to_vec())]),
        );
    }

    /// Ctrl+C of an image (PNG bytes) on `name`.
    pub(crate) fn copy_image(&self, name: &str, png: Vec<u8>) {
        self.copy(name, ClipboardSnapshot::from_items([(Format::Png, png)]));
    }

    /// Ctrl+V on `name`: what pasting there would insert right now.
    pub(crate) fn paste(&self, name: &str) -> ClipboardSnapshot {
        self.pc(name).clipboard.current()
    }

    /// Wait until pasting on `name` gives exactly `expected`.
    pub(crate) async fn wait_paste(&self, name: &str, expected: &ClipboardSnapshot) {
        self.wait_until(
            &format!("paste on {name} == {}", describe_snapshot(expected)),
            Duration::from_secs(10),
            |desk| desk.paste(name) == *expected,
        )
        .await;
    }

    /// Where the cursor moved by the hand PC's mouse is visible right now.
    pub(crate) fn focus(&self) -> Focus {
        self.focus_from(self.hand_pc().name)
    }

    /// Where the cursor moved by `hand`'s mouse is visible right now: on
    /// `hand` while it is not captured, otherwise on the PC being driven.
    pub(crate) fn focus_from(&self, hand: &str) -> Focus {
        let hand = self.pc(hand);
        let Some(edge) = hand.state().captured else {
            return Focus::On(hand.name);
        };
        let target = self.neighbour(hand.name, edge);
        match target {
            Some(pc) if pc.state().remote_edge.is_some() => Focus::On(pc.name),
            _ => Focus::Lost,
        }
    }

    /// The PC configured at `side` of `of`: from the declared layout, or from
    /// the client the service holds at that side (a link created at run time,
    /// e.g. from the Network page), matched by its listen port.
    fn neighbour(&self, of: &str, side: Position) -> Option<&Pc> {
        if let Some(&(_, _, target)) = self.layout.iter().find(|&&(o, s, _)| o == of && s == side) {
            return Some(self.pc(target));
        }
        let clients = &self.pc(of).clients;
        let side = match side {
            Position::Left => lan_mouse_ipc::Position::Left,
            Position::Right => lan_mouse_ipc::Position::Right,
            Position::Top => lan_mouse_ipc::Position::Top,
            Position::Bottom => lan_mouse_ipc::Position::Bottom,
        };
        let handle = clients.client_at(side)?;
        let port = clients.get_state(handle)?.0.port;
        self.pcs.iter().find(|pc| pc.name != of && pc.port == port)
    }

    /// Cursor position on `name`'s screen.
    pub(crate) fn cursor(&self, name: &str) -> (f64, f64) {
        self.pc(name).state().cursor
    }

    /// Wait (real time, up to `timeout`) until `condition` holds; panics with
    /// a full desk report otherwise.
    pub(crate) async fn wait_until(
        &self,
        what: &str,
        timeout: Duration,
        condition: impl Fn(&Desk) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            if condition(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "desk-sim: timed out after {timeout:?} waiting for: {what}\n{}",
                    self.report()
                );
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Wait until every service has run `polls` more lock polls. A poll reads
    /// the state and sends any change before the next one starts, so after
    /// a few polls on every PC a lock change (and any echo of it) has gone
    /// round; counting polls instead of sleeping keeps that true on a slow
    /// machine and returns sooner on a fast one.
    pub(crate) async fn settle_lock_polls(&self, polls: usize) {
        let start: Vec<usize> = self.pcs.iter().map(|pc| pc.state().lock_reads).collect();
        self.wait_until(
            &format!("{polls} lock polls on every PC"),
            Duration::from_secs(10),
            |desk| {
                desk.pcs
                    .iter()
                    .zip(&start)
                    .all(|(pc, start)| pc.state().lock_reads >= start + polls)
            },
        )
        .await;
    }

    /// Wait until the hand PC's cursor is on `name`.
    pub(crate) async fn wait_focus(&self, name: &'static str) {
        self.wait_focus_from(self.hand_pc().name, name).await;
    }

    /// Wait until the cursor moved by `hand`'s mouse is on `name`.
    pub(crate) async fn wait_focus_from(&self, hand: &str, name: &'static str) {
        self.wait_until(
            &format!("{hand}'s cursor on {name}"),
            Duration::from_secs(5),
            |desk| desk.focus_from(hand) == Focus::On(name),
        )
        .await;
    }

    /// Let `duration` of real time pass and assert that `condition` held the
    /// whole time (checked every few ms). Catches flapping/ping-pong.
    pub(crate) async fn hold(
        &self,
        what: &str,
        duration: Duration,
        condition: impl Fn(&Desk) -> bool,
    ) {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            assert!(
                condition(self),
                "desk-sim: stopped holding: {what}\n{}",
                self.report()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Human-readable state of every PC and its recent screen events.
    pub(crate) fn report(&self) -> String {
        let mut out = format!("focus: {:?}\n", self.focus());
        for pc in &self.pcs {
            let state = pc.state();
            writeln!(out, "  {state}").expect("write to string");
            writeln!(
                out,
                "    clipboard: {} (applied from peers: {})",
                describe_snapshot(&pc.clipboard.current()),
                pc.clipboard.applied().len()
            )
            .expect("write to string");
            let skip = state.log.len().saturating_sub(15);
            for logged in &state.log[skip..] {
                writeln!(
                    out,
                    "    +{:>6} ms  {:?}",
                    logged.at.duration_since(self.started).as_millis(),
                    logged.event
                )
                .expect("write to string");
            }
            for (_, config, client) in pc.clients.get_client_states() {
                writeln!(
                    out,
                    "    client {:?}:{} active={} alive={} addr={:?} error={:?}",
                    config.pos,
                    config.port,
                    client.active,
                    client.alive,
                    client.active_addr,
                    client.last_error
                )
                .expect("write to string");
            }
            writeln!(out, "    files: {}", pc.env.root().display()).expect("write to string");
        }
        out
    }

    /// Every configured client is connected and its peer can emulate.
    async fn wait_for_links(&self) {
        self.wait_until(
            "every configured link connected",
            Duration::from_secs(10),
            |desk| {
                desk.links.iter().all(|&(of, port)| {
                    desk.pc(of)
                        .clients
                        .get_client_states()
                        .iter()
                        .any(|(_, config, state)| {
                            config.port == port && state.alive && state.active_addr.is_some()
                        })
                })
            },
        )
        .await;
    }
}

/// Short description of a clipboard snapshot for reports.
pub(crate) fn describe_snapshot(snapshot: &ClipboardSnapshot) -> String {
    if snapshot.is_empty() {
        return "<empty>".to_string();
    }
    snapshot
        .iter()
        .map(|(format, data)| match format {
            Format::Text if data.len() <= 40 => {
                format!("{format} {:?}", String::from_utf8_lossy(data))
            }
            _ => format!("{format} {} bytes", data.len()),
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

fn position_name(side: Position) -> &'static str {
    match side {
        Position::Left => "left",
        Position::Right => "right",
        Position::Top => "top",
        Position::Bottom => "bottom",
    }
}

/// A free loopback UDP port. The listener binds it a moment later; the tiny
/// race is acceptable for the simulator (the listener has no port-0 API).
fn free_udp_port() -> u16 {
    UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|socket| socket.local_addr())
        .map(|addr| addr.port())
        .expect("free udp port")
}
