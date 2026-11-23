//! Manual check of desktop edge pressure under remote control (Linux, libei).
//!
//! Plays a peer that enters this machine through its right edge, moves inside
//! and then pushes the pointer into the bottom edge for about two seconds,
//! using the real libei emulation backend and its absolute tracker - the path
//! taken when another PC drives this one. On Ubuntu with the auto-hiding dock
//! the dock must appear; the `edge pressure: ... replay Sent` debug line says
//! the push reached the compositor as relative motion.
//!
//! It moves the real cursor; do not touch the mouse while it runs:
//!
//!     cargo run --example edge_pressure_probe
use input_emulation::{InputEmulation, PointerEdge};
use input_event::{Event, PointerEvent};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::Builder::new()
        .parse_filters("input_emulation=debug")
        .init();
    let mut emulation = InputEmulation::new(None).await.expect("emulation backend");
    emulation.create(0).await;
    emulation
        .begin_remote_control(PointerEdge::Right, Some(0.5))
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let motion = |dx: f64, dy: f64| Event::Pointer(PointerEvent::Motion { time: 0, dx, dy });
    // move well inside the screen (towards the dock's x range)
    for _ in 0..90 {
        emulation
            .consume(motion(-8.0, 0.0), 0)
            .await
            .expect("motion");
        tokio::time::sleep(Duration::from_millis(8)).await;
    }
    // push down into the bottom edge for ~2 s, like a hand holding the push
    for _ in 0..250 {
        emulation
            .consume(motion(0.0, 10.0), 0)
            .await
            .expect("motion");
        tokio::time::sleep(Duration::from_millis(8)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    emulation.end_remote_control().await;
    emulation.terminate().await;
}
