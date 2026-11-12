//! Independent, deterministic adversarial QA harness for clipboard copy
//! propagation ("Ctrl+C on one machine must be pasteable on the other").
//!
//! The harness only uses the public `input-clipboard` / `lan-mouse-proto` APIs
//! (no private service internals). Every frame is encoded with the production
//! [`lan_mouse_proto::clipboard::ClipboardFrame`] codec and decoded again on
//! delivery, so wire bugs are exercised too. A [`Node`] mirrors the service
//! glue from `src/clipboard.rs`:
//!
//! - `changed()` stream -> `ClipboardSync::on_local_change` ([`Node::copy`],
//!   [`Node::pump_local_changes`]);
//! - peer accepted by the service -> route + `add_peer` + read the current
//!   clipboard and offer it ([`Node::connect`] / [`Node::register`]);
//! - `ClipboardSync::tick` -> write to the backend + `on_applied`
//!   ([`Node::tick`]);
//! - disable/enable and backend restart semantics ([`Node::disable`],
//!   [`Node::enable`], [`Node::restart_backend`]).
//!
//! ## Determinism
//!
//! No sleeps, sockets, threads or real clock: time is a simulated `Instant`
//! that advances [`STEP`] per round, every wait is bounded, and loss /
//! duplication are periodic or driven by a seeded xorshift generator. A
//! failure is therefore reproducible from the test name alone.
//!
//! ## What is regression-sensitive
//!
//! The reported symptom is that the *first* copy after a connection/handoff is
//! sometimes lost and a *second* Ctrl+C fixes it. The tests tagged with
//! "reported symptom" below are acceptance tests for that fix: they are
//! expected to be red until the atomicity fix lands, and must stay green after.
//! Robustness checks beyond the stated transport contract (DTLS is
//! ordered-but-lossy) are now part of the normal suite; the atomicity fix made
//! both the reconnect and long-outage cases green.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    rc::Rc,
    time::{Duration, Instant},
};

use input_clipboard::sync::{ClipboardSync, FrameSink, PeerId};
use input_clipboard::{ClipboardSnapshot, Format, Limits};
use lan_mouse_proto::clipboard::{
    CHUNK_SIZE, ClipboardFrame, MAX_CLIPBOARD_FRAME, MIME_HTML, MIME_PNG, MIME_TEXT,
};

/// Simulated network/service round (the production clipboard tasker runs at
/// 15 ms; tests only need the round to be small compared to `ACK_TIMEOUT`).
const STEP: Duration = Duration::from_millis(10);

const NODE_A: PeerId = 1;
const NODE_B: PeerId = 2;
const NODE_C: PeerId = 3;

// ---------------------------------------------------------------------------
// In-memory wire
// ---------------------------------------------------------------------------

/// Deterministic link behaviour for one direction.
#[derive(Clone, Copy, Debug)]
struct Link {
    /// Drop the first `drop_first` datagrams of the direction.
    drop_first: u32,
    /// Drop every `n`-th datagram (0 disables).
    drop_every: u32,
    /// Drop this percentage of datagrams via the seeded generator.
    loss_percent: u8,
    /// Seed for the loss generator (must be non-zero to be usable).
    seed: u64,
    /// Queue every `n`-th datagram twice (0 disables).
    duplicate_every: u32,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            drop_first: 0,
            drop_every: 0,
            loss_percent: 0,
            seed: 0x9e37_79b9_7f4a_7c15,
            duplicate_every: 0,
        }
    }
}

impl Link {
    fn lossless() -> Self {
        Self::default()
    }

