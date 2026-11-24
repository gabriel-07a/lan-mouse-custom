//! Moving the keyboard and mouse without crossing an edge: `GoTo` (what
//! `lan-mouse cli go/home` and the GUI send) and the switch hotkeys typed
//! while driving a peer.
//!
//! The simulated capture backend, like GNOME's, can only start a capture at
//! a barrier, so these runs also exercise the pointer walk: the emulation
//! pushes the local pointer into the target's edge and the barrier there
//! starts the capture.

use std::time::Duration;

use input_capture::Position;
use lan_mouse_ipc::{ClientHandle, FrontendRequest, connect_async_at};
use tokio::task::LocalSet;

use super::{Desk, Focus};

const KEY_RIGHTALT: u32 = 100;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_1: u32 = 2;

fn init_log() {
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
}

/// C | A | B: A in the middle with B on its right and C on its left.
async fn three_in_a_row() -> Desk {
    Desk::builder()
        .pc("A", 1920, 1080)
        .pc("B", 1920, 1080)
        .pc("C", 1920, 1080)
        .place("B", Position::Right, "A")
        .place("C", Position::Left, "A")
        .place("A", Position::Left, "B")
        .place("A", Position::Right, "C")
        .start()
        .await
}

/// A's client handle for the PC `name`.
fn handle_on(desk: &Desk, on: &str, name: &str) -> ClientHandle {
    let port = desk.pc(name).port;
    desk.pc(on)
        .clients
        .get_client_states()
        .into_iter()
        .find(|(_, config, _)| config.port == port)
        .map(|(handle, _, _)| handle)
        .unwrap_or_else(|| panic!("{on} has no client for {name}"))
}

async fn wait_connected(desk: &Desk) {
    desk.wait_until("A connected to B and C", Duration::from_secs(10), |desk| {
        desk.pc("A")
            .clients
            .get_client_states()
            .iter()
            .filter(|(_, _, state)| state.active_addr.is_some())
            .count()
            == 2
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn go_to_moves_the_keyboard_there_and_home_brings_it_back() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = three_in_a_row().await;
            wait_connected(&desk).await;
            let (_events, mut requests) =
                connect_async_at(&desk.pc("A").ipc_socket(), Some(Duration::from_secs(5)))
                    .await
                    .expect("ipc");

            // The pointer starts in the middle of A: no edge was crossed.
            requests
                .request(FrontendRequest::GoTo(Some(handle_on(&desk, "A", "C"))))
                .await
                .expect("go to C");
            desk.wait_focus("C").await;

            // From C straight to B: the capture moves without going home first.
            requests
                .request(FrontendRequest::GoTo(Some(handle_on(&desk, "A", "B"))))
                .await
                .expect("go to B");
            desk.wait_focus("B").await;

            requests
                .request(FrontendRequest::GoTo(None))
                .await
                .expect("home");
            desk.wait_focus("A").await;
            assert_eq!(desk.focus(), Focus::On("A"));
        })
        .await;
}

/// Our own pointer walk must not reach the peer as the user's motion: the
/// push that is still in flight when the capture starts is dropped.
#[tokio::test(flavor = "current_thread")]
async fn the_pointer_walk_does_not_leak_into_the_peer() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = three_in_a_row().await;
            wait_connected(&desk).await;
            let (_events, mut requests) =
                connect_async_at(&desk.pc("A").ipc_socket(), Some(Duration::from_secs(5)))
                    .await
                    .expect("ipc");
            requests
                .request(FrontendRequest::GoTo(Some(handle_on(&desk, "A", "B"))))
                .await
                .expect("go to B");
            desk.wait_focus("B").await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            // B is entered through its left edge; every leaked push step
            // would have moved its cursor 30 px further right.
            let (x, _) = desk.cursor("B");
            assert!(
                x < 10.0,
                "the walk kept moving B's cursor after the entry (x = {x})\n{}",
                desk.report()
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn altgr_right_shift_goes_to_the_next_pc_and_then_home() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = three_in_a_row().await;
            wait_connected(&desk).await;
            let order = {
                let mut handles = [handle_on(&desk, "A", "B"), handle_on(&desk, "A", "C")];
                handles.sort();
                handles
            };
            let name_of = |handle| {
                if handle == handle_on(&desk, "A", "B") {
                    "B"
                } else {
                    "C"
                }
            };
            let (first, second) = (name_of(order[0]), name_of(order[1]));

            // Drive the first PC by crossing its edge.
            let dx = if first == "B" { 1200.0 } else { -1200.0 };
            desk.move_mouse(dx, 0.0).await;
            desk.wait_focus(first).await;

            // AltGr + Right Shift: the next PC.
            desk.key_chord_on("A", &[KEY_RIGHTALT, KEY_RIGHTSHIFT])
                .await;
            desk.wait_focus(second).await;
            // After the last PC: home.
            desk.key_chord_on("A", &[KEY_RIGHTALT, KEY_RIGHTSHIFT])
                .await;
            desk.wait_focus("A").await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn altgr_right_shift_and_a_digit_pick_that_pc_without_typing_it() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = three_in_a_row().await;
            wait_connected(&desk).await;
            let mut handles = [handle_on(&desk, "A", "B"), handle_on(&desk, "A", "C")];
            handles.sort();
            let second = if handles[1] == handle_on(&desk, "A", "B") {
                "B"
            } else {
                "C"
            };
            let first = if second == "B" { "C" } else { "B" };

            let dx = if first == "B" { 1200.0 } else { -1200.0 };
            desk.move_mouse(dx, 0.0).await;
            desk.wait_focus(first).await;
            let typed_before = desk.pc(first).state().injected_other.len();

            desk.key_chord_on("A", &[KEY_RIGHTALT, KEY_RIGHTSHIFT, KEY_1 + 1])
                .await;
            desk.wait_focus(second).await;
            // Neither PC saw the "2" go down or up: the press would have
            // reached the first, the release the second.
            let typed_after = desk.pc(second).state().injected_other.len();
            tokio::time::sleep(Duration::from_millis(200)).await;
            for (name, from) in [(first, typed_before), (second, 0)] {
                let typed: Vec<_> = desk.pc(name).state().injected_other[from..].to_vec();
                assert!(
                    typed.iter().all(|event| !matches!(
                        event,
                        input_event::Event::Keyboard(input_event::KeyboardEvent::Key { key, .. })
                            if *key == KEY_1 + 1
                    )),
                    "the digit of the hotkey was typed on {name}: {typed:?}"
                );
            }
            let _ = typed_after;

            // + 0: home.
            desk.key_chord_on("A", &[KEY_RIGHTALT, KEY_RIGHTSHIFT, 11])
                .await;
            desk.wait_focus("A").await;
        })
        .await;
}
