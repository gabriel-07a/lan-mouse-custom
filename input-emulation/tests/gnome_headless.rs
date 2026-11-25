//! End-to-end tests of the libei backend against a real GNOME Shell.
//!
//! Run through `scripts/gnome-headless.sh --test`, which starts a throwaway
//! headless GNOME Shell (Ubuntu mode, auto-hiding dock) and a probe extension
//! that reports the pointer and the dock state. Without that session
//! (`LAN_MOUSE_TEST_PROBE` unset) every test returns immediately, so a plain
//! `cargo test` stays hermetic.
//!
//! The EIS socket comes straight from Mutter's `org.gnome.Mutter.RemoteDesktop`
//! D-Bus API (what the RemoteDesktop portal uses underneath), so no consent
//! dialog is involved; everything after the socket is the shipping backend.
#![cfg(all(unix, not(target_os = "macos")))]

use std::{collections::HashMap, os::fd::OwnedFd, path::PathBuf, time::Duration};

use input_emulation::{InputEmulation, PointerEdge};
use input_event::{Event, PointerEvent};
use zbus::zvariant::{self, OwnedObjectPath};

const MUTTER_RD: &str = "org.gnome.Mutter.RemoteDesktop";
const MUTTER_RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const MUTTER_RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";

/// Dock states as ubuntu-dock numbers them (`docking.js` `State`).
const DOCK_HIDDEN: i64 = 0;
const DOCK_SHOWING: i64 = 1;
const DOCK_SHOWN: i64 = 2;

fn probe_path() -> Option<PathBuf> {
    std::env::var_os("LAN_MOUSE_TEST_PROBE").map(PathBuf::from)
}

#[derive(Debug, Clone, Copy)]
struct Probe {
    x: i64,
    y: i64,
    /// primary monitor (the one with the dock): origin and size
    mx: i64,
    my: i64,
    width: i64,
    height: i64,
    dock: i64,
}

/// The probe file is flat JSON written by the test extension.
fn read_probe() -> Option<Probe> {
    let text = std::fs::read_to_string(probe_path()?).ok()?;
    let field = |key: &str| -> Option<i64> {
        let pattern = format!("\"{key}\":");
        let start = text.find(&pattern)? + pattern.len();
        let rest = &text[start..];
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        rest[..end].trim().parse::<f64>().ok().map(|v| v as i64)
    };
    Some(Probe {
        x: field("x")?,
        y: field("y")?,
        mx: field("mx")?,
        my: field("my")?,
        width: field("width")?,
        height: field("height")?,
        dock: field("dock")?,
    })
}

