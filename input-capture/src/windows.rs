use async_trait::async_trait;
use core::task::{Context, Poll};
use event_thread::EventThread;
use futures::Stream;
use std::pin::Pin;

use std::task::ready;
use tokio::sync::mpsc::{Receiver, channel};

use super::{Capture, CaptureError, CaptureEvent, Position};

mod display_util;
mod event_thread;

/// Mouse buttons physically down right now, as Linux `BTN_*` codes.
///
/// Asked when a capture begins: the hook has not swallowed anything of the
/// drag yet, so the asynchronous key state still has the press.
fn held_mouse_buttons() -> Vec<u32> {
    use input_event::{BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VIRTUAL_KEY, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
    };
    // the same mapping as the hook's button events (XBUTTON1 = back)
    const BUTTONS: [(VIRTUAL_KEY, u32); 5] = [
        (VK_LBUTTON, BTN_LEFT),
        (VK_RBUTTON, BTN_RIGHT),
        (VK_MBUTTON, BTN_MIDDLE),
        (VK_XBUTTON1, BTN_BACK),
        (VK_XBUTTON2, BTN_FORWARD),
    ];
    BUTTONS
        .iter()
        .filter(|(key, _)| {
            // the most significant bit: the button is down now
            (unsafe { GetAsyncKeyState(i32::from(key.0)) } as u16) & 0x8000 != 0
        })
        .map(|&(_, button)| button)
        .collect()
}

pub struct WindowsInputCapture {
    event_rx: Receiver<(Position, CaptureEvent)>,
    event_thread: EventThread,
}

#[async_trait]
impl Capture for WindowsInputCapture {
    async fn held_buttons(&mut self) -> Option<Vec<u32>> {
        Some(held_mouse_buttons())
    }

    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.event_thread.create(pos);
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.event_thread.destroy(pos);
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.event_thread.release_capture();
        Ok(())
    }

    async fn release_at(&mut self, pos: Position, along: f32) -> Result<(), CaptureError> {
        self.event_thread.release_capture_at(pos, along);
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    fn set_remote_peer(&mut self, peer: Option<Position>) {
        self.event_thread.set_remote_peer(peer);
    }

    fn begin(&mut self, pos: Position) -> bool {
        self.event_thread.begin_capture(pos);
        true
    }
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Result<Self, super::CaptureCreationError> {
        // 1000 Hz mice deliver events faster than the capture thread can
        // forward them; a 10 slot buffer dropped movements.
        let (event_tx, event_rx) = channel(512);
        let event_thread = EventThread::new(event_tx)?;
        Ok(Self {
            event_thread,
            event_rx,
        })
    }
}

impl Stream for WindowsInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}
