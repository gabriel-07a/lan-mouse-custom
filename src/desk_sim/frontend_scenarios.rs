//! The requests the GUI sends, one by one, through the real IPC to a real
//! service, checking what each one actually changes: the device card's side,
//! sensitivity, name and port, the device list, remote-control speed,
//! pairing, address rules and the enter/leave commands. A request the
//! service silently ignored would leave a GUI control that does nothing.

use std::time::Duration;

use input_capture::Position;
use lan_mouse_ipc::{
    AsyncFrontendEventReader, AsyncFrontendRequestWriter, ClientHandle, FrontendEvent,
    FrontendRequest, connect_async_at,
};
use tokio::task::LocalSet;

use super::{Desk, Focus};

fn init_log() {
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
}

/// A with B on its right, connected both ways.
async fn a_and_b() -> Desk {
    Desk::builder()
        .pc("A", 1920, 1080)
        .pc("B", 1920, 1080)
        .place("B", Position::Right, "A")
        .place("A", Position::Left, "B")
        .start()
        .await
}

async fn frontend(
    desk: &Desk,
    name: &str,
) -> (AsyncFrontendEventReader, AsyncFrontendRequestWriter) {
    connect_async_at(&desk.pc(name).ipc_socket(), Some(Duration::from_secs(5)))
        .await
        .unwrap_or_else(|e| panic!("desk-sim: no IPC to {name}: {e}"))
}

/// A's client handle for B.
fn b_on_a(desk: &Desk) -> ClientHandle {
    let port = desk.pc("B").port;
    desk.pc("A")
        .clients
        .get_client_states()
        .into_iter()
        .find(|(_, config, _)| config.port == port)
        .map(|(handle, _, _)| handle)
        .expect("A has a client for B")
}

/// The next event that `want` accepts (others are skipped).
async fn expect_event<T>(
    events: &mut AsyncFrontendEventReader,
    what: &str,
    want: impl Fn(&FrontendEvent) -> Option<T>,
) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match events.next_event(Some(left)).await.expect("ipc") {
            Some(event) => {
                if let Some(found) = want(&event) {
                    return found;
                }
            }
            None => panic!("desk-sim: no event for: {what}"),
        }
    }
}

async fn send(requests: &mut AsyncFrontendRequestWriter, request: FrontendRequest) {
    requests.request(request).await.expect("ipc request");
}

/// Moving B to A's left in the card changes where the mouse crosses.
#[tokio::test(flavor = "current_thread")]
async fn changing_the_side_moves_the_edge_the_mouse_crosses() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_and_b().await;
            let (_events, mut requests) = frontend(&desk, "A").await;
            let b = b_on_a(&desk);
            desk.wait_until("A connected to B", Duration::from_secs(10), |desk| {
                desk.pc("A").clients.active_addr(b).is_some()
            })
            .await;

            send(
                &mut requests,
                FrontendRequest::UpdatePosition(b, lan_mouse_ipc::Position::Left),
            )
            .await;
            desk.wait_until("B is on A's left", Duration::from_secs(5), |desk| {
                desk.pc("A")
                    .clients
                    .get_client_states()
                    .iter()
                    .any(|(h, c, _)| *h == b && c.pos == lan_mouse_ipc::Position::Left)
            })
            .await;

            // the right edge leads nowhere now
            desk.move_mouse(1200.0, 0.0).await;
            desk.hold(
                "A keeps the mouse at its right edge",
                Duration::from_millis(300),
                |d| d.focus() == Focus::On("A"),
            )
            .await;
            // the left one leads to B
            desk.move_mouse(-2500.0, 0.0).await;
            desk.wait_focus("B").await;
        })
        .await;
}