async fn wait_probe(what: &str, within: Duration, ok: impl Fn(&Probe) -> bool) -> Probe {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Some(probe) = read_probe() {
            if ok(&probe) {
                return probe;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for {what}; last probe: {probe:?}");
            }
        } else if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for {what}; no probe file");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A Mutter remote-desktop session and its EIS socket (pointer + keyboard).
async fn mutter_eis() -> (zbus::Connection, OwnedObjectPath, OwnedFd) {
    let bus = zbus::Connection::session().await.expect("session bus");
    let reply = bus
        .call_method(
            Some(MUTTER_RD),
            MUTTER_RD_PATH,
            Some(MUTTER_RD),
            "CreateSession",
            &(),
        )
        .await
        .expect("Mutter RemoteDesktop.CreateSession");
    let session: OwnedObjectPath = reply.body().deserialize().expect("session path");
    bus.call_method(
        Some(MUTTER_RD),
        session.as_str(),
        Some(MUTTER_RD_SESSION),
        "Start",
        &(),
    )
    .await
    .expect("RemoteDesktop.Session.Start");
    let mut options: HashMap<&str, zvariant::Value> = HashMap::new();
    // keyboard (1) | pointer (2)
    options.insert("device-types", zvariant::Value::U32(3));
    let reply = bus
        .call_method(
            Some(MUTTER_RD),
            session.as_str(),
            Some(MUTTER_RD_SESSION),
            "ConnectToEIS",
            &(options,),
        )
        .await
        .expect("RemoteDesktop.Session.ConnectToEIS");
    let fd: zvariant::OwnedFd = reply.body().deserialize().expect("EIS fd");
    (bus, session, fd.into())
}

async fn stop(bus: &zbus::Connection, session: &OwnedObjectPath) {
    let _ = bus
        .call_method(
            Some(MUTTER_RD),
            session.as_str(),
            Some(MUTTER_RD_SESSION),
            "Stop",
            &(),
        )
        .await;
}

async fn motion(emulation: &mut InputEmulation, dx: f64, dy: f64) {
    emulation
        .consume(Event::Pointer(PointerEvent::Motion { time: 0, dx, dy }), 0)
        .await
        .expect("inject motion");
}

/// Wait until the backend has its EIS devices (the first motions after the
/// handshake may otherwise be dropped).
async fn wait_devices(emulation: &mut InputEmulation) {
    let before = read_probe().expect("probe");
    for _ in 0..200 {
        motion(emulation, 1.0, 0.0).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        if read_probe().is_some_and(|p| p.x != before.x || p.y != before.y) {
            return;
        }
    }
    panic!("EIS devices never moved the pointer");
}

/// Move the pointer (relative motion, no remote control) to `(x, y)`.
async fn walk_to(emulation: &mut InputEmulation, x: i64, y: i64) {
    for _ in 0..200 {
        let p = read_probe().expect("probe");
        let (dx, dy) = ((x - p.x) as f64, (y - p.y) as f64);
        if dx.abs() < 2.0 && dy.abs() < 2.0 {
            return;
        }
        // One axis at a time: a diagonal step out of a corner next to a
        // missing monitor area is clamped away entirely.
        let step = |d: f64| (d / 2.0).clamp(-40.0, 40.0);
        if dy.abs() >= 2.0 {
            motion(emulation, 0.0, step(dy)).await;
        } else {
            motion(emulation, step(dx), 0.0).await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "could not walk the pointer to ({x}, {y}): {:?}",
        read_probe()
    );
}

/// Regression for "under remote control the Ubuntu Dock does not open unless
/// the cursor is flung at the bottom": a peer drives this screen (absolute
/// tracker armed through the right edge), walks the pointer to the bottom
/// edge and keeps pushing like a user would. The auto-hidden dock has to
/// show, which needs real pressure on Mutter's barrier, not just a pointer
/// clamped onto the last row.
#[tokio::test(flavor = "current_thread")]
async fn pushing_against_the_bottom_edge_under_remote_control_opens_the_dock() {
    if probe_path().is_none() {
        eprintln!("skipped: run through scripts/gnome-headless.sh --test");
        return;
    }
    let _ = env_logger::builder().is_test(true).try_init();
    tokio::task::LocalSet::new()
        .run_until(async {
            let (bus, session, fd) = mutter_eis().await;
            let mut emulation = InputEmulation::libei_with_eis_fd(fd)
                .await
                .expect("libei on EIS");
            emulation.create(0).await;
            wait_devices(&mut emulation).await;

            // Setup, outside remote control: the dock starts shown and only
            // auto-hides once the pointer has hovered it and left. Walk over
            // it (bottom centre) and up to the middle of the screen.
            let screen = read_probe().expect("probe");
            let centre_x = screen.mx + screen.width / 2;
            let bottom = screen.my + screen.height - 1;
            walk_to(&mut emulation, centre_x, bottom - 20).await;
            walk_to(&mut emulation, centre_x, screen.my + screen.height / 2).await;
            let start = wait_probe("the dock to auto-hide", Duration::from_secs(10), |p| {
                p.dock == DOCK_HIDDEN
            })
            .await;

            // The peer enters through the right edge of the desktop,
            // mid-height, and walks left onto the dock's monitor (with a
            // second monitor on the right it crosses that one first).
            emulation
                .begin_remote_control(PointerEdge::Right, Some(0.5))
                .await;
            // the entry warp, before trusting the probe's x again
            motion(&mut emulation, -1.0, 0.0).await;
            wait_probe(
                "the pointer at the entry edge",
                Duration::from_secs(5),
                |p| p.x > centre_x + 8,
            )
            .await;
            for _ in 0..2000 {
                let p = read_probe().expect("probe");
                if p.x <= centre_x {
                    break;
                }
                motion(&mut emulation, -8.0, 0.0).await;
                tokio::time::sleep(Duration::from_millis(4)).await;
            }
            // Down to the bottom and keep pushing: ~2 s, 10 px per event,
            // well past the 100 px pressure threshold.
            let mut opened = None;
            for i in 0..250 {
                motion(&mut emulation, 0.0, 10.0).await;
                tokio::time::sleep(Duration::from_millis(8)).await;
                if let Some(p) = read_probe() {
                    if matches!(p.dock, DOCK_SHOWING | DOCK_SHOWN) {
                        opened = Some((i, p));
                        break;
                    }
                }
            }
            let last = read_probe().expect("probe");
            emulation.end_remote_control().await;
            emulation.terminate().await;
            stop(&bus, &session).await;

            assert_eq!(
                last.y,
                start.my + start.height - 1,
                "the pointer must reach the bottom row: {last:?}"
            );
            let (events, probe) = opened.unwrap_or_else(|| {
                panic!(
                    "the auto-hidden dock never opened while pushing against the bottom edge \
                     under remote control; last probe: {last:?} (screen {}x{})",
                    start.width, start.height
                )
            });
            eprintln!("dock opened after {events} pushes: {probe:?}");
        })
        .await;
}

/// The cursor-hiding extension's state files, in the headless session's
/// runtime dir.
fn cursor_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("runtime dir")).join("lan-mouse")
}