    fn lossy(percent: u8, seed: u64) -> Self {
        Self {
            loss_percent: percent,
            seed,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct LinkState {
    link: Link,
    sequence: u32,
    rng: u64,
}

impl LinkState {
    fn new(link: Link) -> Self {
        Self {
            link,
            sequence: 0,
            rng: link.seed,
        }
    }
}

/// One datagram on the wire, exactly as the production codec encoded it.
#[derive(Clone, Debug)]
struct Datagram {
    from: PeerId,
    to: PeerId,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
struct WireStats {
    sent: u64,
    dropped: u64,
    queued: u64,
    duplicated: u64,
}

#[derive(Debug, Default)]
struct WireState {
    queue: VecDeque<Datagram>,
    links: HashMap<(PeerId, PeerId), LinkState>,
    stats: WireStats,
}

/// In-memory, lossy/in-order datagram network with per-direction link
/// behaviour. `transport_up` models `ConnectionSender::send_raw` failing while
/// the DTLS association is (re)connecting: the frame is dropped by the sink and
/// never reaches the peer.
#[derive(Debug, Default)]
struct Wire {
    state: RefCell<WireState>,
    transport_up: RefCell<HashMap<(PeerId, PeerId), bool>>,
}

fn next_rng(state: u64) -> u64 {
    let mut value = if state == 0 {
        0x9e37_79b9_7f4a_7c15
    } else {
        state
    };
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    value
}

impl Wire {
    /// Queue a frame produced by a sink, applying the configured loss /
    /// duplication for the direction.
    fn send(&self, from: PeerId, to: PeerId, bytes: Vec<u8>) {
        let direction = (from, to);
        let (sequence, link, rng) = {
            let mut state = self.state.borrow_mut();
            state.stats.sent += 1;
            let entry = state
                .links
                .entry(direction)
                .or_insert_with(|| LinkState::new(Link::lossless()));
            entry.sequence = entry.sequence.wrapping_add(1);
            if entry.link.loss_percent > 0 {
                entry.rng = next_rng(entry.rng);
            }
            (entry.sequence, entry.link, entry.rng)
        };

        let transport_up = self
            .transport_up
            .borrow()
            .get(&direction)
            .copied()
            .unwrap_or(true);
        let mut dropped = !transport_up;
        if !dropped && link.drop_first > 0 && sequence <= link.drop_first {
            dropped = true;
        }
        if !dropped && link.drop_every > 0 && sequence % link.drop_every == 0 {
            dropped = true;
        }
        if !dropped && link.loss_percent > 0 && (rng % 100) < u64::from(link.loss_percent) {
            dropped = true;
        }

        let mut state = self.state.borrow_mut();
        if dropped {
            state.stats.dropped += 1;
            return;
        }
        let copies = if link.duplicate_every > 0 && sequence % link.duplicate_every == 0 {
            2
        } else {
            1
        };
        for _ in 0..copies {
            state.queue.push_back(Datagram {
                from,
                to,
                bytes: bytes.clone(),
            });
            state.stats.queued += 1;
        }
        if copies == 2 {
            state.stats.duplicated += 1;
        }
    }

    /// Queue raw bytes without applying the modelled link (used to replay
    /// captured datagrams or inject hostile/corrupted input).
    fn queue_raw(&self, from: PeerId, to: PeerId, bytes: Vec<u8>) {
        let mut state = self.state.borrow_mut();
        state.queue.push_back(Datagram { from, to, bytes });
        state.stats.queued += 1;
    }

    fn take_all(&self) -> Vec<Datagram> {
        self.state.borrow_mut().queue.drain(..).collect()
    }

    fn set_link(&self, from: PeerId, to: PeerId, link: Link) {
        self.state
            .borrow_mut()
            .links
            .insert((from, to), LinkState::new(link));
    }

    fn set_transport(&self, from: PeerId, to: PeerId, up: bool) {
        self.transport_up.borrow_mut().insert((from, to), up);
    }

    fn queued(&self) -> usize {
        self.state.borrow().queue.len()
    }

    fn stats(&self) -> WireStats {
        self.state.borrow().stats
    }

    fn clear(&self) {
        self.state.borrow_mut().queue.clear();
    }
}

#[derive(Clone)]
struct WireSink {
    from: PeerId,
    wire: Rc<Wire>,
}

impl FrameSink for WireSink {
    fn send(&self, peer: PeerId, frame: ClipboardFrame) {
        self.wire.send(self.from, peer, frame.encode());
    }
}

// ---------------------------------------------------------------------------
// Test node: clipboard backend + sync state machine + service glue
// ---------------------------------------------------------------------------

/// Fake OS clipboard. `copy_local` is a user copy (reported through `changed()`);
/// `apply_remote` is what the service does after `write()` and deliberately does
/// not wake the local change stream (echo suppression contract).
#[derive(Debug, Default)]
struct FakeClipboard {
    current: ClipboardSnapshot,
    applied: Vec<ClipboardSnapshot>,
    local_changes: VecDeque<ClipboardSnapshot>,
}

impl FakeClipboard {
    fn copy_local(&mut self, snapshot: ClipboardSnapshot) {
        self.current = snapshot.clone();
        self.local_changes.push_back(snapshot);
    }

    fn take_local_change(&mut self) -> Option<ClipboardSnapshot> {
        self.local_changes.pop_front()
    }

    fn clear_local_changes(&mut self) {
        self.local_changes.clear();
    }

    fn apply_remote(&mut self, snapshot: ClipboardSnapshot) {
        self.current = snapshot.clone();
        self.applied.push(snapshot);
    }
}

/// One clipboard-sync endpoint. Mirrors the public surface of
/// `src/clipboard.rs` (the private service task cannot be imported).
struct Node {
    id: PeerId,
    origin: u64,
    limits: Rc<Cell<Limits>>,
    sink: WireSink,
    sync: ClipboardSync<WireSink>,
    clipboard: FakeClipboard,
    enabled: bool,
    /// Peers the service considers connectable (active client that advertises
    /// the clipboard capability).
    configured: HashSet<PeerId>,
    /// Peers currently registered with the sync layer (production
    /// `known_outgoing`).
    known: HashSet<PeerId>,
}

impl Node {
    fn new(id: PeerId, origin: u64, wire: Rc<Wire>) -> Self {
        let limits = Rc::new(Cell::new(Limits::default()));
        let sink = WireSink { from: id, wire };
        Self {
            id,
            origin,
            limits: limits.clone(),
            sink: sink.clone(),
            sync: ClipboardSync::new(sink, limits, origin),
            clipboard: FakeClipboard::default(),
            enabled: true,
            configured: HashSet::new(),
            known: HashSet::new(),
        }
    }

    /// Simulate the user pressing Ctrl+C.
    fn copy(&mut self, snapshot: ClipboardSnapshot, now: Instant) {
        self.clipboard.copy_local(snapshot);
        self.pump_local_changes(now);
    }

    /// Drain the backend `changed()` stream into the sync layer. A change that
    /// arrives while sync is disabled is consumed and dropped, like the service
    /// does.
    fn pump_local_changes(&mut self, now: Instant) -> usize {
        let mut forwarded = 0;
        while let Some(snapshot) = self.clipboard.take_local_change() {
            if self.enabled {
                self.sync.on_local_change(snapshot, now);
                forwarded += 1;
            }
        }
        forwarded
    }

    /// Register a peer: add the route, then offer the current clipboard (the
    /// service reads the backend on every fresh clipboard-capable connection).
    fn register(&mut self, peer: PeerId, now: Instant) {
        if !self.known.insert(peer) {
            return;
        }
        self.sync.add_peer(peer);
        let current = self.clipboard.current.clone();
        self.sync.on_local_change(current, now);
    }

    fn connect(&mut self, peer: PeerId, now: Instant) {
        self.configured.insert(peer);
        self.register(peer, now);
    }

    fn disconnect(&mut self, peer: PeerId) {
        self.configured.remove(&peer);
        if self.known.remove(&peer) {
            self.sync.remove_peer(peer);
        }
    }

    fn disable(&mut self) {
        self.enabled = false;
        for peer in std::mem::take(&mut self.known) {
            self.sync.remove_peer(peer);
        }
        self.sync.reset();
        self.sync.clear_error();
        self.clipboard.clear_local_changes();
    }

    /// Re-enable sync: the service re-registers active clients and reads the
    /// current clipboard, so content copied while disabled is offered.
    fn enable(&mut self, now: Instant) {
        self.enabled = true;
        for peer in self.configured.clone() {
            self.register(peer, now);
        }
    }

    /// Simulate the clipboard backend dying and being recreated: the sync state
    /// machine starts fresh and re-registers the connected peers.
    fn restart_backend(&mut self, now: Instant) {
        self.sync = ClipboardSync::new(self.sink.clone(), self.limits.clone(), self.origin);
        self.known.clear();
        for peer in self.configured.clone() {
            self.register(peer, now);
        }
    }

    fn set_limits(&mut self, limits: Limits) {
        self.limits.set(limits);
    }

    fn tick(&mut self, now: Instant) {
        if !self.enabled {
            self.clipboard.clear_local_changes();
            return;
        }
        self.pump_local_changes(now);
        if let Some(snapshot) = self.sync.tick(now) {
            self.clipboard.apply_remote(snapshot.clone());
            self.sync.on_applied(&snapshot);
        }
    }
}

// ---------------------------------------------------------------------------
// Test network: deterministic simulation of the service loop
// ---------------------------------------------------------------------------

struct Net {
    wire: Rc<Wire>,
    nodes: BTreeMap<PeerId, Node>,
    now: Instant,
    started: Instant,
    rounds: usize,
    decode_errors: usize,
}

impl Net {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            wire: Rc::new(Wire::default()),
            nodes: BTreeMap::new(),
            now,
            started: now,
            rounds: 0,
            decode_errors: 0,
        }
    }

    fn add_node(&mut self, id: PeerId, origin: u64) {
        assert!(
            self.nodes
                .insert(id, Node::new(id, origin, self.wire.clone()))
                .is_none(),
            "duplicate node id {id}"
        );
    }

    fn node(&self, id: PeerId) -> &Node {
        self.nodes
            .get(&id)
            .unwrap_or_else(|| panic!("unknown node {id}"))
    }

    fn node_mut(&mut self, id: PeerId) -> &mut Node {
        self.nodes
            .get_mut(&id)
            .unwrap_or_else(|| panic!("unknown node {id}"))
    }

    fn current(&self, id: PeerId) -> ClipboardSnapshot {
        self.node(id).clipboard.current.clone()
    }

    fn applied(&self, id: PeerId) -> &[ClipboardSnapshot] {
        &self.node(id).clipboard.applied
    }

    fn applied_count(&self, id: PeerId) -> usize {
        self.node(id).clipboard.applied.len()
    }

    fn copy(&mut self, id: PeerId, snapshot: ClipboardSnapshot) {
        let now = self.now;
        self.node_mut(id).copy(snapshot, now);
    }

    fn connect(&mut self, a: PeerId, b: PeerId) {
        let now = self.now;
        self.node_mut(a).connect(b, now);
        self.node_mut(b).connect(a, now);
    }

    fn disconnect(&mut self, a: PeerId, b: PeerId) {
        self.node_mut(a).disconnect(b);
        self.node_mut(b).disconnect(a);
    }

    fn disable(&mut self, id: PeerId) {
        self.node_mut(id).disable();
    }

    fn enable(&mut self, id: PeerId) {
        let now = self.now;
        self.node_mut(id).enable(now);
    }

    fn restart_backend(&mut self, id: PeerId) {
        let now = self.now;
        self.node_mut(id).restart_backend(now);
    }

    fn set_limits(&mut self, id: PeerId, limits: Limits) {
        self.node_mut(id).set_limits(limits);
    }

    fn set_link(&mut self, from: PeerId, to: PeerId, link: Link) {
        self.wire.set_link(from, to, link);
    }

    fn set_transport(&mut self, from: PeerId, to: PeerId, up: bool) {
        self.wire.set_transport(from, to, up);
    }

    fn take_datagrams(&mut self) -> Vec<Datagram> {
        self.wire.take_all()
    }

    fn inject_raw(&mut self, from: PeerId, to: PeerId, bytes: Vec<u8>) {
        self.wire.queue_raw(from, to, bytes);
    }

    /// Deliver exactly the given datagrams (a captured/reordered subset),
    /// decoding each one exactly like the service receive loop does.
    fn deliver_datagrams(&mut self, datagrams: Vec<Datagram>) -> usize {
        let now = self.now;
        let mut delivered = 0;
        for datagram in datagrams {
            let Some(node) = self.nodes.get_mut(&datagram.to) else {
                continue;
            };
            if !node.enabled {
                // The service drops clipboard frames while sync is disabled.
                continue;
            }
            match ClipboardFrame::decode(&datagram.bytes) {
                Ok(frame) => {
                    node.sync.handle_frame(datagram.from, frame, now);
                    delivered += 1;
                }
                Err(_) => self.decode_errors += 1,
            }
        }
        delivered
    }

    fn deliver(&mut self) -> usize {
        let datagrams = self.take_datagrams();
        self.deliver_datagrams(datagrams)
    }

    fn tick(&mut self) {
        let now = self.now;
        for node in self.nodes.values_mut() {
            node.tick(now);
        }
    }

    fn advance(&mut self) {
        self.now += STEP;
        self.rounds += 1;
    }

    fn step(&mut self) -> usize {
        let delivered = self.deliver();
        self.tick();
        self.advance();
        delivered
    }

    fn try_run_until(&mut self, max_steps: usize, mut predicate: impl FnMut(&Net) -> bool) -> bool {
        for _ in 0..max_steps {
            self.step();
            if predicate(self) {
                return true;
            }
        }
        false
    }

    fn run_until(&mut self, max_steps: usize, mut predicate: impl FnMut(&Net) -> bool) {
        for _ in 0..max_steps {
            self.step();
            if predicate(self) {
                return;
            }
        }
        panic!(
            "condition not reached after {max_steps} simulated rounds:\n{}",
            self.diagnostics()
        );
    }

    fn run_until_applied(&mut self, node: PeerId, expected: usize, max_steps: usize) {
        self.run_until(max_steps, |net| net.applied(node).len() >= expected);
    }

    fn settle(&mut self, rounds: usize) {
        for _ in 0..rounds {
            self.step();
        }
    }

    /// Assert that the system is quiescent: no frames are pending and no
    /// clipboard is still being updated (no echo loop, no stale retransmit).
    fn assert_quiet(&mut self, label: &str) {
        self.settle(20);
        assert_eq!(
            self.wire.queued(),
            0,
            "{label}: frames are still in flight while the clipboard is idle:\n{}",
            self.diagnostics()
        );
        let before: Vec<usize> = self
            .nodes
            .values()
            .map(|node| node.clipboard.applied.len())
            .collect();
        self.settle(10);
        let after: Vec<usize> = self
            .nodes
            .values()
            .map(|node| node.clipboard.applied.len())
            .collect();
        assert_eq!(
            before,
            after,
            "{label}: the clipboard kept changing while idle (echo loop?):\n{}",
            self.diagnostics()
        );
        assert_eq!(
            self.wire.queued(),
            0,
            "{label}: frames are still in flight while the clipboard is idle:\n{}",
            self.diagnostics()
        );
    }

    fn decode_errors(&self) -> usize {
        self.decode_errors
    }

    fn diagnostics(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "rounds={} simulated={:?} decode_errors={} wire={:?}\n",
            self.rounds,
            self.now.duration_since(self.started),
            self.decode_errors,
            self.wire.stats()
        ));
        for node in self.nodes.values() {
            let formats: Vec<Format> = node.clipboard.current.formats().collect();
            let mut peers: Vec<PeerId> = node.sync.peers().collect();
            peers.sort_unstable();
            out.push_str(&format!(
                "  node {} (origin {:#x}): enabled={} current={} bytes {:?} applied={} last_error={:?} sync_peers={:?} configured={:?}\n",
                node.id,
                node.origin,
                node.enabled,
                node.clipboard.current.total_len(),
                formats,
                node.clipboard.applied.len(),
                node.sync.last_error(),
                peers,
                {
                    let mut configured: Vec<PeerId> = node.configured.iter().copied().collect();
                    configured.sort_unstable();
                    configured
                }
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn text(value: &str) -> ClipboardSnapshot {
    ClipboardSnapshot::from_items([(Format::Text, value.as_bytes().to_vec())])
}

fn bytes_copy(bytes: Vec<u8>) -> ClipboardSnapshot {
    ClipboardSnapshot::from_items([(Format::Text, bytes)])
}

fn multi_format() -> ClipboardSnapshot {
    ClipboardSnapshot::from_items([
        (Format::Text, b"rich copy".to_vec()),
        (Format::Html, vec![b'<'; 64 * 1024]),
        (Format::Png, vec![0x89; 256 * 1024]),
    ])
}

fn pair() -> Net {
    let mut net = Net::new();
    net.add_node(NODE_A, 0xAAAA_0000_0000_0001);
    net.add_node(NODE_B, 0xBBBB_0000_0000_0002);
    net.connect(NODE_A, NODE_B);
    net
}

fn applied(net: &Net, node: PeerId) -> Vec<ClipboardSnapshot> {
    net.applied(node).to_vec()
}

/// Every applied snapshot must be one of the copied snapshots and the applied
/// indices must strictly increase: a peer may coalesce copies, but it must
/// never apply stale/older content after newer content.
fn assert_monotonic_subsequence(
    label: &str,
    applied: &[ClipboardSnapshot],
    copies: &[ClipboardSnapshot],
) {
    let mut previous: Option<usize> = None;
    for snapshot in applied {
        let index = copies
            .iter()
            .position(|copy| copy == snapshot)
            .unwrap_or_else(|| {
                let preview = snapshot
                    .get(Format::Text)
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                    .unwrap_or_else(|| "<non-text>".to_string());
                panic!("{label}: peer applied content that was never copied: {preview:?}")
            });
        if let Some(previous) = previous {
            assert!(
                index > previous,
                "{label}: peer applied copy #{index} after copy #{previous} (stale clipboard)"
            );
        }
        previous = Some(index);
    }
}

// ---------------------------------------------------------------------------
// Tests: basic propagation
// ---------------------------------------------------------------------------

/// A single text copy must arrive byte-exact (UTF-8, newlines, tabs).
#[test]
fn text_copy_reaches_peer_exactly() {
    let mut net = pair();
    let copy = text("ação – Ctrl+C ✓\nline 2\ttab");
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 500);
    assert_eq!(net.current(NODE_B), copy);
    assert_eq!(applied(&net, NODE_B), vec![copy.clone()]);
    assert_eq!(net.applied_count(NODE_A), 0);
    assert_eq!(net.node(NODE_A).sync.last_error(), None);
    net.assert_quiet("single text copy");
}

/// Multi-format snapshots (text + html + png) must transfer without losing or
/// mixing representations.
#[test]
fn multi_format_snapshot_transfers_byte_exact() {
    let mut net = pair();
    let copy = multi_format();
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 8_000);
    assert_eq!(net.current(NODE_B), copy);
    assert_eq!(net.applied_count(NODE_B), 1);
    net.assert_quiet("multi-format copy");
}

/// The largest text representation the default configuration accepts
/// (256 KiB) must transfer byte-exact.
#[test]
fn large_text_at_the_default_limit_transfers_exactly() {
    let mut net = pair();
    let limits = Limits::default();
    let payload: Vec<u8> = (0..limits.max_text)
        .map(|index| (index % 251) as u8)
        .collect();
    let copy = bytes_copy(payload);
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 5_000);
    assert_eq!(net.current(NODE_B), copy);
    assert_eq!(net.applied_count(NODE_B), 1);
}

/// A single copy must reach every connected peer, and a dead peer must not
/// block a healthy one or poison the next copy.
#[test]
fn one_copy_reaches_every_peer_and_a_dead_peer_does_not_block_the_others() {
    let mut net = Net::new();
    net.add_node(NODE_A, 0xAAAA_0000_0000_0001);
    net.add_node(NODE_B, 0xBBBB_0000_0000_0002);
    net.add_node(NODE_C, 0xCCCC_0000_0000_0003);
    net.connect(NODE_A, NODE_B);
    net.connect(NODE_A, NODE_C);

    // Peer C is unreachable (DTLS send keeps failing): only B must get the copy.
    net.set_transport(NODE_A, NODE_C, false);
    let first = text("first copy while C is dead");
    net.copy(NODE_A, first.clone());
    net.run_until_applied(NODE_B, 1, 2_000);
    assert_eq!(net.current(NODE_B), first);
    net.settle(200); // let the transfer to C exhaust its retries (~1.2 s)
    assert!(
        net.node(NODE_A).sync.last_error().is_some(),
        "the failed transfer to C should be reported"
    );

    // C comes back: the next copy must reach both peers.
    net.set_transport(NODE_A, NODE_C, true);
    let second = text("second copy, C is back");
    net.copy(NODE_A, second.clone());
    net.run_until(2_000, |net| {
        net.applied_count(NODE_B) == 2 && net.applied_count(NODE_C) == 1
    });
    assert_eq!(net.current(NODE_B), second);
    assert_eq!(net.current(NODE_C), second);
}

// ---------------------------------------------------------------------------
// Tests: rapid consecutive copies
// ---------------------------------------------------------------------------

/// 64 copies delivered one after another: every copy must be applied in copy
/// order on a clean link.
#[test]
fn repeated_copies_are_applied_in_order_on_a_clean_link() {
    let copies: Vec<ClipboardSnapshot> = (0..64)
        .map(|index| text(&format!("copy {index}")))
        .collect();
    let mut net = pair();
    for (index, copy) in copies.iter().enumerate() {
        net.copy(NODE_A, copy.clone());
        net.run_until_applied(NODE_B, index + 1, 500);
    }
    assert_eq!(applied(&net, NODE_B), copies);
    assert_eq!(net.node(NODE_A).sync.last_error(), None);
    net.assert_quiet("64 sequential copies");
}

/// 80 copies in quick succession over a link that drops 20% of datagrams in
/// both directions. The last copy must always win, no older copy may ever be
/// applied after a newer one, and no retry may leave a stale frame behind.
#[test]
fn rapid_copies_never_lose_the_last_copy_under_loss() {
    let copies: Vec<ClipboardSnapshot> = (0..80)
        .map(|index| text(&format!("rapid copy {index}")))
        .collect();
    let mut net = pair();
    net.set_link(NODE_A, NODE_B, Link::lossy(20, 0x5eed));
    net.set_link(NODE_B, NODE_A, Link::lossy(20, 0xf00d));
    for copy in &copies {
        net.copy(NODE_A, copy.clone());
        net.step();
    }
    net.run_until(5_000, |net| net.applied(NODE_B).last() == copies.last());
    assert_eq!(
        net.current(NODE_B),
        *copies.last().expect("at least one copy"),
        "the last copy must win after rapid updates"
    );
    assert_monotonic_subsequence("rapid copies under loss", net.applied(NODE_B), &copies);
    assert_eq!(net.applied(NODE_B).last(), copies.last());
}

/// Duplicate owner-change events for the same content (the compositor emits one
/// per MIME update) must not duplicate the transfer or apply it twice.
#[test]
fn duplicate_owner_change_events_are_offered_once() {
    let mut net = pair();
    let copy = text("one copy, several owner-change events");
    net.copy(NODE_A, copy.clone());
    net.copy(NODE_A, copy.clone());
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 500);
    net.assert_quiet("duplicate owner-change events");
    assert_eq!(applied(&net, NODE_B), vec![copy]);
}

/// Slow/fast peers and role changes: A→B, then B→A, then A→B again.
#[test]
fn both_directions_and_role_swaps_preserve_the_clipboard() {
    let mut net = pair();
    let first = text("A to B");
    net.copy(NODE_A, first.clone());
    net.run_until_applied(NODE_B, 1, 500);
    assert_eq!(net.current(NODE_B), first);

    let second = text("B to A");
    net.copy(NODE_B, second.clone());
    net.run_until_applied(NODE_A, 1, 500);
    assert_eq!(net.current(NODE_A), second);

    let third = text("A to B, again");
    net.copy(NODE_A, third.clone());
    net.run_until_applied(NODE_B, 2, 500);

    assert_eq!(applied(&net, NODE_A), vec![second]);
    assert_eq!(applied(&net, NODE_B), vec![first, third]);
    net.assert_quiet("bidirectional copies");
}

// ---------------------------------------------------------------------------
// Tests: copy before / during connection handoff
// ---------------------------------------------------------------------------

/// A copy made while no peer is connected must be offered as soon as a
/// clipboard-capable peer connects (service reads the current clipboard).
#[test]
fn copy_before_peer_registration_is_offered_on_connect() {
    let mut net = Net::new();
    net.add_node(NODE_A, 0xAAAA_0000_0000_0001);
    net.add_node(NODE_B, 0xBBBB_0000_0000_0002);
    let copy = text("copied before the link came up");
    net.copy(NODE_A, copy.clone());
    assert_eq!(
        net.wire.queued(),
        0,
        "nothing may be sent while no peer is registered"
    );
    assert_eq!(net.current(NODE_A), copy);

    net.connect(NODE_A, NODE_B);
    net.run_until_applied(NODE_B, 1, 500);
    assert_eq!(net.current(NODE_B), copy);
    assert_eq!(applied(&net, NODE_B), vec![copy]);
}

/// A copy whose change event was never seen by the service (backend started
/// after the copy) is still offered through the read-on-connect path.
#[test]
fn copy_missed_by_a_starting_backend_is_offered_on_connect() {
    let mut net = Net::new();
    net.add_node(NODE_A, 0xAAAA_0000_0000_0001);
    net.add_node(NODE_B, 0xBBBB_0000_0000_0002);
    let copy = text("copied while the backend was starting");
    // The OS clipboard holds the new content, but no `changed()` event reached
    // the sync task (the backend was not ready yet).
    net.node_mut(NODE_A).clipboard.copy_local(copy.clone());

    net.connect(NODE_A, NODE_B);
    net.run_until_applied(NODE_B, 1, 500);
    assert_eq!(net.current(NODE_B), copy);
}

/// A copy made in the same tick the peer is registered must supersede the
/// read-on-connect offer: the peer ends up with the newest content only, never
/// with a stale offer dangling.
#[test]
fn copy_during_connection_setup_supersedes_the_resync_offer() {
    let mut net = Net::new();
    net.add_node(NODE_A, 0xAAAA_0000_0000_0001);
    net.add_node(NODE_B, 0xBBBB_0000_0000_0002);
    let stale = text("clipboard when the link came up");
    let newest = text("copy right after connect");
    net.copy(NODE_A, stale);
    net.connect(NODE_A, NODE_B);
    net.copy(NODE_A, newest.clone());
    net.run_until(2_000, |net| net.applied_count(NODE_B) == 1);
    assert_eq!(net.current(NODE_B), newest);
    assert_eq!(applied(&net, NODE_B), vec![newest]);
    net.assert_quiet("copy during connect");
}

/// The first copy after a connection is established must survive one lost
/// offer on startup (the reported "sometimes the first Ctrl+C does not reach
/// the other machine" symptom).
#[test]
fn first_copy_after_connect_survives_a_lost_offer() {
    let mut net = pair();
    net.set_link(
        NODE_A,
        NODE_B,
        Link {
            drop_first: 1,
            loss_percent: 10,
            seed: 0x0bad_c0de,
            ..Link::lossless()
        },
    );
    let copy = text("first copy after connect");
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 2_000);
    assert_eq!(net.current(NODE_B), copy);
    assert_eq!(applied(&net, NODE_B), vec![copy]);
    assert_eq!(
        net.node(NODE_A).sync.last_error(),
        None,
        "the offer retransmission must recover without surfacing an error"
    );
}

