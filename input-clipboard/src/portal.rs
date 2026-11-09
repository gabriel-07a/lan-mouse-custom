//! Linux clipboard backend using the XDG desktop portal.
//!
//! GNOME 50 and newer expose `org.freedesktop.portal.Clipboard`. The portal
//! requires a `RemoteDesktop` session and a one-time user consent ("Allow
//! Clipboard Access"); the consent is persisted with a restore token.
//!
//! Clipboard data is requested lazily by the portal through file descriptors,
//! which is why the actual work happens in a background task.

use std::{
    cell::RefCell,
    env, fs,
    io::{self, Read, Write},
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use ashpd::desktop::{
    PersistMode, Session,
    clipboard::{Clipboard as AshpdClipboard, RequestClipboardOptions, SetSelectionOptions},
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions},
};
use futures::StreamExt;
use tokio::{
    sync::mpsc::{UnboundedReceiver, unbounded_channel},
    task::JoinHandle,
};

use crate::{Clipboard, ClipboardSnapshot, Error, Format};

const TOKEN_FILE: &str = "clipboard.token";
const START_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_MIME_BYTES: usize = 16 * 1024 * 1024;
const PIPE_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-format read budget. A slow owner must not block the whole selection.
const MIME_TIMEOUT: Duration = Duration::from_secs(2);
/// Total budget for reading one clipboard selection.
const SELECTION_BUDGET: Duration = Duration::from_secs(4);

/// Check whether the desktop exposes the clipboard portal.
///
/// This only creates the D-Bus proxy; it does not start a session and does
/// not show a permission dialog, so it is safe to call from diagnostics.
pub async fn available_version() -> Result<u32, Error> {
    let clipboard = AshpdClipboard::new()
        .await
        .map_err(|e| Error::Backend(format!("Clipboard portal: {e}")))?;
    Ok(clipboard.version())
}

/// Whether a restore token (user consent) has been saved for the portal.
///
/// Read-only on purpose: diagnostics call this and must not touch the state
/// directory (migration happens lazily on the first runtime read).
pub fn restore_token_present() -> bool {
    read_token_without_migration().is_some()
}

/// Read a token the runtime can use without migrating legacy cache tokens.
fn read_token_without_migration() -> Option<String> {
    let state = token_dir().map(|dir| dir.join(TOKEN_FILE));
    if let Some(token) = state.as_deref().and_then(read_token_file) {
        return Some(token);
    }
    legacy_token_dir()
        .map(|dir| dir.join(TOKEN_FILE))
        .as_deref()
        .and_then(read_token_file)
}

pub struct PortalClipboard {
    clipboard: Rc<AshpdClipboard>,
    session: Session<RemoteDesktop>,
    shared: Rc<RefCell<Option<ClipboardSnapshot>>>,
    last_mimes: Rc<RefCell<Vec<String>>>,
    changes: UnboundedReceiver<ClipboardSnapshot>,
    task: JoinHandle<()>,
    _remote_desktop: RemoteDesktop,
}

