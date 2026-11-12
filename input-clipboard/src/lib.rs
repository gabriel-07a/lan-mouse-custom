//! Clipboard sync primitives shared by the OS backends.
//!
//! The canonical representation of a clipboard snapshot is a small set of
//! well-known formats ([`Format`]). OS backends translate their native
//! clipboard types to and from this set, so the wire protocol and the sync
//! state machine are platform independent.
//!
//! This crate is deliberately free of OS-specific code for now: the in-memory
//! backend is used by tests, and the OS backends are added in later phases.

use std::{cell::RefCell, collections::BTreeMap, fmt::Display, rc::Rc, time::Duration};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::watch;

pub mod cf_html;
pub mod dib;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod portal;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod wayland;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod x11;
#[cfg(all(unix, not(target_os = "macos")))]
pub use portal::available_version as portal_available_version;
#[cfg(all(unix, not(target_os = "macos")))]
pub use portal::restore_token_present as portal_restore_token_present;
pub mod sync;
pub mod text;
pub mod transfer;
#[cfg(windows)]
pub mod windows;

/// In-process clipboard used by tests and by platforms without a native
/// backend yet.
pub type MemoryClipboard = SharedClipboard;

/// Well-known clipboard formats.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Format {
    Text,
    Html,
    Png,
}

impl Format {
    /// The canonical MIME type.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Text => "text/plain;charset=utf-8",
            Self::Html => "text/html",
            Self::Png => "image/png",
        }
    }

    /// The wire identifier used in [`lan_mouse_proto::clipboard`] frames.
    pub fn id(self) -> u8 {
        match self {
            Self::Text => lan_mouse_proto::clipboard::MIME_TEXT,
            Self::Html => lan_mouse_proto::clipboard::MIME_HTML,
            Self::Png => lan_mouse_proto::clipboard::MIME_PNG,
        }
    }

    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            lan_mouse_proto::clipboard::MIME_TEXT => Some(Self::Text),
            lan_mouse_proto::clipboard::MIME_HTML => Some(Self::Html),
            lan_mouse_proto::clipboard::MIME_PNG => Some(Self::Png),
            _ => None,
        }
    }
}

impl Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.mime())
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("clipboard backend error: {0}")]
    Backend(String),
    #[error("clipboard snapshot exceeds the configured limit")]
    TooLarge,
    #[error("clipboard transfer failed: {0}")]
    Transfer(String),
    #[error("clipboard frame could not be decoded: {0}")]
    Frame(#[from] lan_mouse_proto::clipboard::FrameError),
}

/// Size limits for a single clipboard snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_text: usize,
    pub max_html: usize,
    pub max_image: usize,
    pub max_snapshot: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_text: 256 * 1024,
            max_html: 1024 * 1024,
            max_image: 8 * 1024 * 1024,
            max_snapshot: 10 * 1024 * 1024,
        }
    }
}

impl Limits {
    pub fn max_for(&self, format: Format) -> usize {
        match format {
            Format::Text => self.max_text,
            Format::Html => self.max_html,
            Format::Png => self.max_image,
        }
    }

    /// Whether every representation fits and the total stays below the cap.
    pub fn accepts(&self, snapshot: &ClipboardSnapshot) -> bool {
        snapshot.total_len() <= self.max_snapshot
            && snapshot
                .items
                .iter()
                .all(|(format, data)| data.len() <= self.max_for(*format))
    }
}

/// A complete clipboard snapshot: one or more representations of the same
/// copy operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    items: BTreeMap<Format, Vec<u8>>,
}

