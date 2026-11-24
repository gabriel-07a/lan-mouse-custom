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
pub mod files;
pub mod hdrop;
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
    /// Copied files and folders: on this PC the list of paths, between PCs a
    /// bundle of their contents (see [`files`]).
    Files,
}

impl Format {
    /// The canonical MIME type.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Text => "text/plain;charset=utf-8",
            Self::Html => "text/html",
            Self::Png => "image/png",
            Self::Files => "text/uri-list",
        }
    }

    /// The wire identifier used in [`lan_mouse_proto::clipboard`] frames.
    pub fn id(self) -> u8 {
        match self {
            Self::Text => lan_mouse_proto::clipboard::MIME_TEXT,
            Self::Html => lan_mouse_proto::clipboard::MIME_HTML,
            Self::Png => lan_mouse_proto::clipboard::MIME_PNG,
            Self::Files => lan_mouse_proto::clipboard::MIME_FILES,
        }
    }

    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            lan_mouse_proto::clipboard::MIME_TEXT => Some(Self::Text),
            lan_mouse_proto::clipboard::MIME_HTML => Some(Self::Html),
            lan_mouse_proto::clipboard::MIME_PNG => Some(Self::Png),
            lan_mouse_proto::clipboard::MIME_FILES => Some(Self::Files),
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

impl Error {
    /// Whether this error means the OS clipboard session itself is gone and
    /// the backend must be recreated (for example the XDG portal restarted or
    /// invalidated the RemoteDesktop session). Retrying a write on the same
    /// dead session can never succeed and used to leave clipboard sync broken
    /// until the service was restarted.
    pub fn needs_backend_restart(&self) -> bool {
        let Self::Backend(message) = self else {
            return false;
        };
        let message = message.to_ascii_lowercase();
        message.contains("invalid session")
            || message.contains("session is invalid")
            || message.contains("session invalid")
    }
}

/// Size limits for a single clipboard snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_text: usize,
    pub max_html: usize,
    pub max_image: usize,
    /// text, HTML and image together
    pub max_snapshot: usize,
    /// copied files and folders, on their own (a copy with files carries
    /// nothing else)
    pub max_files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_text: 256 * 1024,
            max_html: 1024 * 1024,
            max_image: 8 * 1024 * 1024,
            max_snapshot: 10 * 1024 * 1024,
            max_files: 64 * 1024 * 1024,
        }
    }
}

impl Limits {
    pub fn max_for(&self, format: Format) -> usize {
        match format {
            Format::Text => self.max_text,
            Format::Html => self.max_html,
            Format::Png => self.max_image,
            Format::Files => self.max_files,
        }
    }

    /// Whether every representation fits and text, HTML and image together
    /// stay below the cap (files have their own).
    pub fn accepts(&self, snapshot: &ClipboardSnapshot) -> bool {
        snapshot.total_len() - snapshot.get(Format::Files).map_or(0, <[u8]>::len)
            <= self.max_snapshot
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

    /// The same snapshot without one representation.
    pub fn without(&self, format: Format) -> Self {
        let mut items = self.items.clone();
        items.remove(&format);
        Self { items }
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

/// Lower-case hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Wraps a backend so copied files travel as their contents (see [`files`]):
/// reading packs the copied paths into a bundle (at most
/// [`Limits::max_files`], read off the async thread), writing extracts a
/// received bundle under [`files::received_dir`] and puts the extracted
/// paths on the clipboard. Snapshots without files pass through unchanged.
pub struct FileBundling<B> {
    inner: B,
    limits: std::rc::Rc<std::cell::Cell<Limits>>,
    root: std::path::PathBuf,
}

impl<B> FileBundling<B> {
    pub fn new(inner: B, limits: std::rc::Rc<std::cell::Cell<Limits>>) -> Self {
        Self::with_root(inner, limits, files::received_dir())
    }

    pub fn with_root(
        inner: B,
        limits: std::rc::Rc<std::cell::Cell<Limits>>,
        root: std::path::PathBuf,
    ) -> Self {
        Self {
            inner,
            limits,
            root,
        }
    }

    async fn bundled(&self, snapshot: ClipboardSnapshot) -> ClipboardSnapshot {
        if snapshot.get(Format::Files).is_none() {
            return snapshot;
        }
        let max = self.limits.get().max_files;
        let copy = snapshot.clone();
        match tokio::task::spawn_blocking(move || files::bundle_snapshot(&copy, max)).await {
            Ok(Ok(bundled)) => {
                log::info!(
                    "clipboard: copied files packed ({} bytes)",
                    bundled.total_len()
                );
                bundled
            }
            Ok(Err(e)) => {
                // still share what else was copied (the paths as text)
                log::warn!("clipboard: not sending the copied files: {e}");
                snapshot.without(Format::Files)
            }
            Err(e) => {
                log::warn!("clipboard: packing the copied files failed: {e}");
                snapshot.without(Format::Files)
            }
        }
    }
}

#[async_trait(?Send)]
impl<B: Clipboard> Clipboard for FileBundling<B> {
    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }

    async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
        let snapshot = self.inner.read().await?;
        Ok(self.bundled(snapshot).await)
    }

    async fn write(&mut self, snapshot: &ClipboardSnapshot) -> Result<(), Error> {
        if snapshot.get(Format::Files).is_none() {
            return self.inner.write(snapshot).await;
        }
        let (copy, root) = (snapshot.clone(), self.root.clone());
        let local = tokio::task::spawn_blocking(move || files::local_snapshot(&copy, &root))
            .await
            .map_err(|e| Error::Backend(format!("extracting the received files: {e}")))??;
        log::info!(
            "clipboard: received files extracted under {}",
            self.root.display()
        );
        self.inner.write(&local).await
    }

    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
        match self.inner.changed().await? {
            Some(snapshot) => Ok(Some(self.bundled(snapshot).await)),
            None => Ok(None),
        }
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
            max_files: 8,
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
    #[test]
    fn an_invalid_portal_session_requests_a_backend_restart() {
        assert!(
            Error::Backend(
                "set portal selection: Portal request failed: org.freedesktop.zbus.Error: Invalid session"
                    .into()
            )
            .needs_backend_restart()
        );
        assert!(!Error::Backend("clipboard is busy".into()).needs_backend_restart());
        assert!(!Error::TooLarge.needs_backend_restart());
    }
}

#[cfg(test)]
mod file_bundling_tests {
    use std::{cell::Cell, path::PathBuf, rc::Rc};

