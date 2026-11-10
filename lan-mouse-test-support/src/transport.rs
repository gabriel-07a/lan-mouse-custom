//! Deterministic in-memory transport for integration/component tests.
//!
//! Two endpoints exchange opaque byte datagrams through channels. Loss and
//! duplication are configured with fixed periods (not random), so a test that
//! covers a retransmission path is reproducible on every run. No sockets, no
//! sleeps and no background thread are involved.
//!
//! Reordering is not simulated yet; add it only together with a consumer that
//! actually needs it (out-of-order deltas are commutative, ordered control
//! events are the interesting case).

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Loss/duplication pattern for one direction of the link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkConfig {
    /// Drop the `drop_every`-th datagram (`0` disables).
    pub drop_every: u32,
    /// Send the `duplicate_every`-th datagram twice (`0` disables).
    pub duplicate_every: u32,
}

impl LinkConfig {
    pub fn lossy(drop_every: u32) -> Self {
        Self {
            drop_every,
            ..Self::default()
        }
    }

    pub fn duplicating(duplicate_every: u32) -> Self {
        Self {
            duplicate_every,
            ..Self::default()
        }
    }
}

/// One endpoint of a [`MemoryLink`].
pub struct LinkSide {
    tx: UnboundedSender<Vec<u8>>,
    rx: UnboundedReceiver<Vec<u8>>,
    sent: Rc<Cell<u32>>,
    config: LinkConfig,
}

impl LinkSide {
    /// Send one datagram to the peer.
    ///
    /// Returns how many copies were actually queued (0 when the configured
    /// loss pattern drops this datagram, 2 when it is duplicated).
    pub fn send(&self, bytes: impl Into<Vec<u8>>) -> u32 {
        let bytes = bytes.into();
        let sequence = self.sent.get().saturating_add(1);
        self.sent.set(sequence);

        if self.config.drop_every != 0 && sequence.is_multiple_of(self.config.drop_every) {
            return 0;
        }
        let copies = if self.config.duplicate_every != 0
            && sequence.is_multiple_of(self.config.duplicate_every)
        {
            2
        } else {
            1
        };
        for _ in 0..copies {
            let _ = self.tx.send(bytes.clone());
        }
        copies
    }

    /// Receive the next datagram (async).
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }

    /// Receive the next datagram if one is already queued (deterministic tests).
    pub fn try_recv(&mut self) -> Option<Vec<u8>> {
        self.rx.try_recv().ok()
    }

    /// Number of datagrams handed to `send` so far (including dropped ones).
    pub fn sequence(&self) -> u32 {
        self.sent.get()
    }
}

/// A pair of connected endpoints with independent per-direction patterns.
pub fn memory_link(a_to_b: LinkConfig, b_to_a: LinkConfig) -> (LinkSide, LinkSide) {
    let (a_tx, b_rx) = unbounded_channel();
    let (b_tx, a_rx) = unbounded_channel();
    let a = LinkSide {
        tx: a_tx,
        rx: a_rx,
        sent: Rc::new(Cell::new(0)),
        config: a_to_b,
    };
    let b = LinkSide {
        tx: b_tx,
        rx: b_rx,
        sent: Rc::new(Cell::new(0)),
        config: b_to_a,
    };
    (a, b)
}

/// Recording sink for frontend events.
///
/// Tests that drive the Service/frontends can inspect the exact event sequence
/// without a real GPUI/GTK window.
#[derive(Default)]
pub struct RecordingFrontend {
    events: Rc<RefCell<Vec<lan_mouse_ipc::FrontendEvent>>>,
}

impl RecordingFrontend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, event: lan_mouse_ipc::FrontendEvent) {
        self.events.borrow_mut().push(event);
    }

    pub fn take(&self) -> Vec<lan_mouse_ipc::FrontendEvent> {
        std::mem::take(&mut self.events.borrow_mut())
    }

    pub fn len(&self) -> usize {
        self.events.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Small helper for deterministic "eventually" checks without `sleep`: drain a
/// queue until the predicate matches or the queue is empty.
pub fn drain_until<T>(queue: &mut VecDeque<T>, mut predicate: impl FnMut(&T) -> bool) -> Option<T> {
    while let Some(item) = queue.pop_front() {
        if predicate(&item) {
            return Some(item);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagrams_reach_the_opposite_endpoint_only() {
        let (mut a, mut b) = memory_link(LinkConfig::default(), LinkConfig::default());
        assert_eq!(a.send(b"hello".to_vec()), 1);
        assert_eq!(b.try_recv(), Some(b"hello".to_vec()));
        assert_eq!(b.send(b"reply".to_vec()), 1);
        assert_eq!(a.try_recv(), Some(b"reply".to_vec()));
        assert_eq!(a.try_recv(), None);
    }

    #[test]
    fn loss_pattern_is_periodic() {
        let (a, mut b) = memory_link(LinkConfig::lossy(2), LinkConfig::default());
        assert_eq!(a.send(b"one".to_vec()), 1);
        assert_eq!(a.send(b"two".to_vec()), 0);
        assert_eq!(a.send(b"three".to_vec()), 1);
        assert_eq!(b.try_recv(), Some(b"one".to_vec()));
        assert_eq!(b.try_recv(), Some(b"three".to_vec()));
        assert_eq!(a.sequence(), 3);
    }

    #[test]
    fn duplication_pattern_is_periodic() {
        let (a, mut b) = memory_link(LinkConfig::duplicating(2), LinkConfig::default());
        assert_eq!(a.send(b"one".to_vec()), 1);
        assert_eq!(a.send(b"two".to_vec()), 2);
        assert_eq!(b.try_recv(), Some(b"one".to_vec()));
        assert_eq!(b.try_recv(), Some(b"two".to_vec()));
        assert_eq!(b.try_recv(), Some(b"two".to_vec()));
        assert_eq!(b.try_recv(), None);
    }

    #[test]
    fn recording_frontend_keeps_the_event_order() {
        let sink = RecordingFrontend::new();
        sink.push(lan_mouse_ipc::FrontendEvent::Error("a".to_string()));
        sink.push(lan_mouse_ipc::FrontendEvent::Error("b".to_string()));
        assert_eq!(sink.len(), 2);
        let events = sink.take();
        assert_eq!(events.len(), 2);
        assert!(sink.is_empty());
    }

    #[test]
    fn drain_until_consumes_only_until_the_match() {
        let mut queue = VecDeque::from([1, 2, 3]);
        assert_eq!(drain_until(&mut queue, |item| *item == 2), Some(2));
        assert_eq!(queue, VecDeque::from([3]));
    }
}
