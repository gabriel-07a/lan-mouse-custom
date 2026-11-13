//! Independent QA harness for the two reported regressions of this cycle:
//!
//! 1. **Hand-off stutter** (Ubuntu touchpad -> Windows): the cursor hesitates
//!    at the shared edge because the source keeps forwarding the residual
//!    edge push or batches the first crossing motion.
//! 2. **Cursor left hidden/parked after closing Lan Mouse** (Ubuntu/GNOME
//!    Wayland): the cursor-hide heartbeat or the cursor-park marker survives
//!    the service and the GNOME extension keeps the pointer hidden.
//!
//! The production state machines that own both behaviours
//! (`capture::CaptureTask`, `service::Service`) live in private modules of the
//! `lan-mouse` binary crate, so an integration test cannot construct them
//! without editing production code. This file therefore tests the seams that
//! *are* deterministic and self-contained, by compiling those source files
//! into the test crate with `#[path]` (read-only: no production file is
//! modified):
//!
//! - `src/cursor_hide.rs`: the heartbeat/park marker contract consumed by the
//!   bundled GNOME extension, including the state left behind on shutdown and
//!   the cleanup of stale markers from a crashed service;
//! - `src/coalesce.rs`: the hand-off leading-edge/burst behaviour that decides
//!   whether the first motion after `Begin` waits for the batch window;
//! - the service <-> GNOME extension constants (heartbeat freshness, extension
//!   version) are pinned by parsing the bundled files, so a drift between the
//!   two halves of the cursor-hide feature fails here instead of in the field.
//!
//! The end-to-end daemon lifecycle (SIGTERM / `cli shutdown` / SIGKILL restart
//! and the real on-disk marker cleanup) is covered by
//! `scripts/qa-handoff-shutdown.sh`; the DXGI/libei portal paths need real
//! hardware and are covered by the manual checklist in
//! `docs/testing/handoff-and-shutdown-verification.md`.
//!
//! Layer: L2 deterministic contract tests. No sockets, no sleeps, no display;
//! every test only writes to its own temporary directory and is safe to run in
//! parallel.

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// Source file compiled into this test crate instead of the binary crate.
#[allow(dead_code)]
#[path = "../src/cursor_hide.rs"]
mod cursor_hide;

/// Source file compiled into this test crate instead of the binary crate.
#[allow(dead_code)]
#[path = "../src/coalesce.rs"]
mod coalesce;

use coalesce::MotionCoalescer;
use cursor_hide::{CursorHide, HEARTBEAT_INTERVAL};

/// Self-deleting temporary directory, unique per test and process so parallel
/// test binaries cannot collide.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-qa-handoff-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Cursor hide / park: shutdown and stale-daemon state
// ---------------------------------------------------------------------------

/// What the GNOME extension observes: a heartbeat file for an active capture
/// and a park marker for a cursor returned to the edge.
fn extension_state(hide_path: &Path, park_path: &Path) -> (bool, bool) {
    (hide_path.exists(), park_path.exists())
}

/// The reported bug: closing Lan Mouse must never leave the GNOME cursor
/// hidden. A clean teardown goes through `clear()` (last session ended /
/// explicit disconnect) and then drops the state; after that neither file may
/// remain, otherwise the extension keeps the pointer hidden with no service
/// left to refresh or restore it.
#[test]
fn a_clean_teardown_leaves_no_cursor_hiding_state() {
    let dir = TempDir::new("teardown");
    let hide_path = dir.join("cursor-hide");
    let park_path = dir.join("cursor-park");

    {
        let mut hide = CursorHide::with_path(Some(hide_path.clone()));

        hide.enter(7);
        assert_eq!(
            extension_state(&hide_path, &park_path),
            (true, false),
            "an outgoing capture must publish the heartbeat"
        );

        // A border Leave/Enter flap keeps the heartbeat for the grace period,
        // so the cursor does not flash at the shared edge during a hand-off.
        hide.leave(7);
        assert!(
            hide_path.exists(),
            "the post-leave grace must keep the cursor hidden through a hand-off flap"
        );

        // Teardown: the session ended, so the hide must be released.
        hide.clear();
        assert!(
            !hide_path.exists(),
            "clearing the capture must remove the heartbeat"
        );

        // A remote peer returned control: the cursor is parked at the edge and
        // the extension hides it until the local pointer moves.
        hide.park();
        assert!(
            park_path.exists(),
            "a returned cursor is parked so it does not appear at the stale entry point"
        );

        // Closing the app must not leave the parked-cursor marker behind: the
        // extension would keep hiding a pointer with no service to restore it.
        hide.clear();
        assert!(
            !park_path.exists(),
            "clearing the cursor state must remove the park marker"
        );
    }

    assert_eq!(
        extension_state(&hide_path, &park_path),
        (false, false),
        "a clean shutdown must leave no cursor-hiding artifact for the extension"
    );
}

