//! State file consumed by the optional GNOME Shell cursor-hiding extension.
//!
//! GNOME Wayland has no client API to hide the compositor cursor. When a peer
//! is being controlled, this file is kept fresh; the extension watches it and
//! hides the cursor while `mtime` is recent. If the service crashes, the file
//! stops being refreshed and the extension restores the cursor after 3 s.

use lan_mouse_ipc::ClientHandle;
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

/// How often the service refreshes the heartbeat while capture is active.
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// Park marker content after a remote peer stopped driving this machine; the
/// extension waits for the peer's last injected motion before trusting
/// pointer movement.
const PARK_REMOTE: &[u8] = b"1";
/// Park marker content after this machine's own capture ended; the extension
/// shows the cursor on the first real movement (see the extension's
/// `PARK_KIND_RETURN`).
const PARK_RETURN: &[u8] = b"return";

pub(crate) struct CursorHide {
    path: Option<PathBuf>,
    /// Marker asking the extension to hide a cursor left parked at the screen
    /// edge after a remote peer returns control to the other machine.
    park_path: Option<PathBuf>,
    active: bool,
    last_touch: Option<Instant>,
    /// The outgoing capture that currently owns the hidden cursor. Tracking it
    /// lets a disconnect clear the heartbeat even when no `ClientLeft` event is
    /// delivered (transport drop, explicit disconnect, capture teardown).
    owner: Option<ClientHandle>,
}

