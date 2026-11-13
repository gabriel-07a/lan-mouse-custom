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
/// Keep hiding briefly after a capture leaves. At a shared edge the capture
/// emits Enter/Leave/Enter within milliseconds; removing the heartbeat on the
/// first Leave made the source cursor flash back at the border on every
/// hand-off.
pub(crate) const LEAVE_GRACE: Duration = Duration::from_secs(4);

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
    /// After `ClientLeft`, keep the heartbeat for [`LEAVE_GRACE`] so an
    /// immediate re-enter at the shared border does not flash the cursor.
    grace_until: Option<Instant>,
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
            grace_until: None,
        }
    }

    #[cfg(test)]
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
            grace_until: None,
        }
    }

    /// An outgoing capture began for `handle`; hide the cursor until it ends.
    pub(crate) fn enter(&mut self, handle: ClientHandle) {
        self.owner = Some(handle);
        self.grace_until = None;
        self.set_active(true);
        log::debug!("cursor-hide: active for client {handle}");
    }

    /// The capture for `handle` ended. A different handle must not clear it.
    /// Keep the heartbeat for a short grace period to absorb border flapping.
    pub(crate) fn leave(&mut self, handle: ClientHandle) {
        if self.owner == Some(handle) {
            self.owner = None;
            self.grace_until = Some(Instant::now() + LEAVE_GRACE);
            log::debug!("cursor-hide: leaving client {handle} (grace period)");
        }
    }

    /// Whether a real outgoing capture (not just its post-leave grace) owns
    /// the cursor. Teardown decisions must not release a live capture because
    /// of a pending grace timer.
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
        self.grace_until = None;
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
        if self.owner.is_some() {
            return;
        }
        let Some(path) = &self.park_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(error) = fs::write(path, b"1") {
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
        if self
            .grace_until
            .is_some_and(|until| Instant::now() >= until)
            && self.owner.is_none()
        {
            self.grace_until = None;
            self.set_active(false);
            return;
        }
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

    #[test]
    fn a_leave_keeps_the_cursor_hidden_during_the_grace_period() {
        let (dir, path) = temp_path("grace");
        {
            let mut hide = CursorHide::with_path(Some(path.clone()));
            hide.enter(7);
            hide.leave(7);
            assert!(
                path.exists(),
                "a border Leave/Enter flap must not flash the cursor"
            );
            hide.tick();
            assert!(
                path.exists(),
                "the heartbeat is still inside the grace window"
            );
            hide.clear();
            assert!(!path.exists());
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
