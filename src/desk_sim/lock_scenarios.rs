//! Caps Lock / Num Lock sync between real services on virtual PCs.
//!
//! Each simulated PC keeps its lock state like an OS would: a lock key typed
//! on its own keyboard toggles it, and so does a lock key injected by the
//! emulation backend. The service reads that state through the same lock
//! reader hook the OS readers plug into.

use std::time::Duration;

use input_capture::Position;
use lan_mouse_proto::lock_bits::{CAPS, NUM};
use tokio::task::LocalSet;

use super::Desk;
use crate::lock_sync::{KEY_CAPSLOCK, KEY_NUMLOCK};

fn init_log() {
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
}

/// A (holding the mouse) with B on its right, connected both ways.
async fn desk_with(configure: impl FnOnce(super::DeskBuilder) -> super::DeskBuilder) -> Desk {
    let builder = Desk::builder()
        .pc("A", 1920, 1080)
        .pc("B", 1920, 1080)
        .place("B", Position::Right, "A")
        .place("A", Position::Left, "B");
    configure(builder).start().await
}

/// Lock polls every PC runs before a check: a change is read and sent in
/// one poll, applied by the peer, read back there in the next and, if it
/// bounced, re-sent and applied again. Anything still bouncing after this
/// many polls shows up as extra injected taps.
const SETTLE_POLLS: usize = 6;

async fn settle(desk: &Desk) {
    desk.settle_lock_polls(SETTLE_POLLS).await;
}

async fn wait_locks(desk: &Desk, name: &'static str, bits: u8, on: u8) {
    desk.wait_until(
        &format!("{name} locks {bits:#04b} == {on:#04b}"),
        Duration::from_secs(5),
        |desk| desk.pc(name).locks() & bits == on,
    )
    .await;
}

fn taps(desk: &Desk, name: &str) -> usize {
    desk.pc(name).state().injected_lock_taps
}

#[tokio::test(flavor = "current_thread")]
async fn caps_lock_on_one_pc_lights_the_other_and_back() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk_with(|b| b).await;
            // Let both services take their baseline reading.
            settle(&desk).await;

            desk.key_tap_on("A", KEY_CAPSLOCK).await;
            assert_eq!(desk.pc("A").locks() & CAPS, CAPS, "the local key toggles A");
            wait_locks(&desk, "B", CAPS, CAPS).await;
            settle(&desk).await;
            assert_eq!(desk.pc("A").locks() & CAPS, CAPS, "{}", desk.report());
            assert_eq!(desk.pc("B").locks() & CAPS, CAPS, "{}", desk.report());
            assert_eq!(taps(&desk, "B"), 1, "B toggled exactly once");
            assert_eq!(taps(&desk, "A"), 0, "B's echo must not bounce back to A");

            desk.key_tap_on("B", KEY_CAPSLOCK).await;
            wait_locks(&desk, "A", CAPS, 0).await;
            settle(&desk).await;
            assert_eq!(desk.pc("B").locks() & CAPS, 0);
            assert_eq!(taps(&desk, "A"), 1);
            assert_eq!(taps(&desk, "B"), 1, "no extra toggle on B");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn num_lock_syncs_both_ways_without_touching_caps_lock() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk_with(|b| b).await;
            settle(&desk).await;

            desk.key_tap_on("B", KEY_NUMLOCK).await;
            wait_locks(&desk, "A", NUM, NUM).await;
            // let A's service read the synced state before the user acts
            settle(&desk).await;
            desk.key_tap_on("A", KEY_NUMLOCK).await;
            wait_locks(&desk, "B", NUM, 0).await;
            settle(&desk).await;
            for name in ["A", "B"] {
                assert_eq!(desk.pc(name).locks(), 0, "{name}: {}", desk.report());
            }
            assert_eq!(taps(&desk, "A") + taps(&desk, "B"), 2);
        })
        .await;
}

/// While A controls B, Caps Lock typed on A's keyboard goes to B (the
/// capture grabs it, A's OS never sees it). B's lock changes, and A must
/// follow so the LED of the keyboard in use is right.
#[tokio::test(flavor = "current_thread")]
async fn a_lock_key_typed_while_controlling_the_peer_lights_both_keyboards() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk_with(|b| b).await;
            settle(&desk).await;
            desk.move_mouse(1200.0, 0.0).await;
            desk.wait_focus("B").await;

            desk.key_tap_on("A", KEY_CAPSLOCK).await;
            wait_locks(&desk, "B", CAPS, CAPS).await;
            wait_locks(&desk, "A", CAPS, CAPS).await;
            settle(&desk).await;
            assert_eq!(desk.pc("A").locks() & CAPS, CAPS, "{}", desk.report());
            assert_eq!(desk.pc("B").locks() & CAPS, CAPS, "{}", desk.report());
            // the forwarded key on B, the synced toggle on A, nothing more
            assert_eq!(taps(&desk, "B"), 1, "{}", desk.report());
            assert_eq!(taps(&desk, "A"), 1, "{}", desk.report());
        })
        .await;
}

/// The keyboard the user types on wins the hand-off: A starts with Caps Lock
/// on and B with it off (they were never synced); moving the mouse to B
/// switches B's on.
#[tokio::test(flavor = "current_thread")]
async fn the_controlling_keyboard_wins_the_handoff() {
    init_log();
    LocalSet::new()
        .run_until(async {
            // Diverged before either service took a reading: a baseline is
            // not a change, so nothing is broadcast.
            let desk = desk_with(|b| b.locks("A", CAPS)).await;
            settle(&desk).await;
            assert_eq!(desk.pc("B").locks(), 0, "a baseline is not a change");

            desk.move_mouse(1200.0, 0.0).await;
            desk.wait_focus("B").await;
            wait_locks(&desk, "B", CAPS, CAPS).await;

            // Matching locks: further hand-offs inject nothing.
            let before = taps(&desk, "A") + taps(&desk, "B");
            for _ in 0..3 {
                desk.move_mouse(-1000.0, 0.0).await;
                desk.wait_focus("A").await;
                desk.move_mouse(1000.0, 0.0).await;
                desk.wait_focus("B").await;
            }
            settle(&desk).await;
            assert_eq!(
                taps(&desk, "A") + taps(&desk, "B"),
                before,
                "{}",
                desk.report()
            );
        })
        .await;
}

/// `sync_num_lock = false` on A: A neither applies nor sends Num Lock, while
/// Caps Lock keeps syncing.
#[tokio::test(flavor = "current_thread")]
async fn num_lock_stays_local_when_its_sync_is_disabled() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk_with(|b| b.top_config("A", "sync_num_lock = false")).await;
            settle(&desk).await;

            desk.key_tap_on("B", KEY_NUMLOCK).await;
            desk.key_tap_on("A", KEY_CAPSLOCK).await;
            wait_locks(&desk, "B", CAPS, CAPS).await;
            settle(&desk).await;
            assert_eq!(desk.pc("A").locks() & NUM, 0, "A ignores B's Num Lock");

            desk.key_tap_on("A", KEY_NUMLOCK).await;
            settle(&desk).await;
            assert_eq!(
                desk.pc("B").locks() & NUM,
                NUM,
                "A does not send its Num Lock: B keeps its own"
            );
        })
        .await;
}
