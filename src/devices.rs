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
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How long an unauthorized connection request stays visible before it is
/// forgotten. The peer keeps retrying while its client is active
/// (250 ms - 5 s backoff), so a request that is not answered simply expires.
pub(crate) const PENDING_TTL: Duration = Duration::from_secs(300);

/// Hard cap on simultaneously tracked unauthorized identities. The listener
/// already rate-limits handshakes per IP, but a spoofed/rotating source must
/// not be able to grow the pending map without bound.
pub(crate) const MAX_PENDING_REQUESTS: usize = 64;

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

/// A revoked fingerprint.
///
/// The revocation can expire: a temporary deny (CLI `deny --for 1h`) stops
/// blocking the device once the deadline passes, while a permanent deny stays
/// until an explicit authorization lifts it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeniedRecord {
    pub fingerprint: String,
    /// unix timestamp in seconds; `None` means permanent
    pub expires: Option<u64>,
    /// optional human readable reason
    pub reason: Option<String>,
}

impl DeniedRecord {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn permanent(fingerprint: String) -> Self {
        Self {
            fingerprint,
            expires: None,
            reason: None,
        }
    }

    pub(crate) fn until(fingerprint: String, expires: Option<u64>, reason: Option<String>) -> Self {
        Self {
            fingerprint,
            expires,
            reason,
        }
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.expires
            .is_some_and(|expires| unix_secs(SystemTime::now()) >= expires)
    }
}