impl CursorHide {
    pub(crate) fn new() -> Self {
        // Only meaningful on Linux/GNOME Wayland; the extension ignores it
        // elsewhere and the file is harmless.
        #[cfg(target_os = "linux")]
        let path = std::env::var_os("XDG_RUNTIME_DIR").and_then(|runtime| {
            let dir = PathBuf::from(runtime).join("lan-mouse");
            fs::create_dir_all(&dir).ok()?;
            Some(dir.join("cursor-hide"))
        });
        #[cfg(not(target_os = "linux"))]
        let path: Option<PathBuf> = None;
        // A previous service crash may have left the heartbeat behind; it
        // must not keep the cursor hidden after a fresh start.
        if let Some(path) = &path {
            let _ = fs::remove_file(path);
        }
        let park_path = path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|parent| parent.join("cursor-park"));
        if let Some(path) = &park_path {
            let _ = fs::remove_file(path);
        }
        Self {
            path,
            park_path,
            active: false,
            last_touch: None,
            owner: None,
        }
    }

    /// Use `path` as the heartbeat file; `None` disables the state files (an
    /// isolated test service must not hide the developer's real cursor).
    pub(crate) fn with_path(path: Option<PathBuf>) -> Self {
        let park_path = path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|parent| parent.join("cursor-park"));
        Self {
            path,
            park_path,
            active: false,
            last_touch: None,
            owner: None,
        }
    }

    /// An outgoing capture began for `handle`; hide the cursor until it ends.
    ///
    /// The heartbeat is written before a parked cursor is unparked: the
    /// extension polls both files, and a moment with neither (unpark first)
    /// flashed the cursor at the border on every re-entry.
    pub(crate) fn enter(&mut self, handle: ClientHandle) {
        self.owner = Some(handle);
        self.set_active(true);
        self.unpark();
        log::debug!("cursor-hide: active for client {handle}");
    }

    /// The capture for `handle` ended and control is back on this machine. A
    /// different handle must not clear it.
    ///
    /// The cursor is parked, not shown: the extension keeps it hidden at the
    /// border until the local pointer really moves. That absorbs the
    /// Enter/Leave/Enter flapping of a shared edge without the old fixed
    /// 4 s grace, which kept the cursor invisible while the user was already
    /// moving it on this screen. The park marker is written before the
    /// heartbeat is removed so the extension never sees neither file.
    pub(crate) fn leave(&mut self, handle: ClientHandle) {
        if self.owner == Some(handle) {
            self.owner = None;
            self.park_after_return();
            self.set_active(false);
            log::debug!("cursor-hide: left client {handle}, cursor parked until the mouse moves");
        }
    }

    /// Whether a real outgoing capture owns the cursor.
    pub(crate) fn is_outgoing_active(&self) -> bool {
        self.owner.is_some()
    }

    /// Remove both marker files this state owns.
    ///
    /// Deliberately unconditional: `set_active(false)` short-circuits when the
    /// in-memory flag is already false, so it cannot be relied on to delete a
    /// marker that exists on disk. During shutdown the file may have been
    /// written by an older instance or left behind by a crash; the extension
    /// hides the cursor based only on the file, so an early return here would
    /// leave the pointer hidden with no service left to restore it.
    fn remove_markers(&mut self) {
        self.active = false;
        self.touch();
        self.unpark();
    }

    /// Disconnect/teardown: never leave the cursor hidden or parked for a dead
    /// session.
    ///
    /// Both markers must go. The bundled GNOME extension hides the pointer
    /// while the heartbeat is fresh *and* while the parked-cursor marker
    /// exists (it only deletes the latter after the physical pointer moves), so
    /// removing just the heartbeat leaves the cursor hidden at the parked edge
    /// after a clean close — the reported shutdown cursor latch.
    pub(crate) fn clear(&mut self) {
        self.owner = None;
        self.remove_markers();
    }

    /// Shutdown entry point: release every marker before the (possibly slow or
    /// failing) backend/portal teardown runs. Idempotent.
    pub(crate) fn shutdown(&mut self) {
        self.clear();
    }

    /// Ask the extension to hide a cursor left at the screen edge after a
    /// remote peer returned control to the other machine. The extension shows
    /// it as soon as the local pointer moves or a new incoming session starts.
    pub(crate) fn park(&mut self) {
        self.write_park(PARK_REMOTE);
    }

    /// Park the cursor after this machine's own capture ended. The extension
    /// shows it on the first real movement: no peer injects motion here, so
    /// the settle delay used for [`Self::park`] would only make the cursor
    /// appear late (mid-screen for a fast hand).
    fn park_after_return(&mut self) {
        self.write_park(PARK_RETURN);
    }

    fn write_park(&mut self, kind: &[u8]) {
        if self.owner.is_some() {
            return;
        }
        let Some(path) = &self.park_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(error) = fs::write(path, kind) {
            log::debug!("could not write cursor-park marker: {error}");
        }
    }

    /// Remove the parked-cursor marker (show the local cursor again).
    pub(crate) fn unpark(&mut self) {
        let Some(path) = &self.park_path else {
            return;
        };
        if let Err(error) = fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::debug!("could not remove cursor-park marker: {error}");
            }
        }
    }

    /// While true, keep the heartbeat fresh; the extension hides the cursor.
    pub(crate) fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        self.touch();
    }

    /// Refresh the heartbeat if a capture is active. Called from the service
    /// loop on the same cadence as `HEARTBEAT_INTERVAL`.
    pub(crate) fn tick(&mut self) {
        if self.active
            && self
                .last_touch
                .is_none_or(|last| last.elapsed() >= HEARTBEAT_INTERVAL)
        {
            self.touch();
        }
    }

    fn touch(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        if self.active {
            if let Some(parent) = path.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    log::debug!("could not create cursor-hide directory: {error}");
                    return;
                }
            }
            if let Err(error) = fs::write(path, b"1") {
                log::debug!("could not refresh cursor-hide heartbeat: {error}");
            } else {
                self.last_touch = Some(Instant::now());
            }
        } else if let Err(error) = fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::debug!("could not remove cursor-hide marker: {error}");
            }
        } else {
            self.last_touch = None;
        }
    }
}