impl PortalClipboard {
    pub async fn new() -> Result<Self, Error> {
        let remote_desktop = RemoteDesktop::new()
            .await
            .map_err(|e| Error::Backend(format!("RemoteDesktop portal: {e}")))?;
        let session = remote_desktop
            .create_session(Default::default())
            .await
            .map_err(|e| Error::Backend(format!("create RemoteDesktop session: {e}")))?;

        let clipboard = Rc::new(
            AshpdClipboard::new()
                .await
                .map_err(|e| Error::Backend(format!("Clipboard portal: {e}")))?,
        );

        // Must be requested before `Start`; afterwards the portal answers
        // `Invalid state`.
        clipboard
            .request(&session, RequestClipboardOptions::default())
            .await
            .map_err(|e| Error::Backend(format!("request clipboard: {e}")))?;

        let restore_token = read_token();
        let options = SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(restore_token.as_deref());
        remote_desktop
            .select_devices(&session, options)
            .await
            .map_err(|e| Error::Backend(format!("select devices: {e}")))?;

        let response = tokio::time::timeout(START_TIMEOUT, async {
            remote_desktop
                .start(&session, None, Default::default())
                .await?
                .response()
        })
        .await
        .map_err(|_| Error::Backend("timed out waiting for the portal permission dialog".into()))?
        .map_err(|e| Error::Backend(format!("start RemoteDesktop session: {e}")))?;

        if !response.is_clipboard_enabled() {
            return Err(Error::Backend(
                "the portal did not allow clipboard access (enable \"Allow Clipboard Access\")"
                    .into(),
            ));
        }
        // Save the token only after consent, so a denied clipboard prompts
        // again on the next start instead of silently staying disabled.
        if let Some(token) = response.restore_token() {
            if let Err(e) = write_token(token) {
                log::warn!("could not save the clipboard restore token: {e}");
            }
        }

        let shared: Rc<RefCell<Option<ClipboardSnapshot>>> = Rc::new(RefCell::new(None));
        let last_mimes: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let (tx, changes) = unbounded_channel();
        let task_shared = shared.clone();
        let task_mimes = last_mimes.clone();
        let task_clipboard = clipboard.clone();
        let task = tokio::task::spawn_local(async move {
            let owner_changes = match task_clipboard
                .receive_selection_owner_changed::<RemoteDesktop>()
                .await
            {
                Ok(stream) => stream,
                Err(e) => {
                    log::warn!("could not watch the portal clipboard owner: {e}");
                    return;
                }
            };
            let transfers = match task_clipboard
                .receive_selection_transfer::<RemoteDesktop>()
                .await
            {
                Ok(stream) => stream,
                Err(e) => {
                    log::warn!("could not watch portal clipboard transfers: {e}");
                    return;
                }
            };
            tokio::pin!(owner_changes);
            tokio::pin!(transfers);
            loop {
                tokio::select! {
                    event = owner_changes.next() => match event {
                        Some((session, event)) => {
                            // Do not skip events where this session owns the
                            // selection: after we apply a remote snapshot we
                            // own it, and on GNOME the flag can stay true for
                            // later user copies — which silently dropped the
                            // whole Ubuntu -> Windows direction. The echo is
                            // dropped by the hash check in the sync layer.
                            if event.session_is_owner() == Some(true) {
                                log::trace!("portal clipboard owner is us, re-reading");
                            }
                            if event.mime_types().iter().any(|mime| is_secret_mime(mime)) {
                                log::debug!("portal clipboard is marked as secret, skipping");
                                task_mimes.borrow_mut().clear();
                                continue;
                            }
                            *task_mimes.borrow_mut() = event.mime_types().to_vec();
                            if let Some(snapshot) =
                                read_selection(&task_clipboard, &session, event.mime_types()).await
                            {
                                if !snapshot.is_empty() {
                                    let _ = tx.send(snapshot);
                                }
                            }
                        }
                        None => break,
                    },
                    event = transfers.next() => match event {
                        Some((session, mime, serial)) => {
                            let data = task_shared
                                .borrow()
                                .as_ref()
                                .and_then(|snapshot| data_for_mime(snapshot, &mime));
                            let ok = match data {
                                Some(bytes) => serve_transfer(&task_clipboard, &session, serial, bytes)
                                    .await
                                    .is_ok(),
                                None => false,
                            };
                            let _ = task_clipboard
                                .selection_write_done(&session, serial, ok)
                                .await;
                        }
                        None => break,
                    },
                }
            }
        });

        Ok(Self {
            clipboard,
            session,
            shared,
            last_mimes,
            changes,
            task,
            _remote_desktop: remote_desktop,
        })
    }
}