/// A crash (`SIGKILL`/power loss) cannot run the destructor. The next start
/// must treat both files as stale: otherwise the fresh service starts with a
/// permanently hidden/parked cursor until the user moves the pointer.
#[cfg(target_os = "linux")]
#[test]
fn a_crashed_service_cannot_keep_the_cursor_hidden_after_restart() {
    // `CursorHide::new` reads `XDG_RUNTIME_DIR`. Only this test touches the
    // process-wide variable and no other test in this binary calls `new()`.
    let dir = TempDir::new("restart");
    let runtime = dir.join("runtime");
    let lan_mouse_dir = runtime.join("lan-mouse");
    std::fs::create_dir_all(&lan_mouse_dir).expect("runtime dir");
    let hide_path = lan_mouse_dir.join("cursor-hide");
    let park_path = lan_mouse_dir.join("cursor-park");
    std::fs::write(&hide_path, b"1").expect("stale heartbeat");
    std::fs::write(&park_path, b"1").expect("stale park marker");

    let previous = std::env::var_os("XDG_RUNTIME_DIR");
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    // SAFETY: no other test in this binary reads or writes XDG_RUNTIME_DIR;
    // `with_path`-based tests pass their paths explicitly.
    let hide = CursorHide::new();
    let state = extension_state(&hide_path, &park_path);
    drop(hide);
    match previous {
        Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
        None => std::env::remove_var("XDG_RUNTIME_DIR"),
    }

    assert_eq!(
        state,
        (false, false),
        "a fresh service must remove the heartbeat and park marker left by a crashed one"
    );
}

/// The hide can only own one capture at a time: a leave from a non-owning
/// handle must not release the cursor (this used to un-hide the pointer while
/// the source was still driving the peer).
#[test]
fn only_the_owning_handle_can_release_the_hidden_cursor() {
    let dir = TempDir::new("owner");
    let hide_path = dir.join("cursor-hide");
    let park_path = dir.join("cursor-park");
    let mut hide = CursorHide::with_path(Some(hide_path.clone()));

    hide.enter(7);
    hide.leave(8);
    assert!(
        hide.is_outgoing_active(),
        "a different client leaving must not release the live capture"
    );
    assert!(hide_path.exists());

    hide.leave(7);
    assert!(!hide.is_outgoing_active());
    assert!(
        hide_path.exists(),
        "the heartbeat stays during the grace window after the owner leaves"
    );

    hide.clear();
    assert_eq!(extension_state(&hide_path, &park_path), (false, false));
}

/// Parking must never downgrade a live outgoing capture to the park shortcut:
/// the extension restores a parked cursor on the first pointer movement, which
/// would reveal the hidden cursor in the middle of the hand-off.
#[test]
fn parking_is_refused_while_a_capture_owns_the_cursor() {
    let dir = TempDir::new("park-guard");
    let hide_path = dir.join("cursor-hide");
    let park_path = dir.join("cursor-park");
    let mut hide = CursorHide::with_path(Some(hide_path.clone()));

    hide.enter(4);
    hide.park();
    assert!(
        !park_path.exists(),
        "parking during an outgoing capture must be a no-op"
    );
    assert!(hide_path.exists());

    hide.clear();
    hide.park();
    assert!(
        park_path.exists(),
        "an idle returned cursor must still be parked after the capture is gone"
    );
    hide.unpark();
    assert!(!park_path.exists());
}

