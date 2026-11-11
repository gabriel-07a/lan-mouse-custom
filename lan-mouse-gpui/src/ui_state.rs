//! Pure UI state derivation for the peer cards.
//!
//! This module owns the honesty rules for the connection badge/action: only a
//! recent round-trip may claim "Connected", and an incoming-only session never
//! claims it at all. Keeping it separate from the GPUI render makes the whole
//! decision table unit-testable without a window or GPU.

/// A round-trip reply younger than this proves the outgoing link still works.
///
/// The service pings in rounds of ~2 s and closes a link that stopped answering
/// after two missed rounds; 6 s is conservative and absorbs scheduler jitter
/// without flickering the badge on a single lost packet. Mirrors the GTK
/// frontend's `LINK_FRESH_WINDOW` (docs/audit-connected-truth.md §2.1).
pub(crate) const LINK_FRESH_WINDOW_MS: u64 = 6_000;

/// Honest, proof-based connection state of one logical peer card.
///
/// Derived from the properties the service publishes plus the only direct
/// evidence of a working round-trip: a recent [`ClientState::last_pong_at`].
/// `active_addr` only proves the DTLS handshake completed and `alive` is the
/// peer's "emulation enabled" flag (set optimistically at the handshake), so
/// neither may be used as liveness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// Turned off locally and no peer session.
    Disabled,
    /// Enabled, an outgoing link may exist, but no round-trip was observed yet.
    Connecting,
    /// The link answered before but the peer went silent: never claim Connected.
    Reconnecting,
    /// Link up and answering, but the peer reports its emulation as disabled.
    ConnectedDegraded,
    /// Outgoing link established and answering, peer emulation enabled.
    Connected,
    /// Same, plus an accepted incoming session from the peer.
    ConnectedBidirectional,
    /// Incoming session accepted (peer may control us while it enters); the
    /// outgoing direction may still be missing or unconfirmed.
    Receiving,
}

impl LinkState {
    /// Badge text. Only states backed by a recent round-trip claim "Connected".
    pub(crate) fn badge(self) -> &'static str {
        match self {
            Self::Disabled => "Disconnected",
            Self::Connecting => "Connecting…",
            Self::Reconnecting => "Reconnecting…",
            Self::Connected | Self::ConnectedBidirectional | Self::ConnectedDegraded => "Connected",
            // An accepted incoming session does not prove the peer entered this
            // screen or sends input, so this deliberately says neither.
            Self::Receiving => "Peer connected",
        }
    }

    /// Whether the card must use the amber "not fully healthy" styling.
    pub(crate) fn is_degraded(self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Reconnecting | Self::ConnectedDegraded
        )
    }
}

/// The single action a card offers for its derived state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowAction {
    /// Turn the client on and connect to it.
    Connect,
    /// Stop a pending attempt (the service keeps retrying while the client is
    /// enabled; deactivating is the only cancellation the IPC offers).
    Cancel,
    /// Turn the client off.
    Disconnect,
}

impl RowAction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Connect => "Connect",
            Self::Cancel => "Cancel",
            Self::Disconnect => "Disconnect",
        }
    }

    /// Whether pressing the action turns the client on.
    pub(crate) fn wants_activation(self) -> bool {
        matches!(self, Self::Connect)
    }
}

/// Derive the card state from the service-published properties and the last
/// observed `Pong` timestamp.
pub(crate) fn link_state(
    active: bool,
    alive: bool,
    has_outgoing: bool,
    has_incoming: bool,
    last_pong_at: Option<u64>,
    now_ms: u64,
) -> LinkState {
    if !active {
        return if has_incoming {
            LinkState::Receiving
        } else {
            LinkState::Disabled
        };
    }
    if !has_outgoing {
        return if has_incoming {
            LinkState::Receiving
        } else {
            LinkState::Connecting
        };
    }
    let fresh =
        last_pong_at.is_some_and(|seen| now_ms.saturating_sub(seen) <= LINK_FRESH_WINDOW_MS);
    if !fresh {
        return if last_pong_at.is_some() {
            LinkState::Reconnecting
        } else {
            LinkState::Connecting
        };
    }
    match (alive, has_incoming) {
        (true, true) => LinkState::ConnectedBidirectional,
        (true, false) => LinkState::Connected,
        (false, _) => LinkState::ConnectedDegraded,
    }
}

/// Pick the single action for a state (same table as the GTK frontend).
pub(crate) fn row_action(state: LinkState, active: bool) -> RowAction {
    match state {
        LinkState::Disabled => RowAction::Connect,
        LinkState::Connecting => RowAction::Cancel,
        LinkState::Reconnecting => RowAction::Disconnect,
        LinkState::Connected | LinkState::ConnectedBidirectional | LinkState::ConnectedDegraded => {
            RowAction::Disconnect
        }
        LinkState::Receiving if active => RowAction::Cancel,
        // A peer that only receives input is controlled by the other machine;
        // "Disconnect" must actually close that incoming session. It used to
        // map to "Also connect", so clicking it did nothing and the peer kept
        // controlling this machine.
        LinkState::Receiving => RowAction::Disconnect,
    }
}

/// Whether the per-device outgoing sensitivity control is meaningful.
///
/// It scales motion sent to the peer, so a device that only receives input
/// (the other PC controls this one) must not offer it.
pub(crate) fn outgoing_sensitivity_visible(active: bool) -> bool {
    active
}