impl Drop for PortalClipboard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait::async_trait(?Send)]
impl Clipboard for PortalClipboard {
    fn backend_name(&self) -> &'static str {
        "portal"
    }

    async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
        // The backend is driven by the portal's owner-change signal; a direct
        // read is used when a peer connects, to push the current clipboard.
        let mimes = self.last_mimes.borrow().clone();
        Ok(read_selection(&self.clipboard, &self.session, &mimes)
            .await
            .unwrap_or_default())
    }

    async fn write(&mut self, snapshot: &ClipboardSnapshot) -> Result<(), Error> {
        let mut mimes = snapshot
            .formats()
            .map(|format| format.mime())
            .collect::<Vec<_>>();
        if snapshot.get(Format::Text).is_some() {
            mimes.push("text/plain");
        }
        self.clipboard
            .set_selection(
                &self.session,
                SetSelectionOptions::default().set_mime_types(&mimes),
            )
            .await
            .map_err(|e| Error::Backend(format!("set portal selection: {e}")))?;
        // Only serve the snapshot once the selection really points at us;
        // publishing it earlier would answer transfer requests for content the
        // portal never accepted.
        *self.shared.borrow_mut() = Some(snapshot.clone());
        Ok(())
    }

    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
        match self.changes.recv().await {
            Some(snapshot) => Ok(Some(snapshot)),
            None => Ok(None),
        }
    }
}

impl std::fmt::Debug for PortalClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PortalClipboard")
    }
}

/// Formats we understand, in the order that is cheapest to read.
fn canonical_mimes() -> Vec<String> {
    [
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "text/html",
        "image/png",
    ]
    .iter()
    .map(|mime| (*mime).to_string())
    .collect()
}

/// Supported MIME types first (text, then HTML, then image), then the rest.
fn ordered_mimes(mimes: &[String]) -> Vec<String> {
    let rank = |mime: &str| match format_for_mime(mime) {
        Some(Format::Text) => 0,
        Some(Format::Html) => 1,
        Some(Format::Png) => 2,
        None => 3,
    };
    let mut ordered = mimes.to_vec();
    ordered.sort_by_key(|mime| rank(mime));
    ordered.dedup();
    ordered
}

async fn read_selection(
    clipboard: &AshpdClipboard,
    session: &Session<RemoteDesktop>,
    mime_types: &[String],
) -> Option<ClipboardSnapshot> {
    // An event without (usable) MIME types still usually means "the clipboard
    // changed"; try the canonical list instead of giving up.
    let mut mimes = ordered_mimes(mime_types);
    if !mimes.iter().any(|mime| format_for_mime(mime).is_some()) {
        mimes = canonical_mimes();
    }

    let deadline = Instant::now() + SELECTION_BUDGET;
    let mut snapshot = ClipboardSnapshot::new();
    for mime in mimes {
        let Some(format) = format_for_mime(&mime) else {
            continue;
        };
        if snapshot.get(format).is_some() {
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            log::debug!("portal clipboard read budget exhausted");
            break;
        }
        let read = tokio::time::timeout(remaining.min(MIME_TIMEOUT), async {
            let fd = clipboard
                .selection_read(session, &mime)
                .await
                .map_err(|e| Error::Backend(format!("selection read: {e}")))?;
            read_fd(fd.into(), MAX_MIME_BYTES).await
        })
        .await;
        match read {
            Ok(Ok(bytes)) if !bytes.is_empty() => {
                snapshot.insert(format, bytes);
            }
            Ok(Err(e)) => log::debug!("portal clipboard read of {mime} failed: {e}"),
            Err(_) => log::debug!("portal clipboard read of {mime} timed out"),
            Ok(Ok(_)) => {}
        }
    }
    (!snapshot.is_empty()).then_some(snapshot)
}

