//! Windows clipboard backend.
//!
//! Uses a message-only window with `AddClipboardFormatListener` for change
//! notifications and the `CF_*` / registered formats for the data itself.
//! Images are converted between `CF_DIB`/`CF_DIBV5` and the canonical PNG
//! representation by [`crate::dib`].

use std::{sync::Mutex, time::Duration};

use async_trait::async_trait;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use windows::{
    Win32::{
        Foundation::{
            ERROR_CLASS_ALREADY_EXISTS, GetLastError, GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND,
            LPARAM, LRESULT, WPARAM,
        },
        System::{
            DataExchange::{
                AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
                GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
                RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
            },
            LibraryLoader::GetModuleHandleW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        },
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
            RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLIPBOARDUPDATE,
            WNDCLASSW,
        },
    },
    core::{PCWSTR, w},
};

use crate::{
    Clipboard, ClipboardSnapshot, Error, Format, cf_html, dib,
    text::{crlf_to_lf, lf_to_crlf},
};

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_DIBV5: u32 = 17;
const MAX_GLOBAL_BYTES: usize = 16 * 1024 * 1024;
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Sender of the clipboard-change notification, set while the listener
/// message window is alive.
///
/// A Mutex instead of a OnceLock so a listener that exits can be replaced: a
/// failed startup or a dead message loop must stay retryable (F15).
static NOTIFY: Mutex<Option<UnboundedSender<()>>> = Mutex::new(None);

pub struct WindowsClipboard {
    changes: UnboundedReceiver<()>,
    formats: Formats,
    /// Clipboard sequence number produced by our own write, used to ignore the
    /// matching `WM_CLIPBOARDUPDATE`.
    own_sequence: u32,
}

struct Formats {
    html: u32,
    png: u32,
    exclude: u32,
    include_in_history: u32,
    upload_to_cloud: u32,
}

impl Formats {
    fn new() -> Result<Self, Error> {
        let register = |name: PCWSTR| -> Result<u32, Error> {
            let format = unsafe { RegisterClipboardFormatW(name) };
            if format == 0 {
                Err(Error::Backend(
                    "RegisterClipboardFormatW failed".to_string(),
                ))
            } else {
                Ok(format)
            }
        };
        Ok(Self {
            html: register(w!("HTML Format"))?,
            png: register(w!("PNG"))?,
            exclude: register(w!("ExcludeClipboardContentFromMonitorProcessing"))?,
            include_in_history: register(w!("CanIncludeInClipboardHistory"))?,
            upload_to_cloud: register(w!("CanUploadToClipboardHistory"))?,
        })
    }
}

impl WindowsClipboard {
    pub fn new() -> Result<Self, Error> {
        let (tx, rx) = unbounded_channel();
        let formats = Formats::new()?;
        spawn_listener(tx)?;
        Ok(Self {
            changes: rx,
            formats,
            own_sequence: 0,
        })
    }

    async fn open(&self) -> Result<OpenGuard, Error> {
        // Another app (clipboard manager, Office, remote desktop) can hold
        // the clipboard for a few milliseconds.
        for _ in 0..10 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(OpenGuard);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(Error::Backend(
            "could not open the Windows clipboard (another app is holding it)".into(),
        ))
    }

    unsafe fn clipboard_bytes(&self, format: u32) -> Option<Vec<u8>> {
        let handle = unsafe { GetClipboardData(format) }.ok()?;
        unsafe { read_global(handle) }
    }

    /// Respect the Windows clipboard-history/cloud opt-out formats.
    unsafe fn is_secret(&self) -> bool {
        if unsafe { IsClipboardFormatAvailable(self.formats.exclude) }.is_ok() {
            return true;
        }
        for format in [
            self.formats.include_in_history,
            self.formats.upload_to_cloud,
        ] {
            if let Some(bytes) = unsafe { self.clipboard_bytes(format) } {
                if bytes.len() >= 4 && u32::from_le_bytes(bytes[..4].try_into().expect("len")) == 0
                {
                    return true;
                }
            }
        }
        false
    }
}

