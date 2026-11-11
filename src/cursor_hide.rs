//! State file consumed by the optional GNOME Shell cursor-hiding extension.
//!
//! GNOME Wayland has no client API to hide the compositor cursor. When a peer
//! is being controlled, this file is kept fresh; the extension watches it and
//! hides the cursor while `mtime` is recent. If the service crashes, the file
//! stops being refreshed and the extension restores the cursor after 3 s.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

/// How often the service refreshes the heartbeat while capture is active.
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) struct CursorHide {
    path: Option<PathBuf>,
    active: bool,
    last_touch: Option<Instant>,
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
        let path = None;
        // A previous service crash may have left the heartbeat behind; it
        // must not keep the cursor hidden after a fresh start.
        if let Some(path) = &path {
            let _ = fs::remove_file(path);
        }
        Self {
            path,
            active: false,
            last_touch: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_path(path: Option<PathBuf>) -> Self {
        Self {
            path,
            active: false,
            last_touch: None,
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
        // Never leave the GNOME cursor hidden after a clean service shutdown.
        self.active = false;
        self.touch();
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
}