async fn serve_transfer(
    clipboard: &AshpdClipboard,
    session: &Session<RemoteDesktop>,
    serial: u32,
    bytes: Vec<u8>,
) -> Result<(), Error> {
    let result = tokio::time::timeout(PIPE_TIMEOUT, async {
        let fd = clipboard
            .selection_write(session, serial)
            .await
            .map_err(|e| Error::Backend(format!("selection write: {e}")))?;
        write_fd(fd.into(), bytes).await
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => Err(Error::Backend(
            "timed out reading or writing the clipboard pipe".into(),
        )),
    }
}

/// KDE and some password managers advertise this hint next to a secret.
fn is_secret_mime(mime: &str) -> bool {
    mime.eq_ignore_ascii_case("x-kde-passwordManagerHint")
        || mime.eq_ignore_ascii_case("application/x-kde-passwordManagerHint")
}

fn format_for_mime(mime: &str) -> Option<Format> {
    let base = mime.split(';').next().unwrap_or(mime).trim();
    match base {
        "text/plain" | "UTF8_STRING" => Some(Format::Text),
        "text/html" => Some(Format::Html),
        "image/png" => Some(Format::Png),
        _ => None,
    }
}

fn data_for_mime(snapshot: &ClipboardSnapshot, mime: &str) -> Option<Vec<u8>> {
    format_for_mime(mime).and_then(|format| snapshot.get(format).map(|bytes| bytes.to_vec()))
}

async fn read_fd(fd: OwnedFd, limit: usize) -> Result<Vec<u8>, Error> {
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::from(fd);
        let mut out = Vec::new();
        // Read one byte past the limit to distinguish "exactly the limit" from
        // an oversized payload that must not be forwarded truncated.
        file.take((limit as u64).saturating_add(1))
            .read_to_end(&mut out)?;
        if out.len() > limit {
            return Err(std::io::Error::other(format!(
                "clipboard payload exceeds {limit} bytes"
            )));
        }
        Ok::<_, std::io::Error>(out)
    })
    .await
    .map_err(|e| Error::Backend(format!("clipboard pipe task failed: {e}")))?
    .map_err(|e| Error::Backend(format!("clipboard pipe read failed: {e}")))
}

async fn write_fd(fd: OwnedFd, bytes: Vec<u8>) -> Result<(), Error> {
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::from(fd);
        file.write_all(&bytes)?;
        file.flush()?;
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|e| Error::Backend(format!("clipboard pipe task failed: {e}")))?
    .map_err(|e| Error::Backend(format!("clipboard pipe write failed: {e}")))
}

/// Directory holding portal tokens: `$XDG_STATE_HOME/lan-mouse`, falling back
/// to `~/.local/state/lan-mouse`.
///
/// State, not cache: cache directories are routinely purged by cleanup tools
/// and losing the token makes the portal ask for clipboard permission again.
fn token_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(base.join("lan-mouse"))
}

/// Token location used by builds that stored consent in the cache directory.
fn legacy_token_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("lan-mouse"))
}

fn read_token_file(path: &Path) -> Option<String> {
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Read the saved consent token, migrating a token left in the legacy cache
/// directory so an already-granted consent survives the move.
fn read_token_from(state_dir: Option<&Path>, legacy_dir: Option<&Path>) -> Option<String> {
    let state_dir = state_dir?;
    let path = state_dir.join(TOKEN_FILE);
    if let Some(token) = read_token_file(&path) {
        return Some(token);
    }
    let legacy_path = legacy_dir?.join(TOKEN_FILE);
    let token = read_token_file(&legacy_path)?;
    log::info!(
        "migrating clipboard restore token from {} to {}",
        legacy_path.display(),
        path.display()
    );
    if let Err(e) = write_token_to(state_dir, TOKEN_FILE, &token) {
        log::warn!("could not migrate the clipboard restore token: {e}");
    } else if let Err(e) = fs::remove_file(&legacy_path) {
        // Do not keep a world-readable copy of a bearer capability around.
        log::debug!("could not remove the legacy restore token: {e}");
    }
    Some(token)
}

fn read_token() -> Option<String> {
    read_token_from(token_dir().as_deref(), legacy_token_dir().as_deref())
}

/// Save the consent token atomically with owner-only permissions.
///
/// Tokens are bearer capabilities: whoever can read one can restore the
/// portal session without a permission dialog, hence 0600 for the file and
/// 0700 for the directory.
fn write_token(token: &str) -> io::Result<()> {
    let dir = token_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "neither XDG_STATE_HOME nor HOME is set",
        )
    })?;
    write_token_to(&dir, TOKEN_FILE, token)
}