#[async_trait(?Send)]
impl Clipboard for WindowsClipboard {
    async fn read(&mut self) -> Result<ClipboardSnapshot, Error> {
        // Copy the raw clipboard blocks while the clipboard is open, then
        // release it: decoding a multi-megabyte DIB into PNG can take tens of
        // milliseconds and holding the Win32 clipboard open blocks every other
        // application (clipboard managers, Office, remote desktop) meanwhile.
        let (text, html, png, dib) = {
            let _guard = self.open().await?;
            if unsafe { self.is_secret() } {
                log::debug!("windows clipboard is marked as secret, skipping");
                return Ok(ClipboardSnapshot::default());
            }
            let text = unsafe { self.clipboard_bytes(CF_UNICODETEXT) };
            let html = unsafe { self.clipboard_bytes(self.formats.html) };
            let mut png = unsafe { self.clipboard_bytes(self.formats.png) }
                .map(|bytes| trim_png(&bytes))
                .filter(|bytes| !bytes.is_empty());
            // Some applications expose a registered "PNG" format without
            // actually storing a PNG (it can be a container or padding); never
            // forward that as image/png. Fall back to CF_DIB in that case.
            if png
                .as_ref()
                .is_some_and(|bytes| !bytes.starts_with(&PNG_SIGNATURE))
            {
                log::debug!("windows clipboard: registered PNG format is not a PNG, ignoring");
                png = None;
            }
            let dib = if png.is_none() {
                unsafe { self.clipboard_bytes(CF_DIBV5) }
                    .or_else(|| unsafe { self.clipboard_bytes(CF_DIB) })
            } else {
                None
            };
            (text, html, png, dib)
        };

        let mut snapshot = ClipboardSnapshot::new();

        if let Some(text) = text.and_then(|bytes| utf16_text(&bytes)) {
            let text = crlf_to_lf(&text);
            if !text.is_empty() {
                snapshot.insert(Format::Text, text.into_bytes());
            }
        }

        if let Some(html) = html {
            let html = cf_html::parse(strip_nul(&html));
            if !html.is_empty() {
                snapshot.insert(Format::Html, html);
            }
        }

        let png = match png {
            Some(png) => Some(png),
            None => dib.and_then(|dib| match dib::dib_to_png(&dib) {
                Ok(png) => Some(png),
                Err(e) => {
                    log::debug!("windows clipboard: could not convert the DIB to PNG: {e}");
                    None
                }
            }),
        };
        if let Some(png) = png {
            snapshot.insert(Format::Png, png);
        }

        Ok(snapshot)
    }

    async fn write(&mut self, snapshot: &ClipboardSnapshot) -> Result<(), Error> {
        let _guard = self.open().await?;
        unsafe { EmptyClipboard() }
            .map_err(|e| Error::Backend(format!("EmptyClipboard failed: {e}")))?;

        if let Some(text) = snapshot.get(Format::Text) {
            let text = lf_to_crlf(&String::from_utf8_lossy(text));
            let mut utf16 = text.encode_utf16().collect::<Vec<_>>();
            utf16.push(0);
            let mut bytes = Vec::with_capacity(utf16.len() * 2);
            for unit in utf16 {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            unsafe { write_global(CF_UNICODETEXT, &bytes)? };
        }

        if let Some(html) = snapshot.get(Format::Html) {
            let mut blob = cf_html::build(html);
            blob.push(0);
            unsafe { write_global(self.formats.html, &blob)? };
        }

        if let Some(png) = snapshot.get(Format::Png) {
            if let Err(e) = unsafe { write_global(self.formats.png, png) } {
                log::warn!("could not publish the PNG clipboard format: {e}");
            }
            // Compatibility with apps that only understand CF_DIB.
            if let Ok(dib) = dib::png_to_dib(png) {
                if let Err(e) = unsafe { write_global(CF_DIB, &dib) } {
                    log::warn!("could not publish the CF_DIB clipboard format: {e}");
                }
            }
        }

        self.own_sequence = unsafe { GetClipboardSequenceNumber() };
        Ok(())
    }

    async fn changed(&mut self) -> Result<Option<ClipboardSnapshot>, Error> {
        loop {
            match self.changes.recv().await {
                Some(()) => {
                    let sequence = unsafe { GetClipboardSequenceNumber() };
                    if sequence == self.own_sequence {
                        continue;
                    }
                    return self.read().await.map(Some);
                }
                None => return Ok(None),
            }
        }
    }
}

struct OpenGuard;

impl Drop for OpenGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn spawn_listener(sender: UnboundedSender<()>) -> Result<(), Error> {
    if NOTIFY.lock().expect("lock").is_some() {
        return Err(Error::Backend(
            "the Windows clipboard listener is already running".into(),
        ));
    }
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("lan-mouse-clipboard".into())
        .spawn(move || {
            let result = unsafe { init_listener() };
            match result {
                Ok(hwnd) => {
                    // Publish the sender only once the message-only window is
                    // live: a failed startup must stay retryable instead of
                    // poisoning the singleton for the whole process.
                    {
                        let mut notify = NOTIFY.lock().expect("lock");
                        if notify.is_some() {
                            let _ = ready_tx.send(Err(Error::Backend(
                                "the Windows clipboard listener is already running".into(),
                            )));
                            return;
                        }
                        *notify = Some(sender);
                    }
                    let _ = ready_tx.send(Ok(()));
                    unsafe { message_loop(hwnd) };
                    // The listener thread is exiting; allow a later backend
                    // restart to install a fresh listener.
                    *NOTIFY.lock().expect("lock") = None;
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| Error::Backend(format!("could not start the clipboard thread: {e}")))?;

    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::Backend(
            "the Windows clipboard listener did not start".into(),
        )),
    }
}

unsafe fn init_listener() -> Result<HWND, Error> {
    let class_name = w!("LanMouseClipboardListener");
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|e| Error::Backend(format!("GetModuleHandleW failed: {e}")))?;
    let class = WNDCLASSW {
        hInstance: HINSTANCE(module.0),
        lpszClassName: class_name,
        lpfnWndProc: Some(wnd_proc),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        // A previous failed attempt may have registered the class already; the
        // class is per-process, so it is ours in any case.
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::Backend(format!(
                "could not register the clipboard window class: {}",
                error.0
            )));
        }
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("lan-mouse clipboard listener"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(HINSTANCE(module.0)),
            None,
        )
    }
    .map_err(|e| Error::Backend(format!("CreateWindowExW failed: {e}")))?;

    unsafe { AddClipboardFormatListener(hwnd) }
        .map_err(|e| Error::Backend(format!("AddClipboardFormatListener failed: {e}")))?;
    Ok(hwnd)
}