/// Border flapping (Enter/Leave/Enter within milliseconds) is the normal
/// hand-off pattern at a shared edge. It must never leave the state machine
/// thinking a capture is live after the session ended, and must never leave a
/// heartbeat behind after teardown.
#[test]
fn repeated_enter_leave_churn_releases_the_cursor_on_teardown() {
    let dir = TempDir::new("churn");
    let hide_path = dir.join("cursor-hide");
    let park_path = dir.join("cursor-park");
    let mut hide = CursorHide::with_path(Some(hide_path.clone()));

    for _ in 0..5 {
        hide.enter(7);
        hide.leave(7);
    }
    assert!(
        !hide.is_outgoing_active(),
        "after the flapping ends no outgoing capture may stay active"
    );
    // The last Leave keeps the hide alive during the grace window on purpose.
    assert!(hide_path.exists());
    hide.clear();
    assert_eq!(extension_state(&hide_path, &park_path), (false, false));
}

/// Last line of defence for the reported shutdown bug: even if the service
/// exits without calling `clear()`/`shutdown()` (panic, launcher kill on a
/// different path, early return), the destructor must restore the pointer.
/// A parked cursor is the case that used to survive: `cursor-hide` was removed
/// but `cursor-park` stayed, so the extension kept hiding the pointer.
#[test]
fn a_process_exit_never_leaves_the_parked_cursor_marker_behind() {
    let dir = TempDir::new("exit-park");
    let hide_path = dir.join("cursor-hide");
    let park_path = dir.join("cursor-park");
    {
        let mut hide = CursorHide::with_path(Some(hide_path.clone()));
        hide.park();
        assert!(
            park_path.exists(),
            "precondition: the returned cursor is parked at the edge"
        );
        // drop without an explicit teardown call, like an unexpected exit path
    }
    assert!(
        !park_path.exists(),
        "dropping the service must remove the parked-cursor marker"
    );
    assert!(!hide_path.exists());
}

/// The shutdown path can run when the in-memory state is idle while the files
/// still exist (markers written by an older instance or a previous crashed
/// run). Cleanup must be unconditional and idempotent: the extension hides the
/// cursor from the files alone.
#[test]
fn shutdown_removes_markers_this_instance_never_wrote() {
    let dir = TempDir::new("stale-shutdown");
    let hide_path = dir.join("cursor-hide");
    let park_path = dir.join("cursor-park");
    std::fs::create_dir_all(dir.path()).expect("temp dir");
    std::fs::write(&hide_path, b"1").expect("stale heartbeat");
    std::fs::write(&park_path, b"1").expect("stale park marker");

    let mut hide = CursorHide::with_path(Some(hide_path.clone()));
    assert!(
        !hide.is_outgoing_active(),
        "precondition: this instance never saw a capture"
    );
    hide.shutdown();
    assert_eq!(
        extension_state(&hide_path, &park_path),
        (false, false),
        "shutdown must remove markers written by another/crashed instance"
    );
    // Idempotent: teardown can race with Drop and be reached from several paths.
    hide.shutdown();
    hide.clear();
    assert_eq!(extension_state(&hide_path, &park_path), (false, false));
}

// ---------------------------------------------------------------------------
// GNOME extension contract (service side constants vs bundled JS)
// ---------------------------------------------------------------------------

