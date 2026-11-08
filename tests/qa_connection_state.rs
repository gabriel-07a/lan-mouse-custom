//! QA regression tests for the `ClientState` transport contract (IPC/serde).
//!
//! Reported bug: the GTK row said "Connected" while the connection did not
//! work, and the address shown could change between IPv4/IPv6. The UI fix
//! (task-19, conn-state-analyst) may add optional fields such as a connection
//! phase / last-pong timestamp to `ClientState`; those must stay additive so
//! old GUI ↔ new service (and vice versa) keep working.
//!
//! These tests pin the wire contract and the semantic distinction the fix
//! relies on: `alive` is the peer's input-emulation flag, NOT liveness; the
//! link evidence is `active_addr`/`incoming_addr`. They only touch public IPC
//! types (owned by task-19); no production file is edited here.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};

use lan_mouse_ipc::ClientState;
use serde_json::{Value, json};

fn state() -> ClientState {
    ClientState {
        active: true,
        active_addr: Some("192.0.2.10:4242".parse::<SocketAddr>().expect("addr")),
        alive: true,
        rtt_ms: Some(7),
        jitter_ms: Some(1),
        loss_percent: Some(0),
        last_pong_at: Some(1_760_000_000_123),
        dns_ips: vec![
            "192.0.2.10".parse::<IpAddr>().expect("v4"),
            "2001:db8::10".parse::<IpAddr>().expect("v6"),
        ],
        ips: HashSet::from_iter([
            "192.0.2.10".parse::<IpAddr>().expect("v4"),
            "2001:db8::10".parse::<IpAddr>().expect("v6"),
        ]),
        has_pressed_keys: false,
        resolving: false,
        peer_commit: Some(*b"abc12345"),
        peer_proto_version: 1,
        peer_capabilities: 0,
        peer_caps_known: true,
        incoming_addr: Some(
            "[2001:db8::10]:4242"
                .parse::<SocketAddr>()
                .expect("inbound"),
        ),
        peer_incoming_caps: Some(0b111),
        peer_outgoing_caps: Some(0b111),
    }
}

/// The full state must survive a JSON round-trip (the GUI and the CLI receive
/// exactly this payload through the IPC socket).
#[test]
fn client_state_round_trips_through_json() {
    let original = state();
    let encoded = serde_json::to_string(&original).expect("serialize");
    let decoded: ClientState = serde_json::from_str(&encoded).expect("deserialize");

    assert_eq!(decoded.active, original.active);
    assert_eq!(decoded.active_addr, original.active_addr);
    assert_eq!(decoded.incoming_addr, original.incoming_addr);
    assert_eq!(decoded.alive, original.alive);
    assert_eq!(decoded.rtt_ms, original.rtt_ms);
    assert_eq!(decoded.last_pong_at, original.last_pong_at);
    assert_eq!(
        decoded.ips, original.ips,
        "ips is a set: serialization must not change its contents"
    );
    assert_eq!(decoded.peer_commit, original.peer_commit);
    assert_eq!(decoded.peer_incoming_caps, original.peer_incoming_caps);
    assert_eq!(decoded.peer_outgoing_caps, original.peer_outgoing_caps);
}

/// Addresses stay encoded as socket-address strings, including IPv6 with
/// brackets; the row subtitle/address display depends on this shape.
#[test]
fn addresses_serialize_as_socket_addr_strings() {
    let v4: Value = serde_json::to_value(state()).expect("serialize v4");
    assert_eq!(v4["active_addr"], json!("192.0.2.10:4242"));
    assert_eq!(v4["incoming_addr"], json!("[2001:db8::10]:4242"));

    let mut v6 = state();
    v6.active_addr = Some("[2001:db8::20]:4242".parse::<SocketAddr>().expect("v6"));
    let v6: Value = serde_json::to_value(v6).expect("serialize v6");
    assert_eq!(v6["active_addr"], json!("[2001:db8::20]:4242"));
}