/// Brief transport hiccup (500 ms, shorter than the retry budget): the copy is
/// retransmitted and delivered without the user pressing Ctrl+C again.
#[test]
fn copy_while_transport_briefly_fails_is_retried() {
    let mut net = pair();
    net.set_transport(NODE_A, NODE_B, false);
    let copy = text("copy during a 500 ms transport hiccup");
    net.copy(NODE_A, copy.clone());
    net.settle(50); // 500 ms of failed `send_raw`
    assert_eq!(net.applied_count(NODE_B), 0);
    net.set_transport(NODE_A, NODE_B, true);
    net.run_until_applied(NODE_B, 1, 2_000);
    assert_eq!(net.current(NODE_B), copy);
}

/// The reported workaround must keep working: after a transfer failed during a
/// long transport outage, pressing Ctrl+C again re-offers the identical content.
#[test]
fn pressing_copy_again_after_a_failed_transfer_repairs_delivery() {
    let mut net = pair();
    net.set_transport(NODE_A, NODE_B, false);
    let copy = text("copy lost during a long outage");
    net.copy(NODE_A, copy.clone());
    net.settle(300); // 3 s: the transfer exhausts its retries and aborts
    assert_eq!(net.applied_count(NODE_B), 0);
    assert!(
        net.node(NODE_A).sync.last_error().is_some(),
        "the aborted transfer should be reported"
    );

    net.set_transport(NODE_A, NODE_B, true);
    // The user presses Ctrl+C again (identical content) after the link recovers.
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 2_000);
    assert_eq!(net.current(NODE_B), copy);
    assert_eq!(applied(&net, NODE_B), vec![copy]);
}

