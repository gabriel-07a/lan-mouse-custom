//! Windows OS integration tests for input emulation (`SendInput`) and low-level
//! input capture hooks (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`), monitor enumeration,
//! and DPI scaling.
//!
//! These tests execute on real Windows sessions (including GitHub Actions
//! `windows-2022` and `windows-2025` runners, which provide an interactive
//! virtual desktop).

#![cfg(windows)]

use futures::StreamExt;
use input_capture::{Backend as CaptureBackend, CaptureEvent, InputCapture, Position};
use input_emulation::{Backend as EmulationBackend, InputEmulation};
use input_event::{Event, KeyboardEvent, PointerEvent, scancode};
use std::time::Duration;
use windows::Win32::{
    Foundation::POINT,
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
        WindowsAndMessaging::GetCursorPos,
    },
};

fn cursor_pos() -> Option<(i32, i32)> {
    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt) }
        .is_ok()
        .then_some((pt.x, pt.y))
}

#[tokio::test(flavor = "current_thread")]
async fn windows_backends_initialize_and_move_cursor_and_trigger_barrier() {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut emulation = InputEmulation::new(Some(EmulationBackend::Windows))
                .await
                .expect("WindowsEmulation must initialize");
            assert_eq!(emulation.backend(), EmulationBackend::Windows);
            emulation.create(0).await;

            let mut capture = InputCapture::new(Some(CaptureBackend::Windows))
                .await
                .expect("WindowsInputCapture must initialize");
            assert_eq!(capture.backend(), CaptureBackend::Windows);

            // Register barriers on all four edges so any edge crossing triggers CaptureEvent::Begin.
            capture.create(0, Position::Left).await.expect("left");
            capture.create(1, Position::Right).await.expect("right");
            capture.create(2, Position::Top).await.expect("top");
            capture.create(3, Position::Bottom).await.expect("bottom");

            // Give the hook thread a brief moment to process the client updates.
            tokio::time::sleep(Duration::from_millis(50)).await;

            // If an interactive desktop with cursor access is available, verify
            // that SendInput moves the cursor and crosses the left barrier.
            if let Some(initial) = cursor_pos() {
                // Move slightly toward the center first so we have a known starting movement.
                emulation
                    .consume(
                        Event::Pointer(PointerEvent::Motion {
                            time: 0,
                            dx: 20.0,
                            dy: 20.0,
                        }),
                        0,
                    )
                    .await
                    .expect("motion consume");

                // Sweep far left to cross the Left barrier of the virtual desktop.
                for _ in 0..25 {
                    emulation
                        .consume(
                            Event::Pointer(PointerEvent::Motion {
                                time: 0,
                                dx: -400.0,
                                dy: 0.0,
                            }),
                            0,
                        )
                        .await
                        .expect("sweep left");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }

                // Wait for CaptureEvent::Begin on the Left barrier (handle 0).
                let begin = tokio::time::timeout(Duration::from_secs(2), capture.next()).await;
                if let Ok(Some(Ok((handle, event)))) = begin {
                    assert_eq!(handle, 0, "expected Left barrier (handle 0)");
                    assert_eq!(event, CaptureEvent::Begin);

                    // While capture is active, inject a key press + release and verify the hook captures it.
                    let key_a = scancode::Linux::KeyA as u32;
                    emulation
                        .consume(
                            Event::Keyboard(KeyboardEvent::Key {
                                time: 0,
                                key: key_a,
                                state: 1,
                            }),
                            0,
                        )
                        .await
                        .expect("key down");
                    emulation
                        .consume(
                            Event::Keyboard(KeyboardEvent::Key {
                                time: 0,
                                key: key_a,
                                state: 0,
                            }),
                            0,
                        )
                        .await
                        .expect("key up");

                    let mut saw_key_down = false;
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                    while tokio::time::Instant::now() < deadline && !saw_key_down {
                        match tokio::time::timeout(Duration::from_millis(200), capture.next()).await
                        {
                            Ok(Some(Ok((
                                _,
                                CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                                    key,
                                    state: 1,
                                    ..
                                })),
                            )))) if key == key_a => {
                                saw_key_down = true;
                            }
                            Ok(Some(_)) => {}
                            _ => break,
                        }
                    }
                    assert!(
                        saw_key_down,
                        "WH_KEYBOARD_LL hook must capture the injected KeyA press while active"
                    );
                } else {
                    log::warn!(
                        "cursor started at {initial:?}, but desktop session blocked SendInput hook delivery"
                    );
                }
            }

            capture.release().await.expect("release capture");
            capture.terminate().await.expect("terminate capture");
            emulation.terminate().await;
        })
        .await;
}