impl ClipboardSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_items(items: impl IntoIterator<Item = (Format, Vec<u8>)>) -> Self {
        let mut snapshot = Self::default();
        for (format, data) in items {
            snapshot.insert(format, data);
        }
        snapshot
    }

    pub fn insert(&mut self, format: Format, data: Vec<u8>) -> &mut Self {
        self.items.insert(format, data);
        self
    }

    pub fn get(&self, format: Format) -> Option<&[u8]> {
        self.items.get(&format).map(Vec::as_slice)
    }

    pub fn iter(&self) -> impl Iterator<Item = (Format, &[u8])> {
        self.items.iter().map(|(f, d)| (*f, d.as_slice()))
    }

    pub fn formats(&self) -> impl Iterator<Item = Format> + '_ {
        self.items.keys().copied()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn total_len(&self) -> usize {
        self.items.values().map(Vec::len).sum()
    }

    /// Canonical hash used for echo suppression and integrity checks.
    ///
    /// The hash covers the format id and the length, so `text` and `html`
    /// with identical bytes hash differently.
    pub fn hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        for (format, data) in &self.items {
            hasher.update([format.id()]);
            hasher.update((data.len() as u64).to_be_bytes());
            hasher.update(data);
        }
        hasher.finalize().into()
    }

    pub fn wire_items(&self) -> Vec<(u8, u32)> {
        self.items
            .iter()
            .map(|(format, data)| (format.id(), u32::try_from(data.len()).unwrap_or(u32::MAX)))
            .collect()
    }
}

/// OS clipboard backend.
///
/// Implementations run on the single-threaded tokio runtime of the service,
/// hence `?Send`.
#[async_trait(?Send)]
pub trait Clipboard {
    /// Human readable backend name, used in logs and diagnostics.
    fn backend_name(&self) -> &'static str {
        "unknown"
    }

    /// Read the current clipboard contents.
    async fn read(&mut self) -> Result<ClipboardSnapshot, Error>;

    /// Replace the clipboard contents (used when applying a remote snapshot).
    async fn write(&mut self, snapshot: &ClipboardSnapshot) -> Result<(), Error>;

    /// Wait for the next *local* clipboard change.
    ///
    /// Writes performed through [`Clipboard::write`] must not wake this up;
    /// otherwise the sync layer would echo remote content straight back.
    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error>;
}

/// How many times a failed post-change read is retried before giving up.
pub const CHANGE_READ_RETRIES: u32 = 4;
/// Base delay between retries of a failed post-change read (it grows linearly:
/// 1x, 2x, 3x, 4x).
pub const CHANGE_READ_BACKOFF: Duration = Duration::from_millis(50);

/// Read the clipboard after a change notification whose first read failed.
///
/// The OS notification has already been consumed at this point, so dropping
/// the read would silently lose the user's copy: on Windows another
/// application (clipboard history, Office, a remote-desktop tool) can hold the
/// clipboard open for a few hundred milliseconds, and the portal can race the
/// owner's data. The backoff is small and bounded, so a genuinely broken
/// backend cannot stall the sync task.
pub async fn read_retrying(
    clipboard: &mut dyn Clipboard,
    retries: u32,
    backoff: Duration,
) -> Result<ClipboardSnapshot, Error> {
    let mut result = clipboard.read().await;
    for attempt in 1..=retries {
        if result.is_ok() {
            return result;
        }
        if !backoff.is_zero() {
            tokio::time::sleep(backoff * attempt).await;
        }
        result = clipboard.read().await;
    }
    result
}

/// Backend used when no OS integration is available. It never produces a
/// change and ignores writes, so the sync task stays idle.
#[derive(Debug, Default)]
pub struct NullClipboard;

#[async_trait(?Send)]
impl Clipboard for NullClipboard {
    fn backend_name(&self) -> &'static str {
        "none"
    }

    async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
        Ok(ClipboardSnapshot::default())
    }

    async fn write(&mut self, _snapshot: &ClipboardSnapshot) -> Result<(), Error> {
        Ok(())
    }

    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
        std::future::pending().await
    }
}

/// A shared, in-process clipboard used by tests and by the service while no
/// native backend is available on a platform.
#[derive(Clone, Debug)]
pub struct SharedClipboard {
    state: Rc<RefCell<SharedState>>,
    version: watch::Sender<u64>,
    changed: watch::Receiver<u64>,
}

#[derive(Debug, Default)]
struct SharedState {
    current: ClipboardSnapshot,
    applied: Vec<ClipboardSnapshot>,
}