/// Whether the extension reports the cursor hidden (`None`: no status yet).
fn cursor_hidden() -> Option<bool> {
    let status = std::fs::read_to_string(cursor_dir().join("cursor-extension-status")).ok()?;
    if status.contains("tracker=0") {
        return None;
    }
    Some(status.contains("inhibited=1"))
}

async fn wait_cursor(hidden: bool, within: Duration) -> Duration {
    let started = std::time::Instant::now();
    while started.elapsed() < within {
        if cursor_hidden() == Some(hidden) {
            return started.elapsed();
        }
        tokio::time::sleep(Duration::from_micros(200)).await;
    }
    panic!(
        "the cursor did not become {} within {within:?} (status: {:?})",
        if hidden { "hidden" } else { "visible" },
        std::fs::read_to_string(cursor_dir().join("cursor-extension-status"))
    );
}

/// The return hand-off on the Ubuntu side, as the user feels it: the
/// keyboard and mouse come back from the peer (the service parks the cursor
/// with the "return" marker and drops the heartbeat) and the hand keeps
/// moving. The cursor must show with the first movement - not after a poll
/// tick or a distance threshold - and must stay hidden while nothing moves
/// (a border flap must not flash it).
#[tokio::test(flavor = "current_thread")]
async fn the_cursor_reappears_with_the_first_movement_after_a_return() {
    if probe_path().is_none() {
        eprintln!("skipped: run through scripts/gnome-headless.sh --test");
        return;
    }
    let _ = env_logger::builder().is_test(true).try_init();
    tokio::task::LocalSet::new()
        .run_until(async {
            let (bus, session, fd) = mutter_eis().await;
            let mut emulation = InputEmulation::libei_with_eis_fd(fd)
                .await
                .expect("libei on EIS");
            emulation.create(0).await;
            wait_devices(&mut emulation).await;
            let screen = read_probe().expect("probe");
            walk_to(
                &mut emulation,
                screen.mx + screen.width / 2,
                screen.my + screen.height / 2,
            )
            .await;

            let dir = cursor_dir();
            std::fs::create_dir_all(&dir).unwrap();
            let heartbeat = dir.join("cursor-hide");
            let park = dir.join("cursor-park");
            let mut hide_delays = Vec::new();
            let mut show_delays = Vec::new();
            for round in 0..10 {
                // driving the peer: heartbeat, cursor hidden
                std::fs::write(&heartbeat, b"1").unwrap();
                hide_delays.push(wait_cursor(true, Duration::from_secs(2)).await);

                // control comes back: park first, then the heartbeat goes
                std::fs::write(&park, b"return").unwrap();
                std::fs::remove_file(&heartbeat).unwrap();
                tokio::time::sleep(Duration::from_millis(120)).await;
                assert_eq!(
                    cursor_hidden(),
                    Some(true),
                    "round {round}: the cursor showed before the mouse moved"
                );

                // the hand moves: 3 px every 4 ms (a mouse at moderate speed)
                let started = std::time::Instant::now();
                let shown = loop {
                    motion(&mut emulation, if round % 2 == 0 { 3.0 } else { -3.0 }, 0.0).await;
                    let step = std::time::Instant::now();
                    let mut visible = false;
                    while step.elapsed() < Duration::from_millis(4) {
                        if cursor_hidden() == Some(false) {
                            visible = true;
                            break;
                        }
                        tokio::time::sleep(Duration::from_micros(200)).await;
                    }
                    if visible {
                        break started.elapsed();
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(2),
                        "round {round}: the cursor never showed while the mouse moved"
                    );
                };
                show_delays.push(shown);
                assert!(
                    !park.exists(),
                    "round {round}: the park marker was left behind"
                );
            }
            hide_delays.sort();
            show_delays.sort();
            eprintln!(
                "cursor hide after the heartbeat: median {:?}, worst {:?}",
                hide_delays[hide_delays.len() / 2],
                hide_delays.last().unwrap()
            );
            eprintln!(
                "cursor show after the first movement: median {:?}, worst {:?}",
                show_delays[show_delays.len() / 2],
                show_delays.last().unwrap()
            );
            let worst = *show_delays.last().unwrap();
            assert!(
                worst <= Duration::from_millis(15),
                "the cursor took up to {worst:?} to reappear after the mouse moved"
            );
            stop(&bus, &session).await;
        })
        .await;
}
