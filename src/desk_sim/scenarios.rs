//! Desk scenarios: the hand-off behaviour a person checks by moving the mouse.
//!
//! Each test builds a desk, moves the simulated mouse and asserts what a
//! person would look at: which screen shows the cursor and where.

use std::time::Duration;

use input_capture::Position;
use tokio::task::LocalSet;

use super::{Desk, Focus, Hand};

fn init_log() {
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
}

/// A (1920x1080) with B (2560x1440) to its right.
async fn a_with_b_on_the_right() -> Desk {
    Desk::builder()
        .pc("A", 1920, 1080)
        .pc("B", 2560, 1440)
        .place("B", Position::Right, "A")
        .start()
        .await
}

fn assert_near(actual: f64, expected: f64, what: &str, desk: &Desk) {
    assert!(
        (actual - expected).abs() <= 2.0,
        "{what}: expected ~{expected}, got {actual}\n{}",
        desk.report()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cross_to_the_right_and_back() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_with_b_on_the_right().await;
            assert_eq!(desk.focus(), Focus::On("A"));

            // A's cursor starts at the centre (960, 540): push it past the
            // right edge
            desk.move_mouse(1200.0, 0.0).await;
            desk.wait_focus("B").await;
            let (x, y) = desk.cursor("B");
            assert!(x < 300.0, "B's cursor must enter from the left: x={x}");
            // crossed at 540/1079 of A's height -> same fraction of B's 1440
            assert_near(y, 719.5, "entry height on B", &desk);

            // move around on B, then come back through B's left edge
            desk.move_mouse(300.0, 200.0).await;
            desk.move_mouse(-1000.0, 0.0).await;
            desk.wait_focus("A").await;

            // A got the cursor back on its right edge, at the height where B's
            // cursor left (motion is asynchronous: compare with what B
            // reported at the edge, not with a cursor read mid-flight)
            let (edge, along) = desk.pc("A").last_return().unwrap_or_else(|| {
                panic!("A released without a return coordinate\n{}", desk.report())
            });
            assert_eq!(edge, Position::Right);
            let left_b_at = desk
                .pc("B")
                .last_edge_return()
                .expect("B requested the return");
            assert_near(
                f64::from(along) * 1079.0,
                f64::from(left_b_at) * 1079.0,
                "return height on A",
                &desk,
            );
            // and it is below the entry height: the hand moved down on B
            assert!(along > 0.6, "return must follow the motion on B: {along}");

            // the rest of the swipe kept moving A's cursor to the left; it must
            // stay on A
            desk.hold("cursor stays on A", Duration::from_millis(500), |d| {
                d.focus() == Focus::On("A")
            })
            .await;
            assert_eq!(desk.pc("A").capture_begins(), 1, "{}", desk.report());
            assert_eq!(desk.pc("B").remote_begins(), 1, "{}", desk.report());
        })
        .await;
}

/// Several round trips in a row: leftovers of one hand-off (serials, guards,
/// pending returns) must not break the next one.
#[tokio::test(flavor = "current_thread")]
async fn repeated_round_trips_stay_clean() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_with_b_on_the_right().await;
            for round in 1..=5 {
                desk.move_mouse(1200.0, 0.0).await;
                desk.wait_focus("B").await;
                desk.move_mouse(200.0, 0.0).await;
                desk.move_mouse(-600.0, 0.0).await;
                desk.wait_focus("A").await;
                // back to the middle of A for the next round
                desk.move_mouse(-800.0, 0.0).await;
                assert_eq!(desk.pc("A").capture_begins(), round, "{}", desk.report());
                assert_eq!(desk.pc("B").remote_begins(), round, "{}", desk.report());
            }
        })
        .await;
}

/// Vertical layout, slow hand: the same contract on the top/bottom edges.
#[tokio::test(flavor = "current_thread")]
async fn cross_upwards_and_back_with_a_slow_hand() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = Desk::builder()
                .pc("A", 1920, 1080)
                .pc("B", 1280, 720)
                .place("B", Position::Top, "A")
                .hand(Hand {
                    step_px: 1.0,
                    interval: Duration::from_millis(1),
                })
                .start()
                .await;

            desk.move_mouse(0.0, -600.0).await;
            desk.wait_focus("B").await;
            let (x, y) = desk.cursor("B");
            assert!(y > 600.0, "B's cursor must enter from the bottom: y={y}");
            assert_near(x, 639.5, "entry column on B", &desk);

            desk.move_mouse(0.0, 200.0).await;
            desk.wait_focus("A").await;
            assert_eq!(
                desk.pc("A").last_return().map(|(edge, _)| edge),
                Some(Position::Top)
            );
        })
        .await;
}

/// Three PCs in a row, A | B | C, where A and C both drive B, one after the
/// other. Each return must go back to the PC that is driving, and the other
/// one must not be pulled into the hand-off.
///
/// Note: B only keeps the session that currently drives it, so the return is
/// routed to that session even if the reported edge were wrong (see
/// `service_core::return_targets`). Swapping the edge in the emulation task
/// does not fail this test, and that is the intended behaviour.
#[tokio::test(flavor = "current_thread")]
async fn the_return_goes_to_the_peer_that_drives() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = Desk::builder()
                .pc("A", 1920, 1080)
                .pc("B", 1920, 1080)
                .pc("C", 1920, 1080)
                .place("B", Position::Right, "A")
                .place("B", Position::Left, "C")
                .start()
                .await;

            // C drives B through B's right edge, then returns
            desk.move_mouse_on("C", -1200.0, 0.0).await;
            desk.wait_focus_from("C", "B").await;
            desk.move_mouse_on("C", 300.0, 0.0).await;
            desk.wait_focus_from("C", "C").await;

            // now A drives B through B's left edge and returns through it
            desk.move_mouse_on("A", 1200.0, 0.0).await;
            desk.wait_focus_from("A", "B").await;
            desk.move_mouse_on("A", -300.0, 0.0).await;
            desk.wait_focus_from("A", "A").await;

            // C was never pulled into the second hand-off
            assert_eq!(desk.pc("C").capture_begins(), 1, "{}", desk.report());
            assert_eq!(desk.focus_from("C"), Focus::On("C"));
        })
        .await;
}
