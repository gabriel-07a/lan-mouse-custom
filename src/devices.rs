//! Device identity, permission policies and pending connection requests.
//!
//! A [`Device`] is identified by the SHA-256 fingerprint of its certificate,
//! independent of the address it is reached at. Each device carries two
//! [`DirectionPolicy`] values:
//!
//! * `incoming` — what the peer may do on **this** machine. It is the
//!   authoritative policy and is enforced by the listener/emulation path.
//! * `outgoing` — what **this** machine may do on the peer. It is a local
//!   guard enforced when input is forwarded.
//!
//! Both directions carry an independent set of [`Capabilities`], so a device
//! can e.g. only move the pointer and use the clipboard but never type, or
//! control one machine while not being controllable in return.

use lan_mouse_ipc::{DirectionPolicy, PendingView};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How long an unauthorized connection request stays visible before it is
/// forgotten. The peer keeps retrying while its client is active
/// (250 ms - 5 s backoff), so a request that is not answered simply expires.
pub(crate) const PENDING_TTL: Duration = Duration::from_secs(300);

/// Where the authorization of a device came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DeviceSource {
    /// Authorized interactively by the user (dialog or CLI).
    #[default]
    Manual,
    /// Accepted while the single-use pairing window was open.
    Pairing,
    /// Adopted because the address matched a trusted address rule.
    AddressRule,
    /// Migrated from the pre-permissions config format.
    Legacy,
}

impl DeviceSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Pairing => "pairing",
            Self::AddressRule => "address",
            Self::Legacy => "legacy",
        }
    }

    pub(crate) fn parse(value: &str) -> Self {
        match value {
            "pairing" => Self::Pairing,
            "address" => Self::AddressRule,
            "legacy" => Self::Legacy,
            _ => Self::Manual,
        }
    }
}

/// A device known to this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Device {
    pub fingerprint: String,
    pub name: String,
    pub incoming: DirectionPolicy,
    pub outgoing: DirectionPolicy,
    pub source: DeviceSource,
}

impl Device {
    pub(crate) fn new(fingerprint: String, name: String) -> Self {
        Self {
            fingerprint,
            name,
            incoming: DirectionPolicy::allow_all(),
            outgoing: DirectionPolicy::allow_all(),
            source: DeviceSource::default(),
        }
    }
}

/// An unauthorized device that is currently trying to connect.
#[derive(Clone, Debug)]
pub(crate) struct PendingDevice {
    pub fingerprint: String,
    pub addr: SocketAddr,
    pub name: Option<String>,
    /// previous certificate seen for the same address/name, if any
    pub previous_fingerprint: Option<String>,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    pub attempts: u32,
}

impl PendingDevice {
    fn to_view(&self) -> PendingView {
        PendingView {
            fingerprint: self.fingerprint.clone(),
            addr: self.addr,
            name: self.name.clone(),
            sas: String::new(),
            previous_fingerprint: self.previous_fingerprint.clone(),
            first_seen: unix_secs(self.first_seen),
            last_seen: unix_secs(self.last_seen),
            attempts: self.attempts,
        }
    }
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// In-memory store for devices, revoked fingerprints and pending requests.
#[derive(Default)]
pub(crate) struct DeviceStore {
    devices: HashMap<String, Device>,
    denied: HashSet<String>,
    pending: HashMap<String, PendingDevice>,
}

impl DeviceStore {
    pub(crate) fn seed(
        &mut self,
        devices: impl IntoIterator<Item = Device>,
        denied: impl IntoIterator<Item = String>,
    ) {
        for device in devices {
            self.devices.insert(device.fingerprint.clone(), device);
        }
        self.denied.extend(denied);
        // A revoked device must not stay authorized.
        self.devices
            .retain(|fingerprint, _| !self.denied.contains(fingerprint));
    }

    pub(crate) fn device(&self, fingerprint: &str) -> Option<&Device> {
        self.devices.get(fingerprint)
    }

    pub(crate) fn device_mut(&mut self, fingerprint: &str) -> Option<&mut Device> {
        self.devices.get_mut(fingerprint)
    }

    pub(crate) fn put(&mut self, device: Device) {
        self.denied.remove(&device.fingerprint);
        self.devices.insert(device.fingerprint.clone(), device);
    }