/// The extension hides the cursor while `cursor-hide` is fresh (`mtime` within
/// `HEARTBEAT_MS`, polled every 50 ms). The service must refresh it well inside
/// that window, otherwise the pointer flashes back on every poll interval.
#[test]
fn the_service_heartbeat_fits_inside_the_extension_freshness_window() {
    let extension =
        include_str!("../linux/gnome-extension/lan-mouse-cursor@lan-mouse-custom/extension.js");
    let heartbeat_ms = parse_js_integer(extension, "HEARTBEAT_MS")
        .expect("extension must keep a HEARTBEAT_MS constant");
    let freshness = Duration::from_millis(heartbeat_ms);

    assert!(
        HEARTBEAT_INTERVAL < freshness,
        "the service refreshes every {HEARTBEAT_INTERVAL:?} but the extension only accepts a \
         heartbeat younger than {freshness:?}"
    );
    assert!(
        HEARTBEAT_INTERVAL * 2 <= freshness,
        "a single missed refresh ({:?}) must not expire the heartbeat ({freshness:?})",
        HEARTBEAT_INTERVAL * 2
    );
}

/// The app only hides the cursor when the *loaded* extension reports the same
/// version it bundles (`cursor-extension-loaded` marker), so the version in
/// `cursor_extension.rs`, the bundled JS and `metadata.json` must agree.
#[test]
fn the_extension_version_is_consistent_everywhere() {
    let extension =
        include_str!("../linux/gnome-extension/lan-mouse-cursor@lan-mouse-custom/extension.js");
    let installer = include_str!("../lan-mouse-gpui/src/cursor_extension.rs");
    let metadata =
        include_str!("../linux/gnome-extension/lan-mouse-cursor@lan-mouse-custom/metadata.json");

    let js_version = parse_js_string(extension, "EXTENSION_VERSION")
        .expect("extension.js must keep an EXTENSION_VERSION constant");
    let rust_version = parse_rust_string_const(installer, "EXTENSION_VERSION")
        .expect("cursor_extension.rs must keep an EXTENSION_VERSION constant");
    assert_eq!(
        js_version, rust_version,
        "the bundled extension version and the installer version must match, \
         otherwise the app reports a stale extension after every update"
    );

    let metadata: serde_json::Value = serde_json::from_str(metadata).expect("metadata.json");
    assert_eq!(
        metadata["version"].as_u64().map(|value| value.to_string()),
        Some(js_version),
        "metadata.json version must match the extension source version"
    );
}

/// Extract a numeric `const NAME = 3_000;` from the bundled JS.
fn parse_js_integer(source: &str, name: &str) -> Option<u64> {
    let prefix = format!("const {name}");
    let line = source
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&prefix))?;
    let value = line.split_once('=')?.1.trim().trim_end_matches(';');
    value.replace('_', "").parse().ok()
}

/// Extract a single-quoted string `const NAME = '6';` from the bundled JS.
fn parse_js_string(source: &str, name: &str) -> Option<String> {
    let prefix = format!("const {name}");
    let line = source
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&prefix))?;
    let value = line.split_once('=')?.1.trim().trim_end_matches(';');
    value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .map(str::to_owned)
}

/// Extract a `const NAME: &str = "6";` from a Rust source string.
fn parse_rust_string_const(source: &str, name: &str) -> Option<String> {
    let prefix = format!("const {name}: &str =");
    let line = source
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&prefix))?;
    let value = line.split_once('=')?.1.trim().trim_end_matches(';');
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Hand-off motion: the first crossing motion must not wait for the batch timer
// ---------------------------------------------------------------------------

/// `arm_leading_edge` is called when an outgoing capture starts. The first
/// motion after `Begin` must be flushed immediately, and the whole initial
/// crossing burst must bypass coalescing; otherwise the pointer stutters for a
/// batch window right after the hand-off (reported "stuck at the edge").
#[test]
fn the_first_motion_after_a_handoff_does_not_wait_for_the_batch_window() {
    // A long batch window makes any timing failure obvious: inside the
    // hand-off burst only the leading-edge/burst logic can be responsible.
    let mut coalescer = MotionCoalescer::new(60_000);
    let t0 = std::time::Instant::now();

    // Establish a normal flush point so `last_flush` is recent.
    assert!(!coalescer.push(7, 0, 1.0, 0.0, t0));
    assert!(coalescer.take(7).is_some());

    coalescer.arm_leading_edge();
    assert!(
        coalescer.push(7, 1, 1.0, 0.0, t0 + Duration::from_micros(100)),
        "the first motion after Begin must be sent immediately"
    );
    assert!(
        coalescer.push(7, 2, 1.0, 0.0, t0 + Duration::from_micros(200)),
        "the initial crossing burst must also bypass batching"
    );
    assert!(
        !coalescer.push(7, 3, 1.0, 0.0, t0 + Duration::from_millis(45)),
        "after the hand-off burst, normal batching must resume (no packet flood)"
    );
}

