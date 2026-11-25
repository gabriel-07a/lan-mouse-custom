//! Random use of a two-PC desk by two people at once: each PC's own mouse and
//! keyboard do random things (move, cross, come back, click, drag, type, use
//! the home hotkey, pause), interleaved, from a seed. After every action the
//! desk must settle into a sane state, and at the end nothing may be left
//! pressed:
//!
//! - A and B never drive each other at the same time (no loop);
//! - each person's cursor is visible on exactly one screen once things
//!   settle (never captured into nothing);
//! - a PC that captures has a peer that is being driven by it;
//! - no button or key stays down on any PC.
//!
//! A failure prints the seed and the actions, so it replays exactly.

use std::time::Duration;

use input_capture::Position;
use tokio::task::LocalSet;

use super::{Desk, Focus, Personality, metrics::outward};

const BTN_LEFT: u32 = 0x110;
const KEY_A: u32 = 30;
const KEY_RIGHTALT: u32 = 100;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_0: u32 = 11;

/// Deterministic generator (a failure must replay).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn signed(&mut self, max: f64) -> f64 {
        (self.below(2001) as f64 / 1000.0 - 1.0) * max
    }
}

#[derive(Clone, Copy, Debug)]
enum Action {
    /// a mouse moves by (dx, dy)
    Move(&'static str, f64, f64),
    /// a mouse goes over to the other PC
    Cross(&'static str),
    /// a mouse comes back home from wherever its cursor is
    ComeBack(&'static str),
    Click(&'static str),
    /// press, move across towards the edge, release
    Drag(&'static str),
    Type(&'static str),
    HotkeyHome(&'static str),
    Pause(u64),
}

/// The side of `pc` where the other PC sits.
fn towards(pc: &str, side: Position) -> Position {
    if pc == "A" { side } else { side.opposite() }
}

fn actions(seed: u64, steps: usize, two_hands: bool) -> Vec<Action> {
    let mut rng = Lcg(seed.wrapping_add(0x9e37_79b9_7f4a_7c15));
    (0..steps)
        .map(|_| {
            let pc = if two_hands && rng.below(3) == 0 {
                "B"
            } else {
                "A"
            };
            match rng.below(10) {
                0 | 1 => Action::Move(pc, rng.signed(500.0), rng.signed(300.0)),
                2 | 3 => Action::Cross(pc),
                4 => Action::ComeBack(pc),
                5 => Action::Click(pc),
                6 => Action::Drag(pc),
                7 => Action::Type(pc),
                8 => Action::HotkeyHome(pc),
                _ => Action::Pause(rng.below(120)),
            }
        })
        .collect()
}

pub(crate) fn run(
    seed: u64,
    source: fn() -> Personality,
    target: fn() -> Personality,
    side: Position,
) {
    let two_hands = target().capture_available;
    let plan = actions(seed, 25, two_hands);
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(LocalSet::new().run_until(async move {
        let desk = Desk::builder()
            .pc("A", 1536, 960)
            .pc("B", 2560, 1440)
            .place("B", side, "A")
            .place("A", side.opposite(), "B")
            .personality("A", source())
            .personality("B", target())
            .start()
            .await;
        let label = format!(
            "seed {seed} ({} -> {}, B at {side:?})",
            source().label,
            target().label
        );
        for (step, action) in plan.iter().enumerate() {
            play(&desk, side, *action).await;
            settle(
                &desk,
                &format!("{label}, step {step}: {action:?}\nplan: {plan:?}"),
            )
            .await;
        }
        // the releases of the last action may still be on their way
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !desk.stuck_inputs().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let stuck = desk.stuck_inputs();
        assert!(
            stuck.is_empty(),
            "{label}: {stuck:?}\nplan: {plan:?}\n{}",
            desk.report()
        );
    }));
}

async fn play(desk: &Desk, side: Position, action: Action) {
    match action {
        Action::Move(pc, dx, dy) => desk.move_mouse_on(pc, dx, dy).await,
        Action::Cross(pc) => {
            let edge = towards(pc, side);
            // only from home: a cursor already on the other PC just keeps going
            if desk.focus_from(pc) == Focus::On(name_of(pc)) {
                desk.approach_edge(pc, edge, 0.5).await;
            }
            let (ox, oy) = outward(edge);
            desk.move_mouse_on(pc, ox * 60.0, oy * 60.0).await;
        }
        Action::ComeBack(pc) => {
            let (ox, oy) = outward(towards(pc, side));
            desk.move_mouse_on(pc, -ox * 3000.0, -oy * 3000.0).await;
        }
        Action::Click(pc) => {
            desk.button_on(pc, BTN_LEFT, 1).await;
            desk.button_on(pc, BTN_LEFT, 0).await;
        }
        Action::Drag(pc) => {
            let (ox, oy) = outward(towards(pc, side));
            desk.button_on(pc, BTN_LEFT, 1).await;
            desk.move_mouse_on(pc, ox * 400.0, oy * 400.0).await;
            desk.button_on(pc, BTN_LEFT, 0).await;
        }
        Action::Type(pc) => desk.key_tap_on(pc, KEY_A).await,
        Action::HotkeyHome(pc) => {
            desk.key_chord_on(pc, &[KEY_RIGHTALT, KEY_RIGHTSHIFT, KEY_0])
                .await;
        }
        Action::Pause(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
    }
}

fn name_of(pc: &str) -> &'static str {
    if pc == "A" { "A" } else { "B" }
}

/// Wait until the desk is in a sane state again; check it never loops.
async fn settle(desk: &Desk, what: &str) {
    desk.wait_until(&format!("{what}: settle"), Duration::from_secs(3), |d| {
        assert!(
            !looping(d),
            "{what}: A and B drive each other\n{}",
            d.report()
        );
        sane(d)
    })
    .await;
}

fn looping(desk: &Desk) -> bool {
    let a = desk.pc("A").state();
    let b = desk.pc("B").state();
    a.captured.is_some()
        && b.captured.is_some()
        && a.remote_edge.is_some()
        && b.remote_edge.is_some()
}

/// Every hand's cursor is somewhere visible, and a capture always has a
/// driven peer behind it.
fn sane(desk: &Desk) -> bool {
    let focus_ok = |hand: &str| desk.focus_from(hand) != Focus::Lost;
    let driving_ok = |from: &str, to: &str| {
        desk.pc(from).state().captured.is_none() || desk.pc(to).state().remote_edge.is_some()
    };
    focus_ok("A") && focus_ok("B") && driving_ok("A", "B") && driving_ok("B", "A")
}

/// Run the walks of `seeds`, a few desks at a time (each on its own thread
/// and runtime); a failure names its seed.
fn walk_seeds(
    seeds: std::ops::Range<u64>,
    source: fn() -> Personality,
    target: fn() -> Personality,
    side: Position,
) {
    let seeds: Vec<u64> = seeds.collect();
    // two desks at a time: every service holds an inotify instance (config
    // watch), and the per-user limit is 128 by default
    for chunk in seeds.chunks(2) {
        let walks: Vec<_> = chunk
            .iter()
            .map(|&seed| std::thread::spawn(move || run(seed, source, target, side)))
            .collect();
        for walk in walks {
            if let Err(panic) = walk.join() {
                std::panic::resume_unwind(panic);
            }
        }
    }
}

macro_rules! walks {
    ($($name:ident: $source:ident -> $target:ident at $side:ident, seeds $seeds:expr, $($ignore:meta)?;)*) => {
        $(
            #[test]
            $(#[$ignore])?
            fn $name() {
                walk_seeds($seeds, Personality::$source, Personality::$target, Position::$side);
            }
        )*
    };
}

// The user's desk on every `cargo test`; every pair and side weekly.
walks! {
    gnome_windows_right: gnome -> windows at Right, seeds 0..3, ;
    windows_gnome_left: windows -> gnome at Left, seeds 0..3, ;
    gnome_gnome_right: gnome -> gnome at Right, seeds 0..2, ;
    gnome_sway_right: gnome -> sway at Right, seeds 0..2, ;
    windows_x11_right: windows -> x11 at Right, seeds 0..2, ;
    full_gnome_windows: gnome -> windows at Right, seeds 3..40, ignore = "weekly";
    full_windows_gnome: windows -> gnome at Left, seeds 3..40, ignore = "weekly";
    full_gnome_gnome_top: gnome -> gnome at Top, seeds 0..30, ignore = "weekly";
    full_windows_windows_bottom: windows -> windows at Bottom, seeds 0..30, ignore = "weekly";
    full_sway_gnome: sway -> gnome at Right, seeds 0..30, ignore = "weekly";
    full_gnome_sway_left: gnome -> sway at Left, seeds 0..30, ignore = "weekly";
    full_sway_windows: sway -> windows at Top, seeds 0..30, ignore = "weekly";
    full_windows_sway: windows -> sway at Bottom, seeds 0..30, ignore = "weekly";
    full_sway_sway: sway -> sway at Right, seeds 0..30, ignore = "weekly";
    full_gnome_x11: gnome -> x11 at Left, seeds 0..30, ignore = "weekly";
    full_sway_x11: sway -> x11 at Right, seeds 0..30, ignore = "weekly";
}

#[test]
#[ignore = "debugging a single seed"]
fn single_seed_probe() {
    let seed: u64 = std::env::var("WALK_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let started = std::time::Instant::now();
    run(
        seed,
        Personality::gnome,
        Personality::windows,
        Position::Right,
    );
    eprintln!("seed {seed} took {:?}", started.elapsed());
}
