//! Desk scenarios with OS quirks ([`Personality`]): the mixed Ubuntu/Windows
//! hand-off bugs (churn, ping-pong, re-entry loops) as automated checks.

use std::time::Duration;

use input_capture::Position;
use tokio::task::LocalSet;

use super::{Desk, Focus, Personality};

fn init_log() {
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
}

/// A (source, hand) with B to its right, each with an OS personality.
async fn desk(a: Personality, b: Personality) -> Desk {
    Desk::builder()
        .pc("A", 1920, 1080)
        .pc("B", 1920, 1080)
        .place("B", Position::Right, "A")
        .personality("A", a)
        .personality("B", b)
        .start()
        .await
}

/// The user stops right after crossing, then comes back:
///
/// 1. walk A's cursor to 4 px before the edge, cross by 8 px and stop;
/// 2. the cursor must stay on B (source backswing / receiver jitter must not
///    bounce it back);
/// 3. move back: the cursor returns to A once and stays there;
/// 4. A is never driven by B, and B's capture is not left grabbed.
async fn stop_right_after_crossing_then_return(desk: &Desk) {
    let (x, _) = desk.cursor("A");
    desk.move_mouse(1915.0 - x, 0.0).await;
    assert_eq!(desk.focus(), Focus::On("A"), "{}", desk.report());

    desk.move_mouse(8.0, 0.0).await;
    desk.wait_focus("B").await;
    desk.hold(
        "cursor rests on B right after the crossing",
        Duration::from_millis(600),
        |d| d.focus() == Focus::On("B"),
    )
    .await;

    desk.move_mouse(-300.0, 0.0).await;
    desk.wait_focus("A").await;
    desk.hold("cursor stays on A", Duration::from_millis(600), |d| {
        d.focus() == Focus::On("A")
    })
    .await;

    assert_eq!(desk.pc("A").capture_begins(), 1, "{}", desk.report());
    assert_eq!(
        desk.pc("A").remote_begins(),
        0,
        "B must never start driving A\n{}",
        desk.report()
    );
    desk.wait_until(
        "B's capture backend is not left grabbed",
        Duration::from_secs(2),
        |d| d.pc("B").state().captured.is_none(),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn gnome_to_windows_rests_at_the_border_without_churn() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk(Personality::gnome(), Personality::windows()).await;
            stop_right_after_crossing_then_return(&desk).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn windows_to_gnome_rests_at_the_border_without_churn() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk(Personality::windows(), Personality::gnome()).await;
            stop_right_after_crossing_then_return(&desk).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn gnome_to_gnome_rests_at_the_border_without_churn() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk(Personality::gnome(), Personality::gnome()).await;
            stop_right_after_crossing_then_return(&desk).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn windows_to_windows_rests_at_the_border_without_churn() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk(Personality::windows(), Personality::windows()).await;
            stop_right_after_crossing_then_return(&desk).await;
        })
        .await;
}

/// Mixed desk, several fast round trips: quirks must not accumulate state.
#[tokio::test(flavor = "current_thread")]
async fn mixed_round_trips_stay_clean() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk(Personality::gnome(), Personality::windows()).await;
            for round in 1..=4 {
                desk.move_mouse(1200.0, 0.0).await;
                desk.wait_focus("B").await;
                desk.move_mouse(200.0, 0.0).await;
                desk.move_mouse(-600.0, 0.0).await;
                desk.wait_focus("A").await;
                desk.move_mouse(-800.0, 0.0).await;
                assert_eq!(desk.pc("A").capture_begins(), round, "{}", desk.report());
                assert_eq!(desk.pc("A").remote_begins(), 0, "{}", desk.report());
            }
        })
        .await;
}

/// R-27: on a GNOME receiver the capture portal can report the return on the
/// `EnterOnly` barrier before the emulation's edge return. That take-back path
/// (`ICaptureEvent::CaptureBegin` -> `request_handoff` in `src/service.rs`)
/// used to carry no coordinate, so the source got the cursor back at its old
/// anchor instead of where it left the receiver.
///
/// It only shows after `REMOTE_ENTRY_DWELL` (300 ms, `src/capture.rs`): a
/// faster return is routed through the emulation's edge return, which always
/// kept the coordinate. Real users stay longer than that on the other PC.
#[tokio::test(flavor = "current_thread")]
async fn a_gnome_receiver_returns_with_the_coordinate() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = desk(Personality::windows(), Personality::gnome()).await;
            desk.move_mouse(1200.0, 0.0).await;
            desk.wait_focus("B").await;
            desk.move_mouse(200.0, 150.0).await;
            // stay on B past the remote-entry dwell, like a real user
            desk.hold("cursor on B", Duration::from_millis(500), |d| {
                d.focus() == Focus::On("B")
            })
            .await;
            // slow down and touch B's left edge without pushing past it: only
            // the capture portal reacts, the emulation never sees a crossing
            let (x, _) = desk.cursor("B");
            desk.move_mouse(1.0 - x, 0.0).await;
            desk.wait_focus("A").await;
            // A gets the cursor back on its right edge, at the height where it
            // touched B's left edge (B is the same size, so the same fraction)
            let (edge, along) = desk.pc("A").last_return().unwrap_or_else(|| {
                panic!(
                    "A got the cursor back without a return coordinate\n{}",
                    desk.report()
                )
            });
            assert_eq!(edge, Position::Right);
            let (_, y_on_b) = desk.cursor("B");
            assert!(
                (f64::from(along) * 1079.0 - y_on_b).abs() <= 2.0,
                "return height on A: along={along}, B's cursor at y={y_on_b}\n{}",
                desk.report()
            );
        })
        .await;
}
