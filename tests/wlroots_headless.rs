//! End-to-end test of the wlroots emulation and layer-shell capture backends
//! against a real wlroots compositor (headless sway).
//!
//! Run through `scripts/sway-headless.sh`, which starts a throwaway headless
//! sway and sets `LAN_MOUSE_WLROOTS_HEADLESS=1`; without it the test returns
//! immediately, so a plain `cargo test` stays hermetic.
//!
//! The emulation backend (virtual pointer) plays the local user walking the
//! pointer to the left edge, where the layer-shell capture backend has put
//! its barrier surface: capture must begin, and the motion that follows must
//! come out of the capture as input for the peer instead of moving the
//! local pointer.
#![cfg(all(
    target_os = "linux",
    feature = "layer_shell_capture",
    feature = "wlroots_emulation"
))]

use std::time::Duration;

use futures::StreamExt;
use input_capture::{Backend as CaptureBackend, CaptureEvent, InputCapture, Position};
use input_emulation::{Backend as EmulationBackend, InputEmulation};
use input_event::{Event, PointerEvent};

async fn motion(emulation: &mut InputEmulation, dx: f64, dy: f64) {
    emulation
        .consume(Event::Pointer(PointerEvent::Motion { time: 0, dx, dy }), 0)
        .await
        .expect("inject motion");
}

#[tokio::test(flavor = "current_thread")]
async fn the_pointer_walked_to_the_edge_starts_capture_and_is_forwarded() {
    if std::env::var_os("LAN_MOUSE_WLROOTS_HEADLESS").is_none() {
        eprintln!("skipped: run through scripts/sway-headless.sh");
        return;
    }
    let _ = env_logger::builder().is_test(true).try_init();
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut capture = InputCapture::new(Some(CaptureBackend::LayerShell))
                .await
                .expect("layer-shell capture on sway");
            capture
                .create(0, Position::Left)
                .await
                .expect("left barrier");
            let mut emulation = InputEmulation::new(Some(EmulationBackend::Wlroots))
                .await
                .expect("wlroots emulation on sway");
            emulation.create(0).await;

            // Walk left until the barrier surface reports the crossing.
            let mut begun = false;
            for _ in 0..400 {
                motion(&mut emulation, -20.0, 0.0).await;
                match tokio::time::timeout(Duration::from_millis(10), capture.next()).await {
                    Ok(Some(Ok((0, CaptureEvent::Begin | CaptureEvent::BeginAt(_))))) => {
                        begun = true;
                        break;
                    }
                    Ok(Some(Err(e))) => panic!("capture error: {e}"),
                    Ok(None) => panic!("capture stream ended"),
                    _ => {}
                }
            }
            assert!(begun, "walking to the left edge never started capture");

            // While captured, local motion is input for the peer.
            let mut forwarded = false;
            for _ in 0..200 {
                motion(&mut emulation, -5.0, 3.0).await;
                if let Ok(Some(Ok((
                    0,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { .. })),
                )))) = tokio::time::timeout(Duration::from_millis(10), capture.next()).await
                {
                    forwarded = true;
                    break;
                }
            }
            assert!(forwarded, "motion during capture was not forwarded");

            capture.release().await.expect("release");
            capture.terminate().await.expect("terminate capture");
            emulation.terminate().await;
        })
        .await;
}
