//! Linux clipboard backend using `wlr-data-control` (wl-clipboard-rs).
//!
//! Works on wlroots compositors (Sway, Hyprland, …) and on KDE where the
//! compositor exposes the data-control protocol. GNOME does not, so this
//! backend is only tried after the portal fails.

use std::{
    io::Read,
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender as StdSender},
    },
    thread,
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use wl_clipboard_rs::{
    copy::{MimeSource, MimeType as CopyMime, Options, Source},
    paste::Seat,
    watch::{ClipboardEvent, ClipboardType, Watcher},
};

use crate::{Clipboard, ClipboardSnapshot, Error, Format};

const MAX_MIME_BYTES: usize = 16 * 1024 * 1024;
const SECRET_HINT: &str = "x-kde-passwordManagerHint";

pub struct WaylandClipboard {
    changes: UnboundedReceiver<ClipboardSnapshot>,
    last: Arc<Mutex<Option<ClipboardSnapshot>>>,
    last_written: Arc<Mutex<Option<[u8; 32]>>>,
    cancel: Option<wl_clipboard_rs::watch::CancelHandle>,
    thread: Option<thread::JoinHandle<()>>,
}

impl WaylandClipboard {
    pub fn new() -> Result<Self, Error> {
        let (change_tx, change_rx) = unbounded_channel();
        let last = Arc::new(Mutex::new(None));
        let last_written: Arc<Mutex<Option<[u8; 32]>>> = Arc::new(Mutex::new(None));
        let thread_last = last.clone();
        let thread_written = last_written.clone();
        let (ready_tx, ready_rx) =
            mpsc::channel::<Result<wl_clipboard_rs::watch::CancelHandle, String>>();

        let thread = thread::Builder::new()
            .name("lan-mouse-wayland-clipboard".into())
            .spawn(move || {
                run_watcher(thread_last, thread_written, change_tx, ready_tx);
            })
            .map_err(|e| {
                Error::Backend(format!("could not start the Wayland clipboard thread: {e}"))
            })?;

        let cancel = match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(cancel)) => cancel,
            Ok(Err(e)) => return Err(Error::Backend(e)),
            Err(_) => {
                return Err(Error::Backend(
                    "the Wayland clipboard thread did not start".into(),
                ));
            }
        };

        Ok(Self {
            changes: change_rx,
            last,
            last_written,
            cancel: Some(cancel),
            thread: Some(thread),
        })
    }
}

impl Drop for WaylandClipboard {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_watcher(
    last: Arc<Mutex<Option<ClipboardSnapshot>>>,
    last_written: Arc<Mutex<Option<[u8; 32]>>>,
    changes: UnboundedSender<ClipboardSnapshot>,
    ready: StdSender<Result<wl_clipboard_rs::watch::CancelHandle, String>>,
) {
    let mut watcher = match Watcher::new(ClipboardType::Regular, Seat::Unspecified) {
        Ok(watcher) => watcher,
        Err(e) => {
            let _ = ready.send(Err(format!("wlr-data-control unavailable: {e}")));
            return;
        }
    };
    let _ = ready.send(Ok(watcher.cancel_handle()));

    loop {
        match watcher.next_event() {
            Ok(Some(ClipboardEvent::Changed {
                mime_types,
                mut offer,
                ..
            })) => {
                if mime_types
                    .iter()
                    .any(|mime| mime.eq_ignore_ascii_case(SECRET_HINT))
                {
                    log::debug!("wayland clipboard is marked as secret, skipping");
                    continue;
                }
                let Some(snapshot) = read_offer(&mime_types, &mut offer) else {
                    continue;
                };
                if snapshot.is_empty() {
                    continue;
                }
                if *last_written.lock().expect("lock") == Some(snapshot.hash()) {
                    continue;
                }
                *last.lock().expect("lock") = Some(snapshot.clone());
                let _ = changes.send(snapshot);
            }
            Ok(Some(ClipboardEvent::Cleared { .. })) => {}
            Ok(None) => break,
            Err(e) => {
                log::debug!("Wayland clipboard watcher stopped: {e}");
                break;
            }
        }
    }
}

fn read_offer(
    mime_types: &[String],
    offer: &mut wl_clipboard_rs::watch::Offer<'_>,
) -> Option<ClipboardSnapshot> {
    let mut snapshot = ClipboardSnapshot::new();
    for (mime, format) in [
        ("text/plain;charset=utf-8", Format::Text),
        ("text/plain", Format::Text),
        ("text/html", Format::Html),
        ("image/png", Format::Png),
    ] {
        if snapshot.get(format).is_some() || !mime_types.iter().any(|m| m == mime) {
            continue;
        }
        let Ok(reader) = offer.receive(mime) else {
            continue;
        };
        let mut data = Vec::new();
        if reader
            .take(MAX_MIME_BYTES as u64)
            .read_to_end(&mut data)
            .is_ok()
            && !data.is_empty()
        {
            snapshot.insert(format, data);
        }
    }
    (!snapshot.is_empty()).then_some(snapshot)
}

#[async_trait(?Send)]
impl Clipboard for WaylandClipboard {
    fn backend_name(&self) -> &'static str {
        "wayland-data-control"
    }

    async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
        Ok(self.last.lock().expect("lock").clone().unwrap_or_default())
    }

    async fn write(&mut self, snapshot: &ClipboardSnapshot) -> Result<(), Error> {
        let real_snapshot = snapshot.clone();
        *self.last_written.lock().expect("lock") = Some(real_snapshot.hash());

        let mut sources = Vec::new();
        for (format, data) in snapshot.iter() {
            sources.push(MimeSource {
                source: Source::Bytes(data.to_vec().into_boxed_slice()),
                mime_type: CopyMime::Specific(format.mime().to_string()),
            });
            if format == Format::Text {
                sources.push(MimeSource {
                    source: Source::Bytes(data.to_vec().into_boxed_slice()),
                    mime_type: CopyMime::Specific("text/plain".to_string()),
                });
            }
        }
        if sources.is_empty() {
            return Ok(());
        }

        tokio::task::spawn_blocking(move || {
            Options::new()
                .copy_multi(sources)
                .map_err(|e| Error::Backend(format!("wlr-data-control copy: {e}")))
        })
        .await
        .map_err(|e| Error::Backend(format!("clipboard copy task failed: {e}")))??;

        *self.last.lock().expect("lock") = Some(real_snapshot);
        Ok(())
    }

    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
        Ok(self.changes.recv().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_preferences_match_the_canonical_formats() {
        let snapshot = ClipboardSnapshot::from_items([
            (Format::Text, b"hello".to_vec()),
            (Format::Html, b"<b>hello</b>".to_vec()),
        ]);
        assert_eq!(snapshot.get(Format::Text), Some(&b"hello"[..]));
        assert_eq!(snapshot.get(Format::Html), Some(&b"<b>hello</b>"[..]));
    }
}