impl Default for SharedClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedClipboard {
    pub fn new() -> Self {
        let (version, changed) = watch::channel(0);
        Self {
            state: Default::default(),
            version,
            changed,
        }
    }

    /// Simulate a copy performed by the local user.
    pub fn set_local(&self, snapshot: ClipboardSnapshot) {
        self.state.borrow_mut().current = snapshot;
        let next = *self.version.borrow() + 1;
        let _ = self.version.send_replace(next);
    }

    pub fn current(&self) -> ClipboardSnapshot {
        self.state.borrow().current.clone()
    }

    /// Snapshots applied by the sync layer.
    pub fn applied(&self) -> Vec<ClipboardSnapshot> {
        self.state.borrow().applied.clone()
    }

    /// Record a remote snapshot without waking the local change stream.
    pub fn apply_remote(&self, snapshot: &ClipboardSnapshot) {
        let mut state = self.state.borrow_mut();
        state.current = snapshot.clone();
        state.applied.push(snapshot.clone());
    }
}

#[async_trait(?Send)]
impl Clipboard for SharedClipboard {
    fn backend_name(&self) -> &'static str {
        "memory"
    }

    async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
        Ok(self.current())
    }

    async fn write(&mut self, snapshot: &ClipboardSnapshot) -> Result<(), Error> {
        let mut state = self.state.borrow_mut();
        state.current = snapshot.clone();
        state.applied.push(snapshot.clone());
        Ok(())
    }

    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
        if self.changed.changed().await.is_err() {
            return Ok(None);
        }
        Ok(Some(self.current()))
    }
}

#[async_trait(?Send)]
impl<T: Clipboard + ?Sized> Clipboard for Box<T> {
    fn backend_name(&self) -> &'static str {
        (**self).backend_name()
    }

    async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
        (**self).read().await
    }

    async fn write(&mut self, snapshot: &ClipboardSnapshot) -> Result<(), Error> {
        (**self).write(snapshot).await
    }

    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
        (**self).changed().await
    }
}