unsafe fn message_loop(hwnd: HWND) {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if !result.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    unsafe {
        let _ = RemoveClipboardFormatListener(hwnd);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_CLIPBOARDUPDATE {
        if let Ok(notify) = NOTIFY.lock() {
            if let Some(sender) = notify.as_ref() {
                let _ = sender.send(());
            }
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

unsafe fn read_global(handle: HANDLE) -> Option<Vec<u8>> {
    if handle.is_invalid() {
        return None;
    }
    let hglobal = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(hglobal) };
    if size == 0 || size > MAX_GLOBAL_BYTES {
        return None;
    }
    let ptr = unsafe { GlobalLock(hglobal) } as *const u8;
    if ptr.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
    let _ = unsafe { GlobalUnlock(hglobal) };
    Some(bytes)
}

unsafe fn write_global(format: u32, bytes: &[u8]) -> Result<(), Error> {
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
        .map_err(|e| Error::Backend(format!("GlobalAlloc failed: {e}")))?;
    let ptr = unsafe { GlobalLock(hglobal) } as *mut u8;
    if ptr.is_null() {
        let _ = unsafe { GlobalFree(Some(hglobal)) };
        return Err(Error::Backend("GlobalLock failed".into()));
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
    let _ = unsafe { GlobalUnlock(hglobal) };
    match unsafe { SetClipboardData(format, Some(HANDLE(hglobal.0))) } {
        Ok(_) => Ok(()),
        Err(e) => {
            let _ = unsafe { GlobalFree(Some(hglobal)) };
            Err(Error::Backend(format!("SetClipboardData failed: {e}")))
        }
    }
}

fn strip_nul(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|byte| *byte == 0) {
        Some(end) => &bytes[..end],
        None => bytes,
    }
}

fn utf16_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 {
        return None;
    }
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| u16::from_le_bytes(*chunk))
        .take_while(|unit| *unit != 0)
        .collect::<Vec<_>>();
    Some(String::from_utf16_lossy(&units))
}

/// Cut a `PNG` clipboard blob at the `IEND` chunk. `GlobalSize` reports the
/// allocated block size, which can include padding.
fn trim_png(bytes: &[u8]) -> Vec<u8> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if !bytes.starts_with(&SIGNATURE) {
        return bytes.to_vec();
    }
    let mut position = SIGNATURE.len();
    while position + 12 <= bytes.len() {
        let Ok(length_bytes) = bytes[position..position + 4].try_into() else {
            break;
        };
        let length = u32::from_be_bytes(length_bytes) as usize;
        let Some(end) = position.checked_add(12 + length) else {
            break;
        };
        if end > bytes.len() {
            break;
        }
        let kind = &bytes[position + 4..position + 8];
        position = end;
        if kind == b"IEND" {
            return bytes[..position].to_vec();
        }
    }
    bytes.to_vec()
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    #[test]
    fn utf16_text_stops_at_the_nul_terminator() {
        let mut bytes = Vec::new();
        for unit in "olá".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0, 0x41, 0x00]);
        assert_eq!(utf16_text(&bytes).as_deref(), Some("olá"));
    }

    #[test]
    fn strip_nul_keeps_the_payload() {
        assert_eq!(strip_nul(b"abc\0def"), b"abc");
        assert_eq!(strip_nul(b"abc"), b"abc");
    }

    #[test]
    fn trim_png_cuts_trailing_padding() {
        let png = crate::dib::rgba_to_png(1, 1, &[1, 2, 3, 4]).expect("png");
        let mut padded = png.clone();
        padded.extend_from_slice(&[0; 9]);
        assert_eq!(trim_png(&padded), png);
    }
}
