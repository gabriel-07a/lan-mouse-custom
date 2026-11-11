//! Pure decision logic extracted from the `Service` runtime.
//!
//! Everything in this module must stay free of I/O, locks, clocks and channels
//! so it can be unit-tested deterministically. The `Service` keeps the
//! orchestration; these functions answer the "what should happen next"
//! questions and are the first piece of the planned `ServiceCore` split.

use std::net::SocketAddr;

#[cfg(test)]
use std::collections::HashSet;

use lan_mouse_ipc::{ClientHandle, Position};

/// What `ensure_bidirectional` should do when an incoming session exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BidirectionalAction {
    /// The outgoing link is already enabled: nothing to do.
    AlreadyActive,
    /// Auto-created reverse client with no discovered listen port: dialing the
    /// default port would create a phantom connection, so stay receiving-only.
    NeedsPort,
    /// Activate the outgoing link, optionally first setting the discovered port
    /// of an auto-created reverse client.
    Activate { port: Option<u16> },
}

/// Decide how to make an incoming session bidirectional.
pub(crate) fn bidirectional_action(
    is_active: bool,
    auto_reverse: bool,
    known_port: Option<u16>,
) -> BidirectionalAction {
    if is_active {
        return BidirectionalAction::AlreadyActive;
    }
    if auto_reverse {
        return match known_port {
            Some(port) => BidirectionalAction::Activate { port: Some(port) },
            None => BidirectionalAction::NeedsPort,
        };
    }
    BidirectionalAction::Activate { port: None }
}

/// Addresses that should receive a return hand-off request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReturnTargets {
    /// Incoming sessions whose edge matches the requested position.
    pub matching: Vec<SocketAddr>,
    /// When no edge matches but exactly one peer is driving us, hand control
    /// back to it anyway: a mismatching position must never trap the cursor.
    pub fallback: Option<SocketAddr>,
}

/// Select the return hand-off targets for `requested` from the live incoming
/// sessions.
pub(crate) fn return_targets(
    incoming: &[(SocketAddr, Position)],
    requested: Position,
) -> ReturnTargets {
    let matching: Vec<SocketAddr> = incoming
        .iter()
        .filter(|(_, pos)| *pos == requested)
        .map(|(addr, _)| *addr)
        .collect();
    if !matching.is_empty() {
        return ReturnTargets {
            matching,
            fallback: None,
        };
    }
    let fallback = if incoming.len() == 1 {
        Some(incoming[0].0)
    } else {
        None
    };
    ReturnTargets {
        matching: Vec::new(),
        fallback,
    }
}

/// Whether an incoming connection request should reopen the frontend prompt.
///
/// `is_new`/`cert_changed` are always worth a prompt. An existing pending
/// request used to suppress the prompt forever: dismissing the dialog left the
/// entry in the store, and later Connect clicks produced no visible request on
/// the other PC. Reopening after a bounded cooldown keeps handshake retries
/// from spamming the modal while still making a deliberate retry visible.
pub(crate) fn should_notify_connection_attempt(
    is_new: bool,
    cert_changed: bool,
    last_notified: Option<std::time::Instant>,
    now: std::time::Instant,
    cooldown: std::time::Duration,
) -> bool {
    is_new
        || cert_changed
        || last_notified.is_none_or(|last| now.saturating_duration_since(last) >= cooldown)
}

/// Tracks when the frontend was last told about a pending connection request.
///
/// This state used to live inline in the service loop, which made the
/// "dismissed request never reopens" bug untestable. Keeping it here gives the
/// reconnect/retry sequence a deterministic unit test.
#[derive(Debug, Default)]
pub(crate) struct ConnectionAttemptNotifier {
    notified: std::collections::HashMap<String, std::time::Instant>,
}

impl ConnectionAttemptNotifier {
    /// Decide whether to reopen the approval prompt, and remember the attempt.
    pub(crate) fn should_notify(
        &mut self,
        fingerprint: &str,
        is_new: bool,
        cert_changed: bool,
        now: std::time::Instant,
        cooldown: std::time::Duration,
    ) -> bool {
        let should = should_notify_connection_attempt(
            is_new,
            cert_changed,
            self.notified.get(fingerprint).copied(),
            now,
            cooldown,
        );
        if should {
            self.notified.insert(fingerprint.to_string(), now);
        }
        should
    }

    /// Forget the cooldown once the request was accepted or denied.
    pub(crate) fn clear(&mut self, fingerprint: &str) {
        self.notified.remove(fingerprint);
    }
}

/// Auto-created reverse clients for the same certificate that should be merged
/// into `handle`.
pub(crate) fn duplicate_reverse_clients(
    candidates: &[(ClientHandle, Option<String>)],
    handle: ClientHandle,
    fingerprint: &str,
) -> Vec<ClientHandle> {
    candidates
        .iter()
        .filter(|(other, other_fingerprint)| {
            *other != handle && other_fingerprint.as_deref() == Some(fingerprint)
        })
        .map(|(other, _)| *other)
        .collect()
}