/// Residual robustness (beyond the stated telemetry: a >1.2 s silent transport
/// outage with no peer disconnect event requires a periodic pending re-offer).
#[test]
fn copy_during_a_long_transport_outage_is_recovered_without_a_second_copy() {
    let mut net = pair();
    net.set_transport(NODE_A, NODE_B, false);
    let copy = text("copy lost during a long outage, no second copy");
    net.copy(NODE_A, copy.clone());
    net.settle(400); // 4 s: the transfer aborts
    net.set_transport(NODE_A, NODE_B, true);
    assert!(
        net.try_run_until(3_000, |net| net.applied_count(NODE_B) == 1),
        "copy during a long transport outage was never recovered without a new copy:\n{}",
        net.diagnostics()
    );
    assert_eq!(net.current(NODE_B), copy);
}

// ---------------------------------------------------------------------------
// Tests: disconnect / reconnect / backend restart
// ---------------------------------------------------------------------------

/// Reported symptom acceptance test: if the link goes down in the middle of a
/// transfer, reconnecting must resynchronize the clipboard the user copied --
/// without requiring a second Ctrl+C.
#[test]
fn reconnect_after_an_interrupted_transfer_resyncs_the_current_clipboard() {
    let mut net = pair();
    let copy = bytes_copy(vec![b'x'; 8 * 1024]);
    net.copy(NODE_A, copy.clone());
    net.deliver(); // offer + the first chunks reach the peer, transfer incomplete
    net.tick();
    assert_eq!(net.applied_count(NODE_B), 0);

    net.wire.clear(); // in-flight datagrams are lost with the link
    net.disconnect(NODE_A, NODE_B);
    net.settle(5);
    net.connect(NODE_A, NODE_B);

    assert!(
        net.try_run_until(2_000, |net| net.applied_count(NODE_B) == 1),
        "reconnect did not resync the clipboard copied before the transfer was\n\
         interrupted (reported symptom: first Ctrl+C lost, second one works):\n{}",
        net.diagnostics()
    );
    assert_eq!(net.current(NODE_B), copy);
    assert_monotonic_subsequence(
        "reconnect resync",
        net.applied(NODE_B),
        std::slice::from_ref(&copy),
    );
}