/// Portable permission policy: devices, address rules and revoked identities.
///
/// This is what `export-policy` writes and `import-policy` reads. It contains
/// no addresses of configured links and no session state, so it can be copied
/// between machines.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PolicyBundle {
    #[serde(default)]
    pub devices: Vec<PolicyDevice>,
    #[serde(default)]
    pub address_rules: Vec<PolicyAddressRule>,
    #[serde(default)]
    pub denied: Vec<PolicyDenied>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PolicyDevice {
    pub fingerprint: String,
    pub name: String,
    pub incoming: DirectionPolicy,
    pub outgoing: DirectionPolicy,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PolicyAddressRule {
    pub address: String,
    pub incoming: DirectionPolicy,
    pub outgoing: DirectionPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PolicyDenied {
    pub fingerprint: String,
    #[serde(default)]
    pub expires: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// In-memory store for devices, revoked fingerprints and pending requests.
#[derive(Default)]
pub(crate) struct DeviceStore {
    devices: HashMap<String, Device>,
    denied: HashMap<String, DeniedRecord>,
    /// devices granted only for the current session; never persisted
    ephemeral: HashSet<String>,
    pending: HashMap<String, PendingDevice>,
}

impl DeviceStore {
    pub(crate) fn seed(
        &mut self,
        devices: impl IntoIterator<Item = Device>,
        denied: impl IntoIterator<Item = DeniedRecord>,
    ) {
        for device in devices {
            self.devices.insert(device.fingerprint.clone(), device);
        }
        for record in denied {
            if !record.is_expired() {
                self.denied.insert(record.fingerprint.clone(), record);
            }
        }
        // A revoked device must not stay authorized.
        self.devices
            .retain(|fingerprint, _| !self.denied.contains_key(fingerprint));
    }

    pub(crate) fn device(&self, fingerprint: &str) -> Option<&Device> {
        self.devices.get(fingerprint)
    }

    pub(crate) fn device_mut(&mut self, fingerprint: &str) -> Option<&mut Device> {
        self.devices.get_mut(fingerprint)
    }

    pub(crate) fn put(&mut self, device: Device) {
        self.denied.remove(&device.fingerprint);
        self.ephemeral.remove(&device.fingerprint);
        self.devices.insert(device.fingerprint.clone(), device);
    }

    /// Grant a device for the current session only. The record is never
    /// persisted and is dropped when the peer disconnects.
    pub(crate) fn put_ephemeral(&mut self, device: Device) {
        self.denied.remove(&device.fingerprint);
        self.ephemeral.insert(device.fingerprint.clone());
        self.devices.insert(device.fingerprint.clone(), device);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_ephemeral(&self, fingerprint: &str) -> bool {
        self.ephemeral.contains(fingerprint)
    }

    /// Remove a session-only grant (if it is one) and report whether it was.
    pub(crate) fn remove_ephemeral(&mut self, fingerprint: &str) -> bool {
        if !self.ephemeral.remove(fingerprint) {
            return false;
        }
        self.devices.remove(fingerprint);
        true
    }

    /// Devices that must be written to the config file (not session grants).
    pub(crate) fn persistent_devices(&self) -> impl Iterator<Item = &Device> {
        self.devices
            .values()
            .filter(|device| !self.ephemeral.contains(&device.fingerprint))
    }

    /// Forget a device without revoking its fingerprint.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn remove(&mut self, fingerprint: &str) -> Option<Device> {
        self.ephemeral.remove(fingerprint);
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

    /// Revoke a device permanently.
    pub(crate) fn deny(&mut self, fingerprint: String) {
        self.deny_until(fingerprint, None, None);
    }

    /// Revoke a device, optionally only until `expires` (unix seconds).
    pub(crate) fn deny_until(
        &mut self,
        fingerprint: String,
        expires: Option<u64>,
        reason: Option<String>,
    ) {
        self.devices.remove(&fingerprint);
        self.ephemeral.remove(&fingerprint);
        self.denied.insert(
            fingerprint.clone(),
            DeniedRecord::until(fingerprint, expires, reason),
        );
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
        self.denied
            .get(fingerprint)
            .is_some_and(|record| !record.is_expired())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pending(&self, fingerprint: &str) -> Option<&PendingDevice> {
        self.pending.get(fingerprint)
    }

    /// Non-expired revocations, sorted for stable config output.
    pub(crate) fn denied(&self) -> Vec<DeniedRecord> {
        let mut denied = self
            .denied
            .values()
            .filter(|record| !record.is_expired())
            .cloned()
            .collect::<Vec<_>>();
        denied.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
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
                if self.pending.len() >= MAX_PENDING_REQUESTS {
                    self.prune_pending();
                    if self.pending.len() >= MAX_PENDING_REQUESTS {
                        return false;
                    }
                }
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

    /// Serialize the persistent part of the policy for `export-policy`.
    pub(crate) fn export_bundle(
        &self,
        address_rules: &[crate::trusted::AddressRule],
    ) -> PolicyBundle {
        PolicyBundle {
            devices: self
                .persistent_devices()
                .map(|device| PolicyDevice {
                    fingerprint: device.fingerprint.clone(),
                    name: device.name.clone(),
                    incoming: device.incoming,
                    outgoing: device.outgoing,
                    source: Some(device.source.as_str().to_string()),
                })
                .collect(),
            address_rules: address_rules
                .iter()
                .map(|rule| PolicyAddressRule {
                    address: rule.net.to_string(),
                    incoming: rule.incoming,
                    outgoing: rule.outgoing,
                })
                .collect(),
            denied: self
                .denied()
                .into_iter()
                .map(|record| PolicyDenied {
                    fingerprint: record.fingerprint,
                    expires: record.expires,
                    reason: record.reason,
                })
                .collect(),
        }
    }

    /// Replace the policy with an imported bundle.
    ///
    /// Everything is validated before anything is applied, so a malformed
    /// file leaves the running policy untouched. Session grants are kept.
    pub(crate) fn import_bundle(
        &mut self,
        bundle: &PolicyBundle,
        address_rules: &mut Vec<crate::trusted::AddressRule>,
    ) -> Result<(), String> {
        let mut devices = Vec::with_capacity(bundle.devices.len());
        for device in &bundle.devices {
            let Some(fingerprint) = crate::crypto::normalize_fingerprint(&device.fingerprint)
            else {
                return Err(format!(
                    "invalid device fingerprint: {}",
                    device.fingerprint
                ));
            };
            devices.push(Device {
                fingerprint,
                name: device.name.clone(),
                incoming: device.incoming,
                outgoing: device.outgoing,
                source: device
                    .source
                    .as_deref()
                    .map(DeviceSource::parse)
                    .unwrap_or_default(),
            });
        }
        let mut rules = Vec::with_capacity(bundle.address_rules.len());
        for rule in &bundle.address_rules {
            let Some(net) = crate::trusted::TrustedNet::parse(&rule.address) else {
                return Err(format!("invalid address rule: {}", rule.address));
            };
            rules.push(crate::trusted::AddressRule::new(
                net,
                rule.incoming,
                rule.outgoing,
            ));
        }
        let mut denied = Vec::with_capacity(bundle.denied.len());
        for record in &bundle.denied {
            let Some(fingerprint) = crate::crypto::normalize_fingerprint(&record.fingerprint)
            else {
                return Err(format!(
                    "invalid denied fingerprint: {}",
                    record.fingerprint
                ));
            };
            denied.push(DeniedRecord::until(
                fingerprint,
                record.expires,
                record.reason.clone(),
            ));
        }

        // Validation passed: apply atomically from the caller's point of view.
        let ephemeral = self.ephemeral.clone();
        self.devices.clear();
        self.denied.clear();
        for device in devices {
            self.devices.insert(device.fingerprint.clone(), device);
        }
        for record in denied {
            if !record.is_expired() {
                self.denied.insert(record.fingerprint.clone(), record);
            }
        }
        self.ephemeral = ephemeral;
        for fingerprint in self.denied.keys() {
            self.devices.remove(fingerprint);
            self.ephemeral.remove(fingerprint);
        }
        *address_rules = rules;
        Ok(())
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
            vec![DeniedRecord::permanent(fp(2))],
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
    fn expired_denies_stop_blocking() {
        let mut store = DeviceStore::default();
        let now = unix_secs(SystemTime::now());
        store.deny_until(fp(1), Some(now.saturating_sub(1)), None);
        assert!(!store.is_denied(&fp(1)), "an expired deny does not block");
        assert!(
            store.denied().is_empty(),
            "expired denies are not persisted"
        );
        store.deny_until(fp(2), Some(now + 3600), Some("temporary".into()));
        assert!(store.is_denied(&fp(2)));
        assert_eq!(store.denied().len(), 1);
    }

    #[test]
    fn ephemeral_grants_are_kept_out_of_the_config_and_expire() {
        let mut store = DeviceStore::default();
        store.put_ephemeral(Device::new(fp(1), "visitor".into()));
        assert!(store.is_known(&fp(1)));
        assert!(store.is_ephemeral(&fp(1)));
        assert_eq!(
            store.persistent_devices().count(),
            0,
            "session grants must not be written to the config"
        );
        assert!(store.remove_ephemeral(&fp(1)));
        assert!(!store.is_known(&fp(1)));
        assert!(!store.remove_ephemeral(&fp(1)), "already gone");
    }

    #[test]
    fn policy_bundle_roundtrips_through_json() {
        let mut store = DeviceStore::default();
        store.put(Device {
            fingerprint: fp(1),
            name: "laptop".into(),
            incoming: DirectionPolicy::allow_all(),
            outgoing: DirectionPolicy::deny(),
            source: DeviceSource::Manual,
        });
        store.deny_until(fp(2), Some(unix_secs(SystemTime::now()) + 60), None);
        let rules = vec![crate::trusted::AddressRule::new(
            crate::trusted::TrustedNet::parse("10.0.0.0/24").expect("cidr"),
            DirectionPolicy::allow_all(),
            DirectionPolicy::deny(),
        )];
        let bundle = store.export_bundle(&rules);
        let json = serde_json::to_string(&bundle).expect("serialize");
        let decoded: PolicyBundle = serde_json::from_str(&json).expect("decode");

        let mut imported = DeviceStore::default();
        let mut imported_rules = Vec::new();
        imported
            .import_bundle(&decoded, &mut imported_rules)
            .expect("import");
        assert!(imported.is_known(&fp(1)));
        assert_eq!(imported.outgoing(&fp(1)), DirectionPolicy::deny());
        assert!(imported.is_denied(&fp(2)));
        assert_eq!(imported_rules.len(), 1);
        assert_eq!(imported_rules[0].net.to_string(), "10.0.0.0/24");
    }

    #[test]
    fn invalid_policy_is_rejected_without_changing_the_store() {
        let mut store = DeviceStore::default();
        store.put(Device::new(fp(1), "keep".into()));
        let mut rules = Vec::new();
        let invalid = PolicyBundle {
            devices: vec![PolicyDevice {
                fingerprint: "not-a-fingerprint".into(),
                name: "bad".into(),
                incoming: DirectionPolicy::allow_all(),
                outgoing: DirectionPolicy::deny(),
                source: None,
            }],
            ..Default::default()
        };
        assert!(store.import_bundle(&invalid, &mut rules).is_err());
        assert!(store.is_known(&fp(1)), "existing policy is untouched");
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
    #[test]
    fn pending_requests_are_capped() {
        let mut store = DeviceStore::default();
        for n in 0..(MAX_PENDING_REQUESTS + 10) {
            let fingerprint = format!("{n:064x}");
            store.pending_upsert(&fingerprint, addr(4000), None, None);
        }
        assert!(
            store.pending_count() <= MAX_PENDING_REQUESTS,
            "pending map must stay bounded"
        );
    }
}