/// `alive` (peer input-emulation flag from the last Pong) and link evidence
/// (`active_addr`) are independent: a live link with peer emulation disabled
/// is a degraded connection, not a dead one. The UI fix derives its
/// "Connected (degraded)" state from exactly this combination.
#[test]
fn alive_is_independent_from_link_evidence() {
    let mut degraded = state();
    degraded.alive = false;
    degraded.rtt_ms = None;

    let encoded = serde_json::to_string(&degraded).expect("serialize");
    let decoded: ClientState = serde_json::from_str(&encoded).expect("deserialize");
    assert!(!decoded.alive);
    assert!(
        decoded.active_addr.is_some(),
        "a link that was established must keep active_addr when the peer disables emulation"
    );

    // And the opposite: no link yet, but the peer answered a Pong at some
    // point (legacy data) — the row must not claim Connected from `alive`
    // alone. This is the data-level invariant checked by the GTK unit tests;
    // here we only guarantee the fields are transmitted separately.
    let mut no_link = state();
    no_link.active_addr = None;
    no_link.incoming_addr = None;
    let encoded = serde_json::to_string(&no_link).expect("serialize");
    let decoded: ClientState = serde_json::from_str(&encoded).expect("deserialize");
    assert!(decoded.active_addr.is_none() && decoded.incoming_addr.is_none());
}

/// A payload produced by an *older* service (optional fields absent) must
/// still deserialize, keeping the frontend compatible with a service that has
/// not been restarted after an update.
#[test]
fn payload_without_optional_fields_deserializes() {
    let mut value: Value = serde_json::to_value(state()).expect("serialize");
    let object = value.as_object_mut().expect("object");
    for optional in [
        "rtt_ms",
        "last_pong_at",
        "peer_proto_version",
        "peer_capabilities",
        "peer_caps_known",
        "incoming_addr",
        "peer_incoming_caps",
        "peer_outgoing_caps",
    ] {
        object.remove(optional);
    }

    let decoded: ClientState = serde_json::from_value(value).expect("deserialize old payload");
    assert_eq!(decoded.rtt_ms, None);
    assert_eq!(decoded.last_pong_at, None);
    assert_eq!(decoded.incoming_addr, None);
    assert_eq!(decoded.peer_proto_version, 0);
    assert!(!decoded.peer_caps_known);
    assert!(decoded.active_addr.is_some());
}

/// A payload from a *newer* service (fields this build does not know) must not
/// break the frontend: serde ignores unknown keys by default, so adding a
/// connection phase is backward compatible.
#[test]
fn unknown_future_fields_are_ignored() {
    let mut value: Value = serde_json::to_value(state()).expect("serialize");
    value["connection_phase"] = json!("degraded");
    value["remote_entered"] = json!(true);

    let decoded: ClientState = serde_json::from_value(value).expect("deserialize future payload");
    assert!(decoded.active_addr.is_some());
    assert!(decoded.alive);
}

/// The CLI `list` status mapping (lan-mouse-cli) treats
/// `active && active_addr.is_some() && alive` as connected; guard the field
/// names used by that mapping against accidental renames.
#[test]
fn connected_evidence_fields_exist_in_json() {
    let value: Value = serde_json::to_value(state()).expect("serialize");
    for field in [
        "active",
        "active_addr",
        "alive",
        "rtt_ms",
        "last_pong_at",
        "last_pong_at",
        "incoming_addr",
        "ips",
        "resolving",
    ] {
        assert!(value.get(field).is_some(), "missing IPC field: {field}");
    }
}

/// `last_pong_at` is the only proof of a working round-trip (task-19 fix).
/// The frontend must be able to tell these states apart:
///   * alive=true, last_pong_at=Some -> peer answered and emulation is on;
///   * alive=false, last_pong_at=Some -> link works but peer emulation is off
///     (degraded, not "connecting");
///   * alive=true/None, last_pong_at=None -> handshake only, liveness unproven
///     (the old false-"Connected" case).
#[test]
fn last_pong_at_tracks_liveness_independently_from_alive() {
    let mut answered_emulation_off = state();
    answered_emulation_off.alive = false;
    let value: Value = serde_json::to_value(&answered_emulation_off).expect("serialize");
    assert_eq!(value["alive"], json!(false));
    assert!(value["last_pong_at"].as_u64().is_some());
    assert!(value["active_addr"].is_string());

    let mut handshake_only = state();
    handshake_only.last_pong_at = None;
    handshake_only.alive = true;
    let value: Value = serde_json::to_value(&handshake_only).expect("serialize");
    assert!(value["last_pong_at"].is_null());
    assert!(value["active_addr"].is_string());

    // Round-trip keeps the values distinct (no conflation on the wire).
    let encoded = serde_json::to_string(&answered_emulation_off).expect("serialize");
    let decoded: ClientState = serde_json::from_str(&encoded).expect("deserialize");
    assert!(decoded.last_pong_at.is_some());
    assert!(!decoded.alive);
}