/// Reported symptom acceptance test: even if the reconnect resync is missed,
/// pressing Ctrl+C again with the *same* content after reconnecting must
/// re-offer it (the dedupe of `last_sent` must not swallow the retry).
#[test]
fn copying_again_after_reconnect_is_offered_again() {
    let mut net = pair();
    let copy = bytes_copy(vec![b'y'; 8 * 1024]);
    net.copy(NODE_A, copy.clone());
    net.deliver();
    net.tick();
    net.wire.clear();
    net.disconnect(NODE_A, NODE_B);
    net.settle(5);
    net.connect(NODE_A, NODE_B);
    // The user presses Ctrl+C again after reconnecting.
    net.copy(NODE_A, copy.clone());

    assert!(
        net.try_run_until(2_000, |net| net.applied_count(NODE_B) == 1),
        "pressing Ctrl+C again with identical content after reconnect was ignored\n\
         (reported symptom workaround does not work):\n{}",
        net.diagnostics()
    );
    assert_eq!(net.current(NODE_B), copy);
}

/// A clipboard backend restart (portal/owner restart) recreates the sync state:
/// the current clipboard must be offered again to the connected peer.
#[test]
fn clipboard_survives_a_backend_restart_and_resyncs() {
    let mut net = pair();
    // Multi-chunk payload: the first round delivers an incomplete transfer.
    let copy = bytes_copy(vec![b'r'; 8 * 1024]);
    net.copy(NODE_A, copy.clone());
    net.deliver();
    net.tick();
    assert_eq!(net.applied_count(NODE_B), 0);

    net.restart_backend(NODE_A);
    net.run_until_applied(NODE_B, 1, 2_000);
    assert_eq!(net.current(NODE_B), copy);
}