    use super::*;

    fn temp(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-bundling-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn limits(max_files: usize) -> Rc<Cell<Limits>> {
        Rc::new(Cell::new(Limits {
            max_files,
            ..Limits::default()
        }))
    }

    /// The copy that leaves: the files alone as a bundle; the file manager's
    /// text (this PC's paths) stays behind.
    #[tokio::test(flavor = "current_thread")]
    async fn a_file_copy_is_read_as_a_bundle_without_the_paths_text() {
        let src = temp("src");
        std::fs::write(src.join("a.txt"), b"conteudo").unwrap();
        let local = SharedClipboard::new();
        local.set_local(ClipboardSnapshot::from_items([
            (Format::Files, files::local_item(&[src.join("a.txt")])),
            (
                Format::Text,
                src.join("a.txt").to_string_lossy().as_bytes().to_vec(),
            ),
        ]));
        let mut backend = FileBundling::with_root(local, limits(1 << 20), temp("root"));
        let read = backend.read().await.unwrap();
        assert_eq!(read.formats().collect::<Vec<_>>(), vec![Format::Files]);
        let entries = files::decode(read.get(Format::Files).unwrap()).unwrap();
        assert_eq!(entries[0].data.as_deref(), Some(b"conteudo".as_slice()));
    }

    /// Over the limit the files stay home, but what else was copied (the
    /// paths as text) is still shared.
    #[tokio::test(flavor = "current_thread")]
    async fn a_copy_over_the_limit_shares_the_text_only() {
        let src = temp("big");
        std::fs::write(src.join("big.bin"), vec![7u8; 4096]).unwrap();
        let local = SharedClipboard::new();
        local.set_local(ClipboardSnapshot::from_items([
            (Format::Files, files::local_item(&[src.join("big.bin")])),
            (Format::Text, b"/somewhere/big.bin".to_vec()),
        ]));
        let mut backend = FileBundling::with_root(local, limits(1024), temp("root"));
        let read = backend.read().await.unwrap();
        assert_eq!(read.formats().collect::<Vec<_>>(), vec![Format::Text]);
    }

    /// Writing a received bundle puts this PC's extracted paths on the
    /// clipboard, and reading that back yields the very same bundle, so the
    /// sync layer recognises the echo by hash.
    #[tokio::test(flavor = "current_thread")]
    async fn writing_a_bundle_extracts_it_and_reads_back_identically() {
        let src = temp("src");
        std::fs::create_dir_all(src.join("pasta")).unwrap();
        std::fs::write(src.join("pasta/x.txt"), b"x").unwrap();
        let bundle = files::pack(&[src.join("pasta")], 1 << 20).unwrap();
        let received = ClipboardSnapshot::from_items([(Format::Files, bundle)]);

        let root = temp("root");
        let local = SharedClipboard::new();
        let mut backend = FileBundling::with_root(local.clone(), limits(1 << 20), root.clone());
        backend.write(&received).await.unwrap();

        let on_clipboard = local.current();
        let paths = files::local_paths(on_clipboard.get(Format::Files).unwrap());
        assert_eq!(paths.len(), 1);
        assert!(paths[0].starts_with(&root), "{paths:?}");
        assert_eq!(std::fs::read(paths[0].join("x.txt")).unwrap(), b"x");
        assert!(
            on_clipboard.get(Format::Text).is_some(),
            "paths as text for editors"
        );

        let echo = backend.read().await.unwrap();
        assert_eq!(echo.hash(), received.hash());
    }

    /// A hostile bundle is refused before anything is written.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hostile_bundle_is_refused_and_writes_nothing() {
        let root = temp("root");
        let evil = files::encode(&[files::Entry {
            path: "../escaped.txt".into(),
            data: Some(b"x".to_vec()),
        }]);
        let mut backend =
            FileBundling::with_root(SharedClipboard::new(), limits(1 << 20), root.clone());
        let result = backend
            .write(&ClipboardSnapshot::from_items([(Format::Files, evil)]))
            .await;
        assert!(result.is_err());
        assert!(!root.parent().unwrap().join("escaped.txt").exists());
        assert_eq!(std::fs::read_dir(&root).map(|d| d.count()).unwrap_or(0), 0);
    }

    /// Without files the wrapper changes nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn text_and_images_pass_through_unchanged() {
        let local = SharedClipboard::new();
        let copy = ClipboardSnapshot::from_items([
            (Format::Text, b"oi".to_vec()),
            (Format::Png, vec![0x89, b'P', b'N', b'G']),
        ]);
        local.set_local(copy.clone());
        let mut backend = FileBundling::with_root(local.clone(), limits(1 << 20), temp("root"));
        assert_eq!(backend.read().await.unwrap(), copy);
        backend.write(&copy).await.unwrap();
        assert_eq!(local.current(), copy);
    }
}