/// Sensitivity, name and port typed on the card end up in the client.
#[tokio::test(flavor = "current_thread")]
async fn sensitivity_name_and_port_edits_are_stored() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_and_b().await;
            let (mut events, mut requests) = frontend(&desk, "A").await;
            let b = b_on_a(&desk);

            send(&mut requests, FrontendRequest::UpdateSensitivity(b, 175)).await;
            send(
                &mut requests,
                FrontendRequest::UpdateHostname(b, Some("pc-da-sala".into())),
            )
            .await;
            let port = desk.pc("B").port;
            send(&mut requests, FrontendRequest::UpdatePort(b, port)).await;
            send(
                &mut requests,
                FrontendRequest::UpdateEnterHook(b, Some("notify-send entrou".into())),
            )
            .await;
            send(
                &mut requests,
                FrontendRequest::UpdateLeaveHook(b, Some("notify-send saiu".into())),
            )
            .await;

            let config = expect_event(
                &mut events,
                "B's card after the edits",
                |event| match event {
                    FrontendEvent::State(handle, config, _)
                        if *handle == b && config.leave_cmd.is_some() =>
                    {
                        Some(config.clone())
                    }
                    _ => None,
                },
            )
            .await;
            assert_eq!(config.sensitivity, 175);
            assert_eq!(config.hostname.as_deref(), Some("pc-da-sala"));
            assert_eq!(config.port, port);
            assert_eq!(config.cmd.as_deref(), Some("notify-send entrou"));
            assert_eq!(config.leave_cmd.as_deref(), Some("notify-send saiu"));

            // out-of-range sensitivity is clamped, never stored as typed
            send(&mut requests, FrontendRequest::UpdateSensitivity(b, 9999)).await;
            let clamped = expect_event(&mut events, "clamped sensitivity", |event| match event {
                FrontendEvent::State(handle, config, _)
                    if *handle == b && config.sensitivity != 175 =>
                {
                    Some(config.sensitivity)
                }
                _ => None,
            })
            .await;
            assert!(clamped <= 500, "stored {clamped}");
        })
        .await;
}

/// Renaming a device is display only: the list shows the new name, the
/// identity (fingerprint) stays.
#[tokio::test(flavor = "current_thread")]
async fn renaming_a_device_changes_the_name_in_the_list() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_and_b().await;
            let (mut events, mut requests) = frontend(&desk, "B").await;
            // B knows A once A connected to it
            desk.move_mouse(1200.0, 0.0).await;
            desk.wait_focus("B").await;
            send(&mut requests, FrontendRequest::ListDevices).await;
            let device = expect_event(&mut events, "B's device list with A", |event| match event {
                FrontendEvent::Devices(devices) if !devices.is_empty() => Some(devices[0].clone()),
                _ => None,
            })
            .await;

            send(
                &mut requests,
                FrontendRequest::RenameDevice {
                    fingerprint: device.fingerprint.clone(),
                    name: "Notebook do Gabriel".into(),
                },
            )
            .await;
            send(&mut requests, FrontendRequest::ListDevices).await;
            let renamed = expect_event(&mut events, "the renamed device", |event| match event {
                FrontendEvent::Devices(devices) => devices
                    .iter()
                    .find(|d| d.name == "Notebook do Gabriel")
                    .cloned(),
                _ => None,
            })
            .await;
            assert_eq!(renamed.fingerprint, device.fingerprint);
        })
        .await;
}