/// Disabling clipboard sync mid-transfer clears in-flight state; stale frames
/// arriving afterwards must not be applied, and re-enabling resyncs the current
/// clipboard.
#[test]
fn disabling_mid_transfer_clears_stale_state_and_re_enabling_resyncs() {
    let mut net = pair();
    let copy = bytes_copy(vec![b'z'; 8 * 1024]);
    net.copy(NODE_A, copy.clone());
    net.deliver();
    net.tick();
    assert_eq!(net.applied_count(NODE_B), 0);

    net.disable(NODE_A);
    net.disable(NODE_B);
    // Frames already queued on the wire when sync was disabled arrive late.
    let stale = net.take_datagrams();
    net.deliver_datagrams(stale);
    net.settle(5);
    assert_eq!(
        net.applied_count(NODE_B),
        0,
        "stale frames were applied after disabling clipboard sync"
    );

    net.enable(NODE_B);
    net.enable(NODE_A);
    assert!(
        net.try_run_until(2_000, |net| net.applied_count(NODE_B) == 1),
        "re-enabling clipboard sync did not resync the current clipboard:\n{}",
        net.diagnostics()
    );
    assert_eq!(net.current(NODE_B), copy);
    net.assert_quiet("re-enable resync");
}

/// A copy made while sync is disabled must be offered when sync is enabled
/// again (the service reads the current clipboard on re-registration).
#[test]
fn copy_while_disabled_is_offered_after_re_enable() {
    let mut net = pair();
    net.disable(NODE_A);
    let copy = text("copied while clipboard sync was disabled");
    net.copy(NODE_A, copy.clone());
    assert_eq!(net.wire.queued(), 0);

    net.enable(NODE_A);
    net.run_until_applied(NODE_B, 1, 1_000);
    assert_eq!(net.current(NODE_B), copy);
}

/// A new copy cancels the transfer still in flight: delayed chunks of the old
/// copy (sent before the cancel) must never overwrite the newer clipboard.
#[test]
fn superseded_transfer_never_overwrites_a_newer_copy() {
    let mut net = pair();
    let old = bytes_copy(vec![b'o'; 8 * 1024]);
    let new = text("newer copy wins");
    net.copy(NODE_A, old);

    // Drive the sender without delivering so a full first window is captured.
    let mut captured = Vec::new();
    for _ in 0..3 {
        captured.extend(net.take_datagrams());
        net.tick();
    }
    assert!(captured.len() > 3, "expected several old datagrams");

    // Only the offer and the first two chunks reach the peer.
    net.deliver_datagrams(captured.drain(..3).collect());
    net.tick();
    assert_eq!(net.applied_count(NODE_B), 0);

    net.copy(NODE_A, new.clone());
    net.run_until(2_000, |net| net.applied_count(NODE_B) == 1);
    assert_eq!(net.current(NODE_B), new);

    // Deliver the delayed old chunks, then the cancel/new-copy frames.
    net.deliver_datagrams(captured);
    net.deliver();
    net.settle(20);
    assert_eq!(
        applied(&net, NODE_B),
        vec![new.clone()],
        "a stale chunk of the superseded transfer was applied"
    );
    assert_eq!(net.current(NODE_B), new);
}