/// A discrete event (click/key) flushes the pending motion but must not end
/// the hand-off burst window: the rest of the crossing movement still bypasses
/// batching. The burst must then expire on its own so the packet rate drops.
#[test]
fn a_discrete_flush_does_not_end_the_handoff_burst_window() {
    let mut coalescer = MotionCoalescer::new(60_000);
    let t0 = std::time::Instant::now();
    coalescer.arm_leading_edge();
    assert!(
        coalescer.push(7, 0, 1.0, 2.0, t0),
        "armed leading edge flushes the first delta"
    );
    let motion = coalescer.take(7).expect("pending motion");
    assert_eq!(motion.dx, 1.0);
    assert_eq!(motion.dy, 2.0);

    assert!(
        coalescer.push(7, 1, 1.0, 0.0, t0 + Duration::from_millis(1)),
        "the rest of the crossing burst must still bypass batching after a discrete flush"
    );
    assert!(
        !coalescer.push(7, 2, 1.0, 0.0, t0 + Duration::from_millis(45)),
        "once the hand-off burst expired the normal batch timer must be authoritative"
    );
}

// ---------------------------------------------------------------------------
// Hand-off residual handling: regression sentinel for the reported "stick"
// ---------------------------------------------------------------------------

/// The reported hand-off stick was a *timed* settle window after `Begin`:
/// every motion for 200 ms was dropped, so the crossing felt stuck and the
/// user had to push again (which then caused the extra force / repeated
/// enter-leave chatter). The fix replaces it with a direction-aware,
/// distance-bounded residual filter: the leading reverse deltas are suppressed
/// only until the first forward delta and never beyond a small budget, so the
/// first forward motion is forwarded immediately and a deliberate return is
/// still delivered.
///
/// `capture::CaptureTask` is private and needs a live capture backend, so this
/// pins the mechanism at source level (a reintroduced timed settle or an
/// unbounded filter fails here); the behaviour itself is covered by the
/// `capture_handle_tests` in `src/capture.rs`, exercised by the workspace test
/// run.
#[test]
fn the_handoff_uses_a_bounded_residual_filter_not_a_timed_settle() {
    let capture = include_str!("../src/capture.rs");
    assert!(
        !capture.contains("ENTER_MOTION_SETTLE"),
        "a timed motion settle after Begin must not be reintroduced: it drops the first \
         forward motion (the reported cursor stick at the shared edge)"
    );
    assert!(
        capture.contains("residual_reverse_px = Some(HANDOFF_RESIDUAL_MAX_PX)"),
        "a new outgoing capture must arm the reverse-residual filter"
    );
    let px = parse_rust_f64_const(capture, "HANDOFF_RESIDUAL_MAX_PX")
        .expect("the residual budget must stay a named const");
    assert!(
        px > 0.0 && px <= 64.0,
        "the suppressed residual must stay small ({px} px): a large budget would swallow a \
         deliberate hand-back and require extra force"
    );
}

/// Extract a `const NAME: f64 = 32.0;` from a Rust source string.
fn parse_rust_f64_const(source: &str, name: &str) -> Option<f64> {
    let prefix = format!("const {name}: f64 =");
    let line = source
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&prefix))?;
    let value = line.split_once('=')?.1.trim().trim_end_matches(';');
    value.replace('_', "").parse().ok()
}