/// Remote-control speed, pairing window and address rules answer with the
/// state the GUI shows.
#[tokio::test(flavor = "current_thread")]
async fn speed_pairing_and_address_rules_round_trip() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_and_b().await;
            let (mut events, mut requests) = frontend(&desk, "A").await;

            send(&mut requests, FrontendRequest::SetAbsoluteSensitivity(140)).await;
            let speed = expect_event(
                &mut events,
                "the remote-control speed",
                |event| match event {
                    FrontendEvent::AbsoluteSensitivity(percent) if *percent != 100 => {
                        Some(*percent)
                    }
                    _ => None,
                },
            )
            .await;
            assert_eq!(speed, 140);

            send(&mut requests, FrontendRequest::StartPairing(30)).await;
            assert!(
                expect_event(&mut events, "pairing open", |event| match event {
                    FrontendEvent::PairingActive(active) => Some(*active),
                    _ => None,
                })
                .await
            );

            send(
                &mut requests,
                FrontendRequest::TrustAddress("10.20.30.40".into()),
            )
            .await;
            send(&mut requests, FrontendRequest::ListAddressRules).await;
            expect_event(&mut events, "the new address rule", |event| match event {
                FrontendEvent::AddressRules(rules)
                    if rules.iter().any(|r| r.address.starts_with("10.20.30.40")) =>
                {
                    Some(())
                }
                _ => None,
            })
            .await;

            send(
                &mut requests,
                FrontendRequest::UntrustAddress("10.20.30.40".into()),
            )
            .await;
            send(&mut requests, FrontendRequest::ListAddressRules).await;
            expect_event(&mut events, "the rule removed", |event| match event {
                FrontendEvent::AddressRules(rules)
                    if !rules.iter().any(|r| r.address.starts_with("10.20.30.40")) =>
                {
                    Some(())
                }
                _ => None,
            })
            .await;
        })
        .await;
}

/// The drag setting the GUI and the CLI change is stored and announced, and
/// it changes what a drag at the edge does.
#[tokio::test(flavor = "current_thread")]
async fn the_drag_setting_round_trips_and_applies() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_and_b().await;
            let (mut events, mut requests) = frontend(&desk, "A").await;
            send(&mut requests, FrontendRequest::Sync).await;
            let initial = expect_event(&mut events, "the drag setting", |event| match event {
                FrontendEvent::DragAcross(policy) => Some(*policy),
                _ => None,
            })
            .await;
            assert_eq!(
                initial,
                lan_mouse_ipc::DragAcross::Hold,
                "hold is the default"
            );

            send(
                &mut requests,
                FrontendRequest::SetDragAcross(lan_mouse_ipc::DragAcross::Cross),
            )
            .await;
            let changed = expect_event(&mut events, "the new drag setting", |event| match event {
                FrontendEvent::DragAcross(policy) if *policy != initial => Some(*policy),
                _ => None,
            })
            .await;
            assert_eq!(changed, lan_mouse_ipc::DragAcross::Cross);

            // a drag now crosses, and A's app gets the button back up
            desk.button_on("A", 0x110, 1).await;
            desk.move_mouse(1200.0, 0.0).await;
            desk.wait_focus("B").await;
            desk.wait_until("A's app let go of the drag", Duration::from_secs(2), |d| {
                d.pc("A").state().app_buttons.is_empty()
            })
            .await;
            desk.button_on("A", 0x110, 0).await;
        })
        .await;
}

/// The Windows acceleration switch of the GUI/CLI is stored and announced.
#[tokio::test(flavor = "current_thread")]
async fn the_windows_acceleration_setting_round_trips() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_and_b().await;
            let (mut events, mut requests) = frontend(&desk, "A").await;
            send(&mut requests, FrontendRequest::Sync).await;
            let initial = expect_event(
                &mut events,
                "the acceleration setting",
                |event| match event {
                    FrontendEvent::WindowsPointerAcceleration(on) => Some(*on),
                    _ => None,
                },
            )
            .await;
            assert!(!initial, "off by default");
            send(
                &mut requests,
                FrontendRequest::SetWindowsPointerAcceleration(true),
            )
            .await;
            assert!(
                expect_event(&mut events, "acceleration on", |event| match event {
                    FrontendEvent::WindowsPointerAcceleration(true) => Some(true),
                    _ => None,
                })
                .await
            );
            send(
                &mut requests,
                FrontendRequest::SetWindowsPointerAcceleration(false),
            )
            .await;
            assert!(
                !expect_event(&mut events, "acceleration off", |event| match event {
                    FrontendEvent::WindowsPointerAcceleration(false) => Some(false),
                    _ => None,
                })
                .await
            );
        })
        .await;
}