/// An incomplete incoming transfer is dropped after the receiver timeout and is
/// never applied; a later copy still works.
#[test]
fn incomplete_incoming_transfer_times_out_without_applying() {
    let mut net = pair();
    let partial = bytes_copy(vec![b'p'; 8 * 1024]);
    net.copy(NODE_A, partial);
    net.deliver(); // the offer + the first chunks arrive
    net.tick();
    assert_eq!(net.applied_count(NODE_B), 0);

    // No further chunks arrive: the sender's retries fail too.
    net.wire.clear();
    net.set_transport(NODE_A, NODE_B, false);
    net.settle(3_100); // > 30 s receiver timeout
    assert_eq!(
        net.applied_count(NODE_B),
        0,
        "an incomplete transfer must never be applied"
    );

    net.set_transport(NODE_A, NODE_B, true);
    let copy = text("copy after the timeout");
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 500);
    assert_eq!(net.current(NODE_B), copy);
}

/// Frames from a peer that has already disconnected must not be applied after
/// the disconnect (stale frames queued before the teardown).
#[test]
fn frames_queued_before_a_disconnect_are_not_applied_after_it() {
    let mut net = pair();
    let copy = bytes_copy(vec![b'q'; 8 * 1024]);
    net.copy(NODE_A, copy);

    // Capture the transfer and deliver everything but the last chunk.
    let mut captured = Vec::new();
    while net.applied_count(NODE_B) == 0 {
        captured.extend(net.take_datagrams());
        net.tick();
        if captured.len() >= 8 {
            break;
        }
    }
    assert!(captured.len() >= 2);
    let last = captured.pop().expect("at least one datagram");
    net.deliver_datagrams(captured);
    net.disconnect(NODE_A, NODE_B);
    net.settle(3);

    // Shortly after the disconnect the network delivers the last datagram.
    net.inject_raw(NODE_A, NODE_B, last.bytes);
    net.settle(20);
    assert_eq!(
        net.applied_count(NODE_B),
        0,
        "a clipboard snapshot from a disconnected peer was applied"
    );
}

// ---------------------------------------------------------------------------
// Tests: echo suppression
// ---------------------------------------------------------------------------

/// Applying a remote snapshot must not make the local change watcher echo it
/// back (would create an endless copy loop between the two machines).
#[test]
fn applied_remote_content_is_not_echoed_back() {
    let mut net = pair();
    let copy = text("no echo");
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 500);
    net.settle(10); // let the final acknowledgements settle
    assert_eq!(net.wire.queued(), 0);

    // The OS reports another owner-change event for the content we just wrote.
    let current = net.current(NODE_B);
    net.copy(NODE_B, current);
    assert_eq!(
        net.wire.queued(),
        0,
        "remote content was echoed back to the sender"
    );
    net.settle(10);
    assert_eq!(net.applied_count(NODE_A), 0);
    assert_eq!(applied(&net, NODE_B), vec![copy]);
}

/// An offer carrying our own origin (bounced back by a peer) must be ignored,
/// even if chunks follow.
#[test]
fn offers_with_our_own_origin_are_ignored() {
    let mut net = pair();
    let impostor = ClipboardSnapshot::from_items([(Format::Text, b"bounced back".to_vec())]);
    net.inject_raw(
        NODE_A,
        NODE_B,
        ClipboardFrame::Offer {
            serial: 99,
            origin: 0xBBBB_0000_0000_0002, // NODE_B's own origin
            hash: impostor.hash(),
            items: impostor.wire_items(),
        }
        .encode(),
    );
    for (format, data) in impostor.iter() {
        net.inject_raw(
            NODE_A,
            NODE_B,
            ClipboardFrame::Chunk {
                serial: 99,
                mime: format.id(),
                offset: 0,
                total: data.len() as u32,
                data: data.to_vec(),
            }
            .encode(),
        );
    }
    net.settle(10);
    assert_eq!(
        net.applied_count(NODE_B),
        0,
        "an offer with our own origin was applied"
    );

    // A legitimate copy still works afterwards.
    let copy = text("still works");
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 500);
    assert_eq!(net.current(NODE_B), copy);
}

// ---------------------------------------------------------------------------
// Tests: limits, protocol bounds, hostile input
// ---------------------------------------------------------------------------

/// A snapshot over the configured representation limit is rejected locally
/// with an error and never sent; the session keeps working afterwards.
#[test]
fn oversized_local_snapshot_is_rejected_without_poisoning_the_session() {
    let mut net = pair();
    let limits = Limits::default();
    let oversized = bytes_copy(vec![0u8; limits.max_text + 1]);
    net.copy(NODE_A, oversized);
    assert_eq!(
        net.wire.queued(),
        0,
        "an oversized snapshot must not be sent"
    );
    let error = net
        .node(NODE_A)
        .sync
        .last_error()
        .expect("the oversize must be reported");
    assert!(error.contains("limit"), "unexpected error: {error}");
    assert_eq!(net.applied_count(NODE_B), 0);

    let copy = text("small enough");
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 500);
    assert_eq!(net.current(NODE_B), copy);
}

/// The total snapshot cap is enforced exactly at the boundary, and a live
/// limit change applies to the next copy without a restart.
#[test]
fn configured_limits_apply_live_and_enforce_the_boundary() {
    let mut net = pair();
    let limits = Limits {
        max_text: 100,
        max_html: 100,
        max_image: 100,
        max_snapshot: 300,
    };
    net.set_limits(NODE_A, limits);
    net.set_limits(NODE_B, limits);

    let boundary = ClipboardSnapshot::from_items([
        (Format::Text, vec![b't'; 100]),
        (Format::Html, vec![b'h'; 100]),
        (Format::Png, vec![b'p'; 100]),
    ]);
    net.copy(NODE_A, boundary.clone());
    net.run_until_applied(NODE_B, 1, 2_000);
    assert_eq!(net.current(NODE_B), boundary);

    // The shared limit is lowered live (config reload): the next copy is too big.
    let smaller = Limits {
        max_snapshot: 299,
        ..limits
    };
    net.set_limits(NODE_A, smaller);
    let too_big = text("the next copy is one byte too big".repeat(10).as_str());
    net.copy(NODE_A, too_big);
    let error = net
        .node(NODE_A)
        .sync
        .last_error()
        .expect("the snapshot over the lowered total cap must be reported");
    assert!(error.contains("limit"), "unexpected error: {error}");
}