impl Drop for CursorHide {
    fn drop(&mut self) {
        // Never leave the GNOME cursor hidden or parked at the edge after a
        // clean service shutdown. This is the last line of defence for every
        // exit path (SIGINT/SIGTERM/`cli shutdown`/frontend quit): both
        // markers are removed even if the caller forgot `clear()`/`shutdown()`
        // or the backend teardown failed before reaching it.
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_interval_is_positive() {
        assert!(!HEARTBEAT_INTERVAL.is_zero());
    }

    #[test]
    fn no_runtime_dir_is_a_noop() {
        let mut hide = CursorHide::with_path(None);
        hide.set_active(true);
        hide.tick();
        hide.set_active(false);
    }

    fn temp_path(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-cursor-hide-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let path = dir.join("cursor-hide");
        (dir, path)
    }

    #[test]
    fn active_capture_creates_and_removes_the_heartbeat() {
        let (dir, path) = temp_path("active");
        let mut hide = CursorHide::with_path(Some(path.clone()));
        hide.set_active(true);
        assert!(path.exists(), "active capture must publish the heartbeat");
        hide.set_active(false);
        assert!(!path.exists(), "releasing the capture must remove it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disconnect_clears_the_heartbeat_for_the_owning_handle() {
        let (dir, path) = temp_path("disconnect");
        {
            let mut hide = CursorHide::with_path(Some(path.clone()));
            hide.enter(7);
            assert!(path.exists());
            // A different client leaving must not hide/clear the wrong session.
            hide.leave(8);
            assert!(path.exists());
            hide.clear();
            assert!(!path.exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether the extension would hide the cursor right now: it hides while
    /// the heartbeat exists (fresh) or the park marker exists.
    fn hidden(heartbeat: &std::path::Path, park: &std::path::Path) -> bool {
        heartbeat.exists() || park.exists()
    }

    /// Regression: after controlling the peer, coming back kept the cursor
    /// invisible for a fixed 4 s grace (plus up to a heartbeat tick) no matter
    /// how much the user moved it on this screen. Returning must park the
    /// cursor instead - the extension shows a parked cursor on the first real
    /// local movement - and no tick may revive the heartbeat afterwards.
    #[test]
    fn returning_control_parks_the_cursor_instead_of_keeping_it_hidden() {
        let (dir, path) = temp_path("return");
        let park = dir.join("cursor-park");
        {
            let mut hide = CursorHide::with_path(Some(path.clone()));
            hide.enter(7);
            assert!(path.exists(), "controlling the peer hides the cursor");
            assert!(!park.exists());

            hide.leave(7);
            assert!(
                !path.exists(),
                "the heartbeat must not outlive the capture (it ignores local movement)"
            );
            assert_eq!(
                std::fs::read(&park).expect("park marker"),
                b"return",
                "an own return must tell the extension to show the cursor on the first movement"
            );
            assert!(
                park.exists(),
                "the returned cursor is parked: hidden at the border until the mouse moves"
            );
            assert!(!hide.is_outgoing_active());

            for _ in 0..10 {
                hide.tick();
            }
            assert!(!path.exists(), "no heartbeat after the return");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A shared edge emits Enter/Leave/Enter within milliseconds. The cursor
    /// must stay hidden through every step (no flash at the border), and a
    /// re-entry must not leave the park marker behind.
    #[test]
    fn a_border_flap_never_shows_the_cursor() {
        let (dir, path) = temp_path("flap");
        let park = dir.join("cursor-park");
        {
            let mut hide = CursorHide::with_path(Some(path.clone()));
            for _ in 0..3 {
                hide.enter(7);
                assert!(hidden(&path, &park), "hidden while controlling the peer");
                assert!(!park.exists(), "a capture is not a parked cursor");
                hide.leave(7);
                assert!(hidden(&path, &park), "hidden right after the leave");
            }
            hide.enter(7);
            assert!(path.exists() && !park.exists());
            hide.clear();
            assert!(!hidden(&path, &park), "teardown always shows the cursor");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The marker content is a contract with the bundled GNOME extension:
    /// the fast reveal after an own return only works if both sides agree.
    #[test]
    fn the_extension_understands_both_park_kinds() {
        let extension =
            include_str!("../linux/gnome-extension/lan-mouse-cursor@lan-mouse-custom/extension.js");
        let own_return = std::str::from_utf8(PARK_RETURN).expect("ascii");
        assert!(
            extension.contains(&format!("const PARK_KIND_RETURN = '{own_return}';")),
            "the extension must recognise the own-return park marker"
        );
        assert!(
            extension.contains("this._parkSettleMs = ownReturn ? 0 : PARK_SETTLE_MS;"),
            "an own return must skip the settle delay"
        );
        assert_ne!(PARK_RETURN, PARK_REMOTE);
    }

    #[test]
    fn a_foreign_leave_neither_parks_nor_shows_the_cursor() {
        let (dir, path) = temp_path("foreign");
        let park = dir.join("cursor-park");
        {
            let mut hide = CursorHide::with_path(Some(path.clone()));
            hide.enter(7);
            hide.leave(8);
            assert!(path.exists(), "the live capture still owns the cursor");
            assert!(!park.exists());
            assert!(hide.is_outgoing_active());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parking_a_cursor_creates_and_removes_the_marker() {
        let (dir, path) = temp_path("park");
        let park = dir.join("cursor-park");
        {
            let mut hide = CursorHide::with_path(Some(path));
            hide.park();
            assert!(park.exists(), "a returned cursor must be parked");
            assert_eq!(
                std::fs::read(&park).expect("park marker"),
                b"1",
                "a peer's return keeps the settle delay for its injected motion"
            );
            hide.unpark();
            assert!(!park.exists(), "entering again must restore the cursor");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_removes_an_active_heartbeat() {
        let (dir, path) = temp_path("drop");
        {
            let mut hide = CursorHide::with_path(Some(path.clone()));
            hide.set_active(true);
            assert!(path.exists());
        }
        assert!(
            !path.exists(),
            "dropping the service must restore the cursor"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Shutdown regression: the parked-cursor marker is the *other* input the
    /// GNOME extension uses to hide the pointer. A clean close that removed
    /// only the heartbeat left the cursor hidden at the parked edge until the
    /// user moved the mouse — the reported "cursor pinned at the top after
    /// closing Lan Mouse".
    #[test]
    fn drop_removes_the_parked_cursor_marker_too() {
        let (dir, path) = temp_path("drop-park");
        let park = dir.join("cursor-park");
        {
            let mut hide = CursorHide::with_path(Some(path));
            hide.park();
            assert!(park.exists(), "precondition: the cursor is parked");
        }
        assert!(
            !park.exists(),
            "dropping the service must restore a cursor parked at the edge"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_removes_markers_written_by_another_instance() {
        // Simulates markers left by an older/crashed service or a previous run:
        // the in-memory state is idle and never saw them, but the extension
        // still hides the cursor from the files alone.
        let (dir, path) = temp_path("stale");
        let park = dir.join("cursor-park");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(&path, b"1").expect("stale heartbeat");
        std::fs::write(&park, b"1").expect("stale park marker");
        {
            let mut hide = CursorHide::with_path(Some(path.clone()));
            assert!(!hide.is_outgoing_active());
            hide.shutdown();
            assert!(
                !path.exists(),
                "shutdown must remove a heartbeat this instance did not write"
            );
            assert!(!park.exists(), "shutdown must remove the park marker");
            // Shutdown cleanup must be idempotent (teardown can race with a
            // pending drop and is called from several paths).
            hide.shutdown();
            hide.clear();
            assert!(!path.exists());
            assert!(!park.exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_after_a_return_unparks_the_cursor() {
        let (dir, path) = temp_path("clear-park");
        let park = dir.join("cursor-park");
        {
            let mut hide = CursorHide::with_path(Some(path));
            hide.park();
            assert!(park.exists(), "precondition: the cursor is parked");
            hide.clear();
            assert!(!park.exists(), "teardown must always unpark");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