fn write_token_to(dir: &Path, name: &str, token: &str) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

    let path = dir.join(name);
    // Write next to the destination and rename: a crash mid-write must not
    // leave a truncated token that would then be sent back to the portal.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    // The mode above only applies when the file is created; tighten an
    // existing file as well.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_types_map_to_canonical_formats() {
        assert_eq!(
            format_for_mime("text/plain;charset=utf-8"),
            Some(Format::Text)
        );
        assert_eq!(format_for_mime("text/plain"), Some(Format::Text));
        assert_eq!(format_for_mime("UTF8_STRING"), Some(Format::Text));
        assert_eq!(format_for_mime("text/html"), Some(Format::Html));
        assert_eq!(format_for_mime("image/png"), Some(Format::Png));
        assert_eq!(format_for_mime("image/tiff"), None);
    }

    #[test]
    fn secret_mime_hints_are_detected() {
        assert!(is_secret_mime("x-kde-passwordManagerHint"));
        assert!(is_secret_mime("application/x-kde-passwordManagerHint"));
        assert!(!is_secret_mime("text/plain"));
    }

    #[test]
    fn data_for_mime_uses_the_canonical_format() {
        let snapshot = ClipboardSnapshot::from_items([
            (Format::Text, b"hello".to_vec()),
            (Format::Html, b"<b>hello</b>".to_vec()),
        ]);
        assert_eq!(
            data_for_mime(&snapshot, "text/plain;charset=utf-8"),
            Some(b"hello".to_vec())
        );
        assert_eq!(
            data_for_mime(&snapshot, "text/html"),
            Some(b"<b>hello</b>".to_vec())
        );
        assert_eq!(data_for_mime(&snapshot, "image/png"), None);
    }
}

#[cfg(test)]
mod mime_tests {
    use super::*;

    #[test]
    fn supported_mimes_come_first_and_in_cheap_order() {
        let mimes = vec![
            "image/png".to_string(),
            "text/html".to_string(),
            "application/x-qt-image".to_string(),
            "UTF8_STRING".to_string(),
        ];
        let ordered = ordered_mimes(&mimes);
        assert_eq!(ordered.first().unwrap(), "UTF8_STRING");
        assert_eq!(ordered[1], "text/html");
        assert_eq!(ordered[2], "image/png");
        // unsupported entries are kept at the end, not dropped
        assert!(ordered.contains(&"application/x-qt-image".to_string()));
    }

    #[test]
    fn canonical_list_has_text_before_image() {
        let mimes = canonical_mimes();
        let text = mimes.iter().position(|m| m.contains("text/plain")).unwrap();
        let png = mimes.iter().position(|m| m == "image/png").unwrap();
        assert!(text < png);
    }

    #[test]
    fn an_event_without_mimes_falls_back_to_the_canonical_list() {
        let ordered = ordered_mimes(&[]);
        assert!(ordered.is_empty());
        // read_selection replaces an unusable list with canonical_mimes();
        // this asserts the condition it uses.
        assert!(
            !canonical_mimes()
                .iter()
                .all(|mime| format_for_mime(mime).is_none())
        );
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "lan-mouse-clipboard-token-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn tokens_are_written_atomically_with_owner_only_permissions() {
        let dir = scratch_dir("write");
        write_token_to(&dir, TOKEN_FILE, "token-value").unwrap();

        let path = dir.join(TOKEN_FILE);
        assert_eq!(read_token_file(&path).as_deref(), Some("token-value"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // The temporary file is renamed, not left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_tokens_are_treated_as_missing() {
        let dir = scratch_dir("empty");
        write_token_to(&dir, TOKEN_FILE, " \n\t").unwrap();
        assert_eq!(read_token_file(&dir.join(TOKEN_FILE)), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_cache_token_is_migrated_once() {
        let state = scratch_dir("state");
        let legacy = scratch_dir("legacy");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(TOKEN_FILE), "legacy-token").unwrap();

        assert_eq!(
            read_token_from(Some(&state), Some(&legacy)).as_deref(),
            Some("legacy-token")
        );
        assert_eq!(
            read_token_file(&state.join(TOKEN_FILE)).as_deref(),
            Some("legacy-token")
        );
        // The legacy copy is removed once migration succeeded.
        assert!(!legacy.join(TOKEN_FILE).exists());

        // The migrated (state) token wins over a stale legacy one.
        fs::write(state.join(TOKEN_FILE), "current-token").unwrap();
        fs::write(legacy.join(TOKEN_FILE), "legacy-token").unwrap();
        assert_eq!(
            read_token_from(Some(&state), Some(&legacy)).as_deref(),
            Some("current-token")
        );

        fs::remove_dir_all(&state).unwrap();
        fs::remove_dir_all(&legacy).unwrap();
    }
}