    /// Forget a device without revoking its fingerprint.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn remove(&mut self, fingerprint: &str) -> Option<Device> {
        self.devices.remove(fingerprint)
    }

    pub(crate) fn is_known(&self, fingerprint: &str) -> bool {
        self.devices.contains_key(fingerprint)
    }

    pub(crate) fn devices(&self) -> impl Iterator<Item = &Device> {
        self.devices.values()
    }

    /// Replace the permission policies of a known device.
    pub(crate) fn set_policy(
        &mut self,
        fingerprint: &str,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    ) -> bool {
        match self.devices.get_mut(fingerprint) {
            Some(device) => {
                device.incoming = incoming;
                device.outgoing = outgoing;
                true
            }
            None => false,
        }
    }

    /// Rename a device. Returns `true` when the name actually changed.
    pub(crate) fn rename(&mut self, fingerprint: &str, name: String) -> bool {
        match self.devices.get_mut(fingerprint) {
            Some(device) if device.name != name => {
                device.name = name;
                true
            }
            _ => false,
        }
    }

    /// Revoke a device: forget it and remember the fingerprint as denied.
    ///
    /// Denying removes the device record first, so a revoked device never
    /// keeps a stale policy around and `denied()` is the single source of
    /// truth for the listener.
    pub(crate) fn deny(&mut self, fingerprint: String) {
        self.devices.remove(&fingerprint);
        self.denied.insert(fingerprint);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Incoming policy of a device.
    ///
    /// Unknown devices are denied: no event from a peer is ever accepted
    /// before the service has registered a policy for its fingerprint. A
    /// device that was just auto-authorized is registered within the same
    /// event-loop turn, long before the user can move the cursor.
    pub(crate) fn incoming(&self, fingerprint: &str) -> DirectionPolicy {
        self.devices
            .get(fingerprint)
            .map(|device| device.incoming)
            .unwrap_or_default()
    }

    /// Outgoing policy of a device. Unknown devices are not controllable.
    pub(crate) fn outgoing(&self, fingerprint: &str) -> DirectionPolicy {
        self.devices
            .get(fingerprint)
            .map(|device| device.outgoing)
            .unwrap_or_default()
    }

    /// Display name of a device, if known.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn name_of(&self, fingerprint: &str) -> Option<&str> {
        self.devices
            .get(fingerprint)
            .map(|device| device.name.as_str())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_denied(&self, fingerprint: &str) -> bool {
        self.denied.contains(fingerprint)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pending(&self, fingerprint: &str) -> Option<&PendingDevice> {
        self.pending.get(fingerprint)
    }

    /// Revoked fingerprints, sorted for stable config output.
    pub(crate) fn denied(&self) -> Vec<String> {
        let mut denied = self.denied.iter().cloned().collect::<Vec<_>>();
        denied.sort();
        denied
    }

    /// Register (or refresh) a connection request of an unauthorized device.
    ///
    /// Returns `true` when this is the first attempt, so the frontend can be
    /// notified about a new request.
    pub(crate) fn pending_upsert(
        &mut self,
        fingerprint: &str,
        addr: SocketAddr,
        name: Option<String>,
        previous_fingerprint: Option<String>,
    ) -> bool {
        let now = SystemTime::now();
        match self.pending.get_mut(fingerprint) {
            Some(pending) => {
                pending.addr = addr;
                if name.as_ref().is_some_and(|name| !name.trim().is_empty()) {
                    pending.name = name;
                }
                pending.last_seen = now;
                pending.attempts = pending.attempts.saturating_add(1);
                if previous_fingerprint.is_some() {
                    pending.previous_fingerprint = previous_fingerprint;
                }
                false
            }
            None => {
                self.pending.insert(
                    fingerprint.to_string(),
                    PendingDevice {
                        fingerprint: fingerprint.to_string(),
                        addr,
                        name,
                        previous_fingerprint,
                        first_seen: now,
                        last_seen: now,
                        attempts: 1,
                    },
                );
                true
            }
        }
    }

    pub(crate) fn pending_remove(&mut self, fingerprint: &str) -> Option<PendingDevice> {
        self.pending.remove(fingerprint)
    }

    /// Drop requests that have not been seen for [`PENDING_TTL`].
    pub(crate) fn prune_pending(&mut self) {
        let now = SystemTime::now();
        self.pending.retain(|_, pending| {
            now.duration_since(pending.last_seen)
                .map(|age| age < PENDING_TTL)
                .unwrap_or(true)
        });
    }

    pub(crate) fn pending_views(&mut self) -> Vec<PendingView> {
        self.prune_pending();
        let mut pending = self.pending.values().collect::<Vec<_>>();
        pending.sort_by_key(|pending| pending.first_seen);
        pending.iter().map(|pending| pending.to_view()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lan_mouse_ipc::Capabilities;
    use std::net::{IpAddr, Ipv4Addr};

    fn fp(n: u8) -> String {
        let byte = format!("{n:02x}");
        vec![byte; 32].join(":")
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn seed_registers_devices_and_denies_revoked_ones() {
        let mut store = DeviceStore::default();
        store.seed(
            vec![
                Device::new(fp(1), "laptop".into()),
                Device::new(fp(2), "desktop".into()),
            ],
            vec![fp(2)],
        );
        assert!(store.is_known(&fp(1)));
        assert!(!store.is_known(&fp(2)), "revoked devices are removed");
        assert!(store.is_denied(&fp(2)));
    }

    #[test]
    fn unknown_devices_get_the_safe_defaults() {
        let store = DeviceStore::default();
        // No events from an unknown device are accepted.
        assert!(!store.incoming(&fp(9)).allowed);
        assert!(!store.incoming(&fp(9)).allows_input());
        // They are never controllable in the outgoing direction.
        assert!(!store.outgoing(&fp(9)).allowed);
    }

    #[test]
    fn policies_are_per_direction_and_per_capability() {
        let mut store = DeviceStore::default();
        store.put(Device::new(fp(1), "laptop".into()));
        assert!(store.set_policy(
            &fp(1),
            DirectionPolicy {
                allowed: true,
                capabilities: Capabilities::CLIPBOARD,
            },
            DirectionPolicy::deny(),
        ));
        let device = store.device(&fp(1)).expect("device");
        assert!(device.incoming.allows(Capabilities::CLIPBOARD));
        assert!(!device.incoming.allows(Capabilities::KEYBOARD));
        assert!(!device.incoming.allows_input());
        assert!(!device.outgoing.allowed);
        assert!(store.outgoing(&fp(1)).capabilities.is_empty());
    }

    #[test]
    fn deny_wins_and_can_be_lifted_by_an_explicit_put() {
        let mut store = DeviceStore::default();
        store.put(Device::new(fp(1), "laptop".into()));
        store.deny(fp(1));
        assert!(store.is_denied(&fp(1)));
        assert!(!store.is_known(&fp(1)));
        store.put(Device::new(fp(1), "laptop".into()));
        assert!(!store.is_denied(&fp(1)));
        assert!(store.is_known(&fp(1)));
    }

    #[test]
    fn pending_requests_are_counted_and_pruned() {
        let mut store = DeviceStore::default();
        assert!(store.pending_upsert(&fp(1), addr(4000), Some("peer".into()), None));
        assert!(!store.pending_upsert(&fp(1), addr(4001), None, None));
        let pending = store.pending(&fp(1)).expect("pending");
        assert_eq!(pending.attempts, 2);
        assert_eq!(pending.addr, addr(4001));
        assert_eq!(pending.name.as_deref(), Some("peer"));

        let views = store.pending_views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].attempts, 2);

        // Age the request past the TTL and make sure it is forgotten.
        store.pending.get_mut(&fp(1)).expect("pending").last_seen =
            SystemTime::now() - PENDING_TTL - Duration::from_secs(1);
        assert_eq!(store.pending_views().len(), 0);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn rename_and_remove_are_reflected() {
        let mut store = DeviceStore::default();
        store.put(Device::new(fp(1), "old".into()));
        assert!(store.rename(&fp(1), "new".into()));
        assert!(!store.rename(&fp(1), "new".into()), "no change, no diff");
        assert_eq!(store.name_of(&fp(1)), Some("new"));
        assert!(store.remove(&fp(1)).is_some());
        assert!(!store.is_known(&fp(1)));
    }

    #[test]
    fn source_roundtrips_through_its_wire_name() {
        for source in [
            DeviceSource::Manual,
            DeviceSource::Pairing,
            DeviceSource::AddressRule,
            DeviceSource::Legacy,
        ] {
            assert_eq!(DeviceSource::parse(source.as_str()), source);
        }
    }
}