/// Handles that appear in `set` but not in `registered`, used to prune stale
/// auto-reverse bookkeeping.
#[cfg(test)]
pub(crate) fn stale_handles(
    set: &HashSet<ClientHandle>,
    registered: &HashSet<ClientHandle>,
) -> Vec<ClientHandle> {
    set.difference(registered).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, last)),
            4242,
        )
    }

    #[test]
    fn bidirectional_action_covers_every_case() {
        assert_eq!(
            bidirectional_action(true, false, None),
            BidirectionalAction::AlreadyActive
        );
        assert_eq!(
            bidirectional_action(false, false, None),
            BidirectionalAction::Activate { port: None }
        );
        assert_eq!(
            bidirectional_action(false, true, None),
            BidirectionalAction::NeedsPort
        );
        assert_eq!(
            bidirectional_action(false, true, Some(4243)),
            BidirectionalAction::Activate { port: Some(4243) }
        );
    }

    #[test]
    fn return_targets_prefers_matching_edges() {
        let incoming = [
            (addr(1), Position::Left),
            (addr(2), Position::Left),
            (addr(3), Position::Right),
        ];
        assert_eq!(
            return_targets(&incoming, Position::Left),
            ReturnTargets {
                matching: vec![addr(1), addr(2)],
                fallback: None,
            }
        );
    }

    #[test]
    fn return_targets_falls_back_to_the_only_session_on_a_mismatch() {
        let incoming = [(addr(1), Position::Left)];
        assert_eq!(
            return_targets(&incoming, Position::Right),
            ReturnTargets {
                matching: Vec::new(),
                fallback: Some(addr(1)),
            }
        );
    }

    #[test]
    fn return_targets_never_guesses_between_multiple_mismatching_sessions() {
        let incoming = [(addr(1), Position::Left), (addr(2), Position::Right)];
        assert_eq!(
            return_targets(&incoming, Position::Top),
            ReturnTargets::default()
        );
        assert_eq!(return_targets(&[], Position::Top), ReturnTargets::default());
    }

    #[test]
    fn duplicate_reverse_clients_match_only_the_same_certificate() {
        let candidates = [
            (1, Some("aa".to_string())),
            (2, Some("aa".to_string())),
            (3, Some("bb".to_string())),
            (4, None),
            (5, Some("aa".to_string())),
        ];
        assert_eq!(
            duplicate_reverse_clients(&candidates, 5, "aa"),
            vec![1, 2],
            "the kept handle and other certificates must not be merged"
        );
        assert!(duplicate_reverse_clients(&candidates, 5, "missing").is_empty());
    }

    #[test]
    fn connection_attempt_prompt_reopens_after_the_cooldown() {
        let now = std::time::Instant::now();
        let cooldown = std::time::Duration::from_secs(10);
        assert!(should_notify_connection_attempt(
            true,
            false,
            Some(now),
            now,
            cooldown
        ));
        assert!(should_notify_connection_attempt(
            false,
            true,
            Some(now),
            now,
            cooldown
        ));
        assert!(should_notify_connection_attempt(
            false, false, None, now, cooldown
        ));
        assert!(!should_notify_connection_attempt(
            false,
            false,
            Some(now),
            now,
            cooldown
        ));
        assert!(should_notify_connection_attempt(
            false,
            false,
            Some(now),
            now + cooldown,
            cooldown
        ));
    }

    #[test]
    fn notifier_reopens_a_dismissed_request_after_cooldown_and_clears_on_decision() {
        use std::time::{Duration, Instant};
        let mut notifier = ConnectionAttemptNotifier::default();
        let cooldown = Duration::from_secs(10);
        let now = Instant::now();

        assert!(notifier.should_notify("peer", true, false, now, cooldown));
        assert!(
            !notifier.should_notify("peer", false, false, now + Duration::from_secs(1), cooldown),
            "handshake retries inside the cooldown must not reopen the modal"
        );
        assert!(
            notifier.should_notify("peer", false, true, now + Duration::from_secs(2), cooldown),
            "a changed certificate always opens the prompt"
        );
        assert!(
            notifier.should_notify(
                "peer",
                false,
                false,
                now + Duration::from_secs(2) + cooldown,
                cooldown
            ),
            "a dismissed request must become visible again after the cooldown"
        );

        notifier.clear("peer");
        assert!(
            notifier.should_notify(
                "peer",
                false,
                false,
                now + Duration::from_secs(2) + cooldown,
                cooldown
            ),
            "accepting/denying clears the cooldown for a later explicit request"
        );
    }

    #[test]
    fn notifier_tracks_each_peer_independently() {
        use std::time::{Duration, Instant};
        let mut notifier = ConnectionAttemptNotifier::default();
        let now = Instant::now();
        let cooldown = Duration::from_secs(10);
        assert!(notifier.should_notify("a", true, false, now, cooldown));
        assert!(notifier.should_notify("b", true, false, now, cooldown));
        assert!(!notifier.should_notify("a", false, false, now, cooldown));
        assert!(!notifier.should_notify("b", false, false, now, cooldown));
    }

    #[test]
    fn stale_handles_are_the_difference() {
        let set = HashSet::from([1, 2, 3]);
        let registered = HashSet::from([2, 3, 4]);
        assert_eq!(stale_handles(&set, &registered), vec![1]);
    }
}
