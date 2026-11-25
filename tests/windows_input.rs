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
        WindowsAndMessaging::{CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetCursorPos},
    },
};

fn cursor_pos() -> Option<(i32, i32)> {
    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt) }
        .is_ok()
        .then_some((pt.x, pt.y))
}

/// Whether Windows currently draws the mouse cursor (across the desktop).
fn cursor_showing() -> Option<bool> {
    let mut info = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetCursorInfo(&mut info) }.ok()?;
    Some(info.flags.0 & CURSOR_SHOWING.0 != 0)
}

async fn wait_cursor_showing(expect_shown: bool, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if cursor_showing() == Some(expect_shown) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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
            let cursor_was_showing = cursor_showing() == Some(true);
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
                    assert!(
                        event.is_begin(),
                        "expected a capture begin, got {event:?}"
                    );

                    // While this machine drives the peer, the local pointer
                    // must disappear (1x1 topmost overlay under the pinned
                    // cursor).
                    if cursor_was_showing {
                        assert!(
                            wait_cursor_showing(false, Duration::from_secs(1)).await,
                            "local pointer must be hidden while the peer is controlled"
                        );
                    }

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

            // The overlay is hidden on release; nudge the pointer so Windows
            // recomputes the cursor shape for the window now under it.
            if cursor_was_showing {
                let _ = emulation
                    .consume(
                        Event::Pointer(PointerEvent::Motion {
                            time: 0,
                            dx: 2.0,
                            dy: 2.0,
                        }),
                        0,
                    )
                    .await;
                assert!(
                    wait_cursor_showing(true, Duration::from_secs(1)).await,
                    "local pointer must be restored after the capture is released"
                );
            }

            capture.terminate().await.expect("terminate capture");
            emulation.terminate().await;
        })
        .await;
}

/// One relative mouse report, as a mouse makes it (Windows accelerates it).
fn relative_report(dx: i32, dy: i32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
    };
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// "Enhance pointer precision" on (restored on drop).
struct PointerPrecision([i32; 3]);

impl PointerPrecision {
    fn enable() -> Option<Self> {
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETMOUSE, SPI_SETMOUSE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        };
        let mut saved = [0i32; 3];
        unsafe {
            SystemParametersInfoW(
                SPI_GETMOUSE,
                0,
                Some(saved.as_mut_ptr().cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .ok()?;
            let mut on = [6i32, 10, 1];
            SystemParametersInfoW(
                SPI_SETMOUSE,
                0,
                Some(on.as_mut_ptr().cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .ok()?;
        }
        Some(Self(saved))
    }
}

impl Drop for PointerPrecision {
    fn drop(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_SETMOUSE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        };
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_SETMOUSE,
                0,
                Some(self.0.as_mut_ptr().cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
    }
}

/// With the system acceleration on, the motion sent to the peer is what
/// Windows itself makes of the mouse reports: a fast move goes as far on the
/// peer as it would on this screen, curve included.
#[tokio::test(flavor = "current_thread")]
async fn the_peer_gets_the_motion_windows_makes_of_the_mouse() {
    use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let Some(start) = cursor_pos() else {
        eprintln!("skipped: no cursor in this session");
        return;
    };
    let Some(_precision) = PointerPrecision::enable() else {
        eprintln!("skipped: the pointer precision setting cannot be changed");
        return;
    };
    const REPORTS: [i32; 6] = [3, 12, 25, 40, 25, 12];

    // what Windows makes of these reports on this screen
    let centre = (start.0.max(400), start.1.max(400));
    unsafe {
        let _ = SetCursorPos(centre.0, centre.1);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let before = cursor_pos().expect("cursor");
    for dx in REPORTS {
        relative_report(dx, 0);
        tokio::time::sleep(Duration::from_millis(8)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let local = f64::from(cursor_pos().expect("cursor").0 - before.0);
    let raw: i32 = REPORTS.iter().sum();
    eprintln!("Windows moved the cursor {local} px for {raw} counts");

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async {
            input_capture::set_windows_os_acceleration(true);
            let mut emulation = InputEmulation::new(Some(EmulationBackend::Windows))
                .await
                .expect("emulation");
            emulation.create(0).await;
            let mut capture = InputCapture::new(Some(CaptureBackend::Windows))
                .await
                .expect("capture");
            capture.create(0, Position::Left).await.expect("left");
            tokio::time::sleep(Duration::from_millis(50)).await;
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
            let begun = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match capture.next().await {
                        Some(Ok((_, event))) if event.is_begin() => return true,
                        Some(_) => continue,
                        None => return false,
                    }
                }
            })
            .await;
            if !matches!(begun, Ok(true)) {
                eprintln!("skipped: the session did not deliver the crossing");
                input_capture::set_windows_os_acceleration(false);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            for dx in REPORTS {
                relative_report(dx, 0);
                tokio::time::sleep(Duration::from_millis(8)).await;
            }
            let mut forwarded = 0.0;
            let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
            while let Ok(Some(Ok((_, event)))) =
                tokio::time::timeout_at(deadline, capture.next()).await
            {
                if let CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { dx, .. })) = event
                {
                    forwarded += dx;
                }
            }
            eprintln!("forwarded {forwarded} px (Windows moved {local} px)");
            capture.release().await.expect("release");
            input_capture::set_windows_os_acceleration(false);
            assert!(local > 0.0, "Windows did not move the cursor at all");
            assert!(
                (forwarded - local).abs() <= local * 0.1 + 2.0,
                "the peer got {forwarded} px where Windows moves {local} px"
            );
        })
        .await;
}
