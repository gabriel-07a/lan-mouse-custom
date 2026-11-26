//! The desk over a bad network: datagrams lost, delayed (so reordered) and
//! duplicated in both directions. The hand-off must still end where the
//! person expects, and no button or key may be left pressed on either PC
//! (a lost release used to leave a key repeating on the other machine).

use std::time::Duration;

use input_capture::Position;
use tokio::task::LocalSet;

use super::{Desk, Focus, Personality, metrics::outward, relay::Impairment};

const KEY_A: u32 = 30;
const BTN_LEFT: u32 = 0x110;

fn profile(name: &str) -> Impairment {
    match name {
        // a Wi-Fi link with the latency tails measured on the user's desk
        "wifi_tail" => Impairment {
            loss: 0.0,
            jitter: Duration::from_millis(30),
            duplicate: 0.0,
        },
        "lossy" => Impairment {
            loss: 0.05,
            jitter: Duration::from_millis(5),
            duplicate: 0.02,
        },
        // a badly broken link: 10 % loss with reordering
        "awful" => Impairment {
            loss: 0.10,
            jitter: Duration::from_millis(20),
            duplicate: 0.0,
        },
        // duplicated and reordered datagrams. Not combined with heavy loss:
        // then the DTLS handshake (webrtc-dtls) never completes; real links
        // do not duplicate at these rates.
        "duplicates" => Impairment {
            loss: 0.0,
            jitter: Duration::from_millis(20),
            duplicate: 0.05,
        },
        other => panic!("unknown profile {other}"),
    }
}

fn run(network: &str, source: fn() -> Personality, target: fn() -> Personality) {
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let impairment = profile(network);
    runtime.block_on(LocalSet::new().run_until(async move {
        let desk = Desk::builder()
            .pc("A", 1536, 960)
            .pc("B", 2560, 1440)
            .place("B", Position::Right, "A")
            .place("A", Position::Left, "B")
            .personality("A", source())
            .personality("B", target())
            .impaired(impairment)
            .start()
            .await;
        let label = format!("{network}: {} -> {}", source().label, target().label);
        let (ox, oy) = outward(Position::Right);
        for round in 0..4 {
            desk.approach_edge("A", Position::Right, 0.25 + 0.15 * f64::from(round))
                .await;
            desk.move_mouse_on("A", ox * 60.0, oy * 60.0).await;
            desk.wait_until(
                &format!("{label}, round {round}: on B"),
                Duration::from_secs(5),
                |d| d.focus() == Focus::On("B"),
            )
            .await;
            // typing and clicking on B through the bad link
            for _ in 0..10 {
                desk.key_tap_on("A", KEY_A).await;
            }
            desk.button_on("A", BTN_LEFT, 1).await;
            desk.move_mouse_on("A", 200.0, 120.0).await;
            desk.button_on("A", BTN_LEFT, 0).await;
            // back home
            desk.move_mouse_on("A", -ox * 900.0, -oy * 900.0).await;
            desk.wait_until(
                &format!("{label}, round {round}: back on A"),
                Duration::from_secs(5),
                |d| d.focus() == Focus::On("A"),
            )
            .await;
            desk.hold(
                &format!("{label}, round {round}: rests on A"),
                Duration::from_millis(300),
                |d| d.focus() == Focus::On("A"),
            )
            .await;
        }
        desk.wait_until(
            &format!("{label}: nothing left pressed"),
            Duration::from_secs(3),
            |d| d.stuck_inputs().is_empty(),
        )
        .await;
    }));
}

macro_rules! bad_networks {
    ($($name:ident: $network:literal, $source:ident -> $target:ident $(, $ignore:meta)?;)*) => {
        $(
            #[test]
            $(#[$ignore])?
            fn $name() {
                run($network, Personality::$source, Personality::$target);
            }
        )*
    };
}

bad_networks! {
    wifi_tail_gnome_to_windows: "wifi_tail", gnome -> windows;
    lossy_gnome_to_windows: "lossy", gnome -> windows;
    lossy_windows_to_gnome: "lossy", windows -> gnome;
    awful_gnome_to_windows: "awful", gnome -> windows;
    awful_windows_to_gnome: "awful", windows -> gnome, ignore = "weekly";
    wifi_tail_windows_to_gnome: "wifi_tail", windows -> gnome, ignore = "weekly";
    lossy_sway_to_x11: "lossy", sway -> x11, ignore = "weekly";
    awful_gnome_to_sway: "awful", gnome -> sway, ignore = "weekly";
    awful_sway_to_windows: "awful", sway -> windows, ignore = "weekly";
    awful_windows_to_windows: "awful", windows -> windows, ignore = "weekly";
    duplicates_gnome_to_windows: "duplicates", gnome -> windows;
    duplicates_windows_to_sway: "duplicates", windows -> sway, ignore = "weekly";
}

/// The network drops for a few seconds (Wi-Fi gone, the other PC restarting):
/// nobody pressed Disconnect, so the link must come back by itself and the
/// hand-off work again, without anyone clicking Connect.
#[test]
fn a_dropped_network_reconnects_by_itself() {
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(LocalSet::new().run_until(async {
        let desk = Desk::builder()
            .pc("A", 1536, 960)
            .pc("B", 2560, 1440)
            .place("B", Position::Right, "A")
            .place("A", Position::Left, "B")
            .personality("A", Personality::gnome())
            .personality("B", Personality::windows())
            .impaired(Impairment {
                loss: 0.0,
                jitter: Duration::ZERO,
                duplicate: 0.0,
            })
            .start()
            .await;
        desk.network_down(true);
        // longer than every liveness timeout (incoming 3 s, pings 2 rounds)
        tokio::time::sleep(Duration::from_secs(6)).await;
        desk.network_down(false);
        desk.wait_until(
            "both links back without a click",
            Duration::from_secs(20),
            |d| {
                ["A", "B"].iter().all(|pc| {
                    d.pc(pc)
                        .clients
                        .get_client_states()
                        .iter()
                        .all(|(_, _, state)| state.active && state.active_addr.is_some())
                })
            },
        )
        .await;
        desk.approach_edge("A", Position::Right, 0.5).await;
        desk.move_mouse_on("A", 60.0, 0.0).await;
        desk.wait_focus("B").await;
    }));
}
