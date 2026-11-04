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
    io::{Read, Write},
    os::fd::OwnedFd,
    path::PathBuf,
    rc::Rc,
    time::Duration,
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
pub fn restore_token_present() -> bool {
    read_token().is_some()
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
                            if event.session_is_owner() == Some(true) {
                                continue;
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
        *self.shared.borrow_mut() = Some(snapshot.clone());
        self.clipboard
            .set_selection(
                &self.session,
                SetSelectionOptions::default().set_mime_types(&mimes),
            )
            .await
            .map_err(|e| Error::Backend(format!("set portal selection: {e}")))?;
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

async fn read_selection(
    clipboard: &AshpdClipboard,
    session: &Session<RemoteDesktop>,
    mime_types: &[String],
) -> Option<ClipboardSnapshot> {
    if mime_types.is_empty() {
        return None;
    }
    let mut snapshot = ClipboardSnapshot::new();
    for mime in mime_types {
        let Some(format) = format_for_mime(mime) else {
            continue;
        };
        if snapshot.get(format).is_some() {
            continue;
        }
        let read = tokio::time::timeout(PIPE_TIMEOUT, async {
            let fd = clipboard
                .selection_read(session, mime)
                .await
                .map_err(|e| Error::Backend(format!("selection read: {e}")))?;
            read_fd(fd.into(), MAX_MIME_BYTES).await
        })
        .await;
        if let Ok(Ok(bytes)) = read {
            if !bytes.is_empty() {
                snapshot.insert(format, bytes);
            }
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
        file.take(limit as u64).read_to_end(&mut out)?;
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

fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("lan-mouse").join(TOKEN_FILE))
}

fn read_token() -> Option<String> {
    let path = token_path()?;
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn write_token(token: &str) -> std::io::Result<()> {
    let Some(path) = token_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, token)
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
