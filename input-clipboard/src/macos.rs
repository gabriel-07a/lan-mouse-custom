//! macOS clipboard backend.
//!
//! `NSPasteboard` has no change notification API, so [`changed`] polls
//! `changeCount` every 250 ms. The canonical HTML and PNG representations map
//! directly to `public.html` and `public.png`.

use std::time::Duration;

use async_trait::async_trait;
use objc2_app_kit::{
    NSPasteboard, NSPasteboardType, NSPasteboardTypeHTML, NSPasteboardTypePNG,
    NSPasteboardTypeString,
};
use objc2_foundation::{NSData, NSString};

use crate::{Clipboard, ClipboardSnapshot, Error, Format};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CONCEALED_TYPE: &str = "org.nspasteboard.ConcealedType";
const MAX_DATA_BYTES: usize = 16 * 1024 * 1024;

pub struct MacOsClipboard {
    last_change: isize,
    own_change: isize,
}

impl MacOsClipboard {
    pub fn new() -> Self {
        let pasteboard = NSPasteboard::generalPasteboard();
        Self {
            last_change: pasteboard.changeCount(),
            own_change: 0,
        }
    }

    fn is_secret(pasteboard: &NSPasteboard) -> bool {
        pasteboard
            .types()
            .map(|types| {
                types
                    .to_vec()
                    .iter()
                    .any(|kind| kind.to_string() == CONCEALED_TYPE)
            })
            .unwrap_or(false)
    }

    fn type_string() -> &'static NSPasteboardType {
        // SAFETY: the constant is an immutable null-terminated string owned by
        // AppKit for the lifetime of the process.
        unsafe { NSPasteboardTypeString }
    }

    fn type_html() -> &'static NSPasteboardType {
        unsafe { NSPasteboardTypeHTML }
    }

    fn type_png() -> &'static NSPasteboardType {
        unsafe { NSPasteboardTypePNG }
    }
}

impl Default for MacOsClipboard {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl Clipboard for MacOsClipboard {
    fn backend_name(&self) -> &'static str {
        "macos"
    }

    async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
        let pasteboard = NSPasteboard::generalPasteboard();
        if Self::is_secret(&pasteboard) {
            log::debug!("macos clipboard is marked as concealed, skipping");
            return Ok(ClipboardSnapshot::default());
        }

        let mut snapshot = ClipboardSnapshot::new();
        if let Some(text) = pasteboard.stringForType(Self::type_string()) {
            let text = text.to_string();
            if !text.is_empty() {
                snapshot.insert(Format::Text, text.into_bytes());
            }
        }
        if let Some(data) = pasteboard.dataForType(Self::type_html()) {
            if data.length() > MAX_DATA_BYTES {
                log::debug!("macos HTML clipboard entry is too large, skipping");
                return Ok(ClipboardSnapshot::default());
            }
            let bytes = data.to_vec();
            if !bytes.is_empty() {
                snapshot.insert(Format::Html, bytes);
            }
        }
        if let Some(data) = pasteboard.dataForType(Self::type_png()) {
            if data.length() > MAX_DATA_BYTES {
                log::debug!("macos PNG clipboard entry is too large, skipping");
                return Ok(ClipboardSnapshot::default());
            }
            let bytes = data.to_vec();
            if !bytes.is_empty() {
                snapshot.insert(Format::Png, bytes);
            }
        }
        Ok(snapshot)
    }

    async fn write(&mut self, snapshot: &ClipboardSnapshot) -> Result<(), Error> {
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();

        if let Some(text) = snapshot.get(Format::Text) {
            let text = NSString::from_str(&String::from_utf8_lossy(text));
            if !pasteboard.setString_forType(&text, Self::type_string()) {
                return Err(Error::Backend(
                    "could not write text to NSPasteboard".into(),
                ));
            }
        }
        if let Some(html) = snapshot.get(Format::Html) {
            let data = NSData::with_bytes(html);
            if !pasteboard.setData_forType(Some(&data), Self::type_html()) {
                log::warn!("could not write HTML to NSPasteboard");
            }
        }
        if let Some(png) = snapshot.get(Format::Png) {
            let data = NSData::with_bytes(png);
            if !pasteboard.setData_forType(Some(&data), Self::type_png()) {
                log::warn!("could not write PNG to NSPasteboard");
            }
        }

        self.own_change = pasteboard.changeCount();
        Ok(())
    }

    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let pasteboard = NSPasteboard::generalPasteboard();
            let change = pasteboard.changeCount();
            if change == self.last_change {
                continue;
            }
            self.last_change = change;
            if change == self.own_change {
                continue;
            }
            return self.read().await.map(Some);
        }
    }
}