/// Create the best clipboard backend for the current platform.
///
/// Returns an error when no OS integration exists yet (currently Linux); the
/// service then keeps clipboard sync disabled instead of failing to start.
pub async fn default_backend() -> Result<Box<dyn Clipboard>, Error> {
    #[cfg(windows)]
    {
        Ok(Box::new(windows::WindowsClipboard::new()?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacOsClipboard::new()))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let wayland = std::env::var("XDG_SESSION_TYPE")
            .map(|session| session == "wayland")
            .unwrap_or_else(|_| std::env::var_os("WAYLAND_DISPLAY").is_some());
        if wayland {
            match portal::PortalClipboard::new().await {
                Ok(backend) => return Ok(Box::new(backend)),
                Err(e) => log::info!("portal clipboard unavailable: {e}"),
            }
            match wayland::WaylandClipboard::new() {
                Ok(backend) => return Ok(Box::new(backend)),
                Err(e) => log::info!("Wayland data-control clipboard unavailable: {e}"),
            }
        }
        match x11::X11Clipboard::new() {
            Ok(backend) => Ok(Box::new(backend)),
            Err(e) => {
                log::info!("X11 clipboard unavailable: {e}");
                Err(Error::Backend(
                    "no Linux clipboard backend is available".into(),
                ))
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(Error::Backend(
            "no clipboard backend is available on this platform".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn sample() -> ClipboardSnapshot {
        ClipboardSnapshot::from_items([
            (Format::Text, b"hello".to_vec()),
            (Format::Html, b"<b>hello</b>".to_vec()),
        ])
    }

    #[test]
    fn format_ids_roundtrip() {
        for format in [Format::Text, Format::Html, Format::Png] {
            assert_eq!(Format::from_id(format.id()), Some(format));
            assert!(!format.mime().is_empty());
        }
        assert_eq!(Format::from_id(200), None);
    }

    #[test]
    fn snapshot_hash_is_stable_and_format_sensitive() {
        let a = sample();
        let b = sample();
        assert_eq!(a.hash(), b.hash());
        assert_ne!(
            a.hash(),
            ClipboardSnapshot::from_items([(Format::Text, b"<b>hello</b>".to_vec())]).hash()
        );
        assert_ne!(
            ClipboardSnapshot::from_items([(Format::Text, b"hello".to_vec())]).hash(),
            ClipboardSnapshot::from_items([(Format::Text, b"hello!".to_vec())]).hash()
        );
    }

    #[test]
    fn limits_accept_and_reject() {
        let limits = Limits {
            max_text: 4,
            max_html: 4,
            max_image: 4,
            max_snapshot: 8,
        };
        assert!(limits.accepts(&ClipboardSnapshot::from_items([(
            Format::Text,
            b"abcd".to_vec()
        )])));
        assert!(!limits.accepts(&ClipboardSnapshot::from_items([(
            Format::Text,
            b"abcde".to_vec()
        )])));
        let mut total = ClipboardSnapshot::new();
        total.insert(Format::Text, vec![0; 5]);
        total.insert(Format::Html, vec![0; 5]);
        assert!(!limits.accepts(&total), "total cap is enforced");
    }

    #[test]
    fn wire_items_are_sorted_by_format() {
        let items = sample().wire_items();
        assert_eq!(items, vec![(Format::Text.id(), 5), (Format::Html.id(), 12)]);
    }

    #[tokio::test]
    async fn shared_clipboard_distinguishes_local_and_remote_writes() {
        let clipboard = SharedClipboard::new();
        let mut backend = clipboard.clone();
        assert!(backend.read().await.unwrap().is_empty());

        clipboard.set_local(sample());
        assert_eq!(backend.changed().await.unwrap(), Some(sample()));
        assert_eq!(backend.read().await.unwrap(), sample());

        // A remote write must not wake the local change stream.
        backend.write(&sample()).await.unwrap();
        assert_eq!(clipboard.applied(), vec![sample()]);
        let timeout =
            tokio::time::timeout(std::time::Duration::from_millis(50), backend.changed()).await;
        assert!(timeout.is_err(), "remote write woke the local watcher");
    }

    /// Clipboard backend that fails a scripted number of reads before it
    /// returns `snapshot`, like a Windows clipboard briefly held open by
    /// another application.
    #[derive(Default)]
    struct FlakyClipboard {
        failures: Cell<u32>,
        reads: Cell<u32>,
        snapshot: ClipboardSnapshot,
    }

    #[async_trait(?Send)]
    impl Clipboard for FlakyClipboard {
        async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
            self.reads.set(self.reads.get() + 1);
            if self.failures.get() > 0 {
                self.failures.set(self.failures.get() - 1);
                return Err(Error::Backend("clipboard is busy".into()));
            }
            Ok(self.snapshot.clone())
        }

        async fn write(&mut self, _snapshot: &ClipboardSnapshot) -> Result<(), Error> {
            Ok(())
        }

        async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_transient_read_failure_is_retried_until_it_succeeds() {
        let mut clipboard = FlakyClipboard {
            failures: Cell::new(2),
            snapshot: sample(),
            ..Default::default()
        };
        let result = read_retrying(&mut clipboard, 3, Duration::ZERO).await;
        assert_eq!(result.expect("eventually readable"), sample());
        assert_eq!(clipboard.reads.get(), 3, "two failures, then success");
    }

    #[tokio::test]
    async fn a_transient_read_failure_is_bounded() {
        let mut clipboard = FlakyClipboard {
            failures: Cell::new(u32::MAX),
            snapshot: sample(),
            ..Default::default()
        };
        assert!(
            read_retrying(&mut clipboard, 2, Duration::ZERO)
                .await
                .is_err()
        );
        assert_eq!(
            clipboard.reads.get(),
            3,
            "the initial attempt plus two retries, never more"
        );
    }

    #[tokio::test]
    async fn a_successful_read_is_not_retried() {
        let mut clipboard = FlakyClipboard {
            snapshot: sample(),
            ..Default::default()
        };
        assert_eq!(
            read_retrying(&mut clipboard, 4, Duration::from_millis(10))
                .await
                .expect("read"),
            sample()
        );
        assert_eq!(clipboard.reads.get(), 1);
    }
}