/// An inbound offer that exceeds the receiver's limits is rejected and
/// reported; it can never be applied, and the session keeps working.
#[test]
fn inbound_oversized_offer_is_rejected_with_an_error() {
    let mut net = pair();
    // The sender accepts a larger text size than the receiver.
    net.set_limits(
        NODE_A,
        Limits {
            max_text: 1024 * 1024,
            ..Limits::default()
        },
    );
    let oversized = bytes_copy(vec![b'x'; 300 * 1024]);
    net.copy(NODE_A, oversized);
    net.settle(50);
    assert_eq!(
        net.applied_count(NODE_B),
        0,
        "an oversized inbound offer was applied"
    );
    let error = net
        .node(NODE_B)
        .sync
        .last_error()
        .expect("the oversized inbound offer must be reported");
    assert!(
        error.contains("limit") || error.contains("exceed"),
        "unexpected receiver error: {error}"
    );

    let copy = text("normal copy after the rejected offer");
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 1_000);
    assert_eq!(net.current(NODE_B), copy);
}

/// Every datagram the sync layer produces must stay within the protocol limits
/// and be canonically encoded by the production codec.
#[test]
fn clipboard_frames_stay_within_the_protocol_limits() {
    let mut net = pair();
    let copy = ClipboardSnapshot::from_items([
        (Format::Text, vec![b'w'; CHUNK_SIZE * 3 + 5]),
        (Format::Png, vec![0x11; CHUNK_SIZE + 1]),
    ]);
    net.copy(NODE_A, copy.clone());
    let mut max_frame = 0usize;
    let mut rounds = 0;
    while net.applied_count(NODE_B) == 0 {
        rounds += 1;
        assert!(
            rounds < 2_000,
            "transfer did not converge:\n{}",
            net.diagnostics()
        );
        let datagrams = net.take_datagrams();
        for datagram in &datagrams {
            assert!(
                datagram.bytes.len() <= MAX_CLIPBOARD_FRAME,
                "a {} byte frame exceeds MAX_CLIPBOARD_FRAME ({MAX_CLIPBOARD_FRAME})",
                datagram.bytes.len()
            );
            max_frame = max_frame.max(datagram.bytes.len());
            let frame = ClipboardFrame::decode(&datagram.bytes)
                .unwrap_or_else(|error| panic!("produced an undecodable frame: {error}"));
            assert_eq!(
                frame.encode(),
                datagram.bytes,
                "frame encoding is not canonical"
            );
            match &frame {
                ClipboardFrame::Chunk {
                    mime,
                    offset,
                    total,
                    data,
                    ..
                } => {
                    assert!(data.len() <= CHUNK_SIZE);
                    assert_eq!(offset % CHUNK_SIZE as u32, 0);
                    assert!(*total >= data.len() as u32);
                    let format = Format::from_id(*mime).expect("known mime id");
                    assert!(*total as usize <= Limits::default().max_for(format));
                    assert!(matches!(*mime, MIME_TEXT | MIME_HTML | MIME_PNG));
                }
                ClipboardFrame::Offer { items, .. } => {
                    assert!(!items.is_empty());
                    assert!(items.len() <= 3);
                    for (mime, size) in items {
                        let format = Format::from_id(*mime).expect("known mime id");
                        assert!(*size as usize <= Limits::default().max_for(format));
                    }
                }
                _ => {}
            }
        }
        net.deliver_datagrams(datagrams);
        net.tick();
        net.advance();
    }
    assert_eq!(net.current(NODE_B), copy);
    assert!(max_frame <= MAX_CLIPBOARD_FRAME);
}

/// Duplicated datagrams (transport duplicates) must not apply a snapshot twice
/// or corrupt it.
#[test]
fn duplicated_frames_are_tolerated_and_applied_once() {
    let mut net = pair();
    net.set_link(
        NODE_A,
        NODE_B,
        Link {
            duplicate_every: 2,
            ..Link::lossless()
        },
    );
    let copy = bytes_copy(vec![b'd'; 4 * 1024]);
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 2_000);
    net.settle(20);
    assert_eq!(applied(&net, NODE_B), vec![copy.clone()]);
    assert_eq!(net.current(NODE_B), copy);
}

/// Truncated, mistyped and random frames arriving from an authenticated but
/// hostile/broken peer must be dropped gracefully; the session keeps syncing.
#[test]
fn corrupted_frames_never_panic_or_poison_the_session() {
    let mut net = pair();
    let sample = ClipboardSnapshot::from_items([(Format::Text, b"corruption sample".to_vec())]);
    let offer = ClipboardFrame::Offer {
        serial: 500,
        origin: 0xDEAD_BEEF,
        hash: sample.hash(),
        items: sample.wire_items(),
    }
    .encode();

    // Every truncation prefix of a real frame.
    for length in 0..offer.len() {
        net.inject_raw(NODE_A, NODE_B, offer[..length].to_vec());
    }
    // Valid length but wrong frame-type byte / wrong mime / random payload.
    for first in 0u8..=32 {
        let mut bytes = offer.clone();
        bytes[0] = first;
        net.inject_raw(NODE_A, NODE_B, bytes);
    }
    let mut noise = 0x1234_5678_9abc_def0u64;
    for _ in 0..64 {
        noise = noise.wrapping_mul(6364136223846793005).wrapping_add(1);
        let length = (noise % 96) as usize;
        let bytes = (0..length)
            .map(|_| {
                noise = noise.wrapping_mul(6364136223846793005).wrapping_add(1);
                (noise >> 24) as u8
            })
            .collect();
        net.inject_raw(NODE_A, NODE_B, bytes);
    }
    net.settle(20);
    assert_eq!(net.applied_count(NODE_B), 0);
    assert!(
        net.decode_errors() > 0,
        "expected the decoder to reject the corrupt frames"
    );

    let copy = text("healthy after corruption");
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 500);
    assert_eq!(net.current(NODE_B), copy);
}

/// An empty clipboard snapshot (cleared clipboard) is never synced: the peer
/// keeps its content and no error is raised.
#[test]
fn empty_snapshot_is_not_synced() {
    let mut net = pair();
    let copy = text("stays here");
    net.copy(NODE_A, copy.clone());
    net.run_until_applied(NODE_B, 1, 500);

    net.copy(NODE_A, ClipboardSnapshot::default());
    net.settle(20);
    assert_eq!(net.wire.queued(), 0);
    assert_eq!(net.applied_count(NODE_B), 1);
    assert_eq!(net.current(NODE_B), copy);
    assert_eq!(net.node(NODE_A).sync.last_error(), None);
}
