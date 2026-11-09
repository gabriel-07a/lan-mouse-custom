use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    rc::Rc,
};

use slab::Slab;

use lan_mouse_ipc::{ClientConfig, ClientHandle, ClientState, Position};

use crate::config::ConfigClient;

#[derive(Clone, Default)]
pub struct ClientManager {
    clients: Rc<RefCell<Slab<(ClientConfig, ClientState)>>>,
    /// Last address that actually established a session with this client.
    ///
    /// Kept only in memory (deliberately not part of [`ClientState`], which is
    /// the frontend IPC type): it is a hint for the connection layer so a
    /// reconnect tries the address/family that worked last before falling back
    /// to the other DNS answer, instead of racing A and AAAA on every attempt.
    preferred: Rc<RefCell<HashMap<ClientHandle, SocketAddr>>>,
}

impl ClientManager {
    /// get all clients
    pub fn clients(&self) -> Vec<(ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(_, c)| c.clone())
            .collect::<Vec<_>>()
    }

    pub fn add_with_config(&self, config_client: ConfigClient) -> ClientHandle {
        let config = ClientConfig {
            hostname: config_client.hostname,
            fix_ips: config_client.ips.into_iter().collect(),
            port: config_client.port,
            pos: config_client.pos,
            cmd: config_client.enter_hook,
            leave_cmd: config_client.leave_hook,
            sensitivity: config_client.sensitivity,
            fingerprint: config_client.fingerprint,
        };
        // Lan Mouse is IPv4-only: an AAAA address in the config must not show up
        // in the UI (see update_ips).
        let state = ClientState {
            active: config_client.active,
            ips: HashSet::from_iter(config.fix_ips.iter().filter(|ip| ip.is_ipv4()).cloned()),
            ..Default::default()
        };
        let handle = self.add_client();
        self.set_config(handle, config);
        self.set_state(handle, state);
        handle
    }

    /// add a new client to this manager
    pub fn add_client(&self) -> ClientHandle {
        self.clients.borrow_mut().insert(Default::default()) as ClientHandle
    }

    /// set the config of the given client
    pub fn set_config(&self, handle: ClientHandle, config: ClientConfig) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *c = config;
        }
    }

    /// set the state of the given client
    pub fn set_state(&self, handle: ClientHandle, state: ClientState) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *s = state;
        }
    }

    /// activate the given client
    /// returns, whether the client was activated
    pub fn activate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if !s.active => {
                s.active = true;
                true
            }
            _ => false,
        }
    }

    /// deactivate the given client
    /// returns, whether the client was deactivated
    pub fn deactivate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if s.active => {
                s.active = false;
                s.active_addr = None;
                s.alive = false;
                s.rtt_ms = None;
                s.jitter_ms = None;
                s.loss_percent = None;
                s.last_error = None;
                // No live link, therefore no round-trip proof either.
                s.last_pong_at = None;
                true
            }
            _ => false,
        }
    }

    /// find a client by its address
    pub fn get_client(&self, addr: SocketAddr) -> Option<ClientHandle> {
        // since there shouldn't be more than a handful of clients at any given
        // time this is likely faster than using a HashMap
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (_, s))| {
                if s.active && s.ips.contains(&addr.ip()) {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    /// get the client at the given position
    pub fn client_at(&self, pos: Position) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (c, s))| {
                if s.active && c.pos == pos {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    pub(crate) fn get_hostname(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow_mut()
            .get_mut(handle as usize)
            .and_then(|(c, _)| c.hostname.clone())
    }

    /// get the position of the corresponding client
    pub(crate) fn get_pos(&self, handle: ClientHandle) -> Option<Position> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.pos)
    }

    /// remove a client from the list
    pub fn remove_client(&self, client: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        // The slab reuses vacant handles, so never let a new client inherit
        // the previous session's address preference.
        self.preferred.borrow_mut().remove(&client);
        // remove id from occupied ids
        self.clients.borrow_mut().try_remove(client as usize)
    }

    /// get the config & state of the given client
    pub fn get_state(&self, handle: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        self.clients.borrow().get(handle as usize).cloned()
    }

    /// get the current config & state of all clients
    pub fn get_client_states(&self) -> Vec<(ClientHandle, ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(k, v)| (k as ClientHandle, v.0.clone(), v.1.clone()))
            .collect()
    }

    /// update the fix ips of the client
    pub fn set_fix_ips(&self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.fix_ips = fix_ips
        }
        self.update_ips(handle);
    }

    /// update the dns-ips of the client
    pub fn set_dns_ips(&self, handle: ClientHandle, dns_ips: Vec<IpAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            // IPv4-only product decision: never surface AAAA answers.
            s.dns_ips = dns_ips.into_iter().filter(IpAddr::is_ipv4).collect();
        }
        self.update_ips(handle);
    }

    fn update_ips(&self, handle: ClientHandle) {
        if let Some((c, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            // Lan Mouse is IPv4-only: an AAAA record must not create a
            // second transport or show up in the UI.
            s.ips = c
                .fix_ips
                .iter()
                .cloned()
                .chain(s.dns_ips.iter().cloned())
                .filter(IpAddr::is_ipv4)
                .collect::<HashSet<_>>();
        }
    }

    /// update the hostname of the given client
    /// this automatically clears the active ip address and ips from dns
    pub fn set_hostname(&self, handle: ClientHandle, hostname: Option<String>) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((c, s)) = clients.get_mut(handle as usize) else {
            return false;
        };

        // hostname changed
        if c.hostname != hostname {
            c.hostname = hostname;
            s.active_addr = None;
            s.dns_ips.clear();
            drop(clients);
            self.update_ips(handle);
            true
        } else {
            false
        }
    }

    /// Update the display hostname without resetting `active_addr` or `dns_ips`
    /// when a client was initially registered with no hostname or with a raw IP
    /// string and network discovery subsequently learns the peer's real hostname.
    pub(crate) fn upgrade_hostname_from_discovery(
        &self,
        handle: ClientHandle,
        discovered_hostname: &str,
    ) -> bool {
        let trimmed = discovered_hostname.trim();
        if trimmed.is_empty() {
            return false;
        }
        let mut clients = self.clients.borrow_mut();
        let Some((c, s)) = clients.get_mut(handle as usize) else {
            return false;
        };
        let needs_upgrade = match c.hostname.as_deref() {
            None => true,
            Some(current) => {
                current.trim().is_empty()
                    || current.parse::<IpAddr>().is_ok()
                    || s.ips.iter().any(|ip| ip.to_string() == current)
            }
        };
        if needs_upgrade && c.hostname.as_deref() != Some(trimmed) {
            c.hostname = Some(trimmed.to_string());
            true
        } else {
            false
        }
    }

    /// Record the capabilities the peer grants us / expects from us.
    pub(crate) fn set_peer_permissions(
        &self,
        handle: ClientHandle,
        incoming_caps: Option<u8>,
        outgoing_caps: Option<u8>,
    ) {
        if let Some((_, state)) = self.clients.borrow_mut().get_mut(handle as usize) {
            state.peer_incoming_caps = incoming_caps;
            state.peer_outgoing_caps = outgoing_caps;
        }
    }

    /// update the port of the client
    pub(crate) fn set_port(&self, handle: ClientHandle, port: u16) {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.port != port => {
                c.port = port;
                s.active_addr = s.active_addr.map(|a| SocketAddr::new(a.ip(), port));
            }
            _ => {}
        };
    }

    /// Pin the peer certificate fingerprint of a client.
    ///
    /// Returns `true` when the stored fingerprint changed, so the caller can
    /// persist the config.
    pub(crate) fn set_fingerprint(
        &self,
        handle: ClientHandle,
        fingerprint: Option<String>,
    ) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, _)) if c.fingerprint != fingerprint => {
                c.fingerprint = fingerprint;
                true
            }
            _ => false,
        }
    }

    /// Pinned certificate fingerprint of a client, if any.
    pub(crate) fn fingerprint(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(c, _)| c.fingerprint.clone())
    }

    /// Find the client that is pinned to `fingerprint`.
    pub(crate) fn all_handles_for_fingerprint(&self, fingerprint: &str) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .filter_map(|(handle, (config, _))| {
                (config.fingerprint.as_deref() == Some(fingerprint))
                    .then_some(handle as ClientHandle)
            })
            .collect()
    }

    pub(crate) fn find_by_fingerprint(&self, fingerprint: &str) -> Option<ClientHandle> {
        self.clients.borrow().iter().find_map(|(k, (c, _))| {
            (c.fingerprint.as_deref() == Some(fingerprint)).then_some(k as ClientHandle)
        })
    }

    /// update the position of the client
    /// returns true, if a change in capture position is required (pos changed & client is active)
    pub(crate) fn set_pos(&self, handle: ClientHandle, pos: Position) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.pos != pos => {
                log::info!("update pos {handle} {} -> {}", c.pos, pos);
                c.pos = pos;
                s.active
            }
            _ => false,
        }
    }

    /// update the motion scale of the client (percent)
    pub(crate) fn set_sensitivity(&self, handle: ClientHandle, sensitivity: u16) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, _)) if c.sensitivity != sensitivity => {
                c.sensitivity = sensitivity;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn get_sensitivity(&self, handle: ClientHandle) -> u16 {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.sensitivity)
            .unwrap_or(100)
    }

    /// update the enter hook command of the client
    pub(crate) fn set_enter_hook(&self, handle: ClientHandle, enter_hook: Option<String>) {
        if let Some((c, _s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.cmd = enter_hook;
        }
    }

    /// update the leave hook command of the client
    pub(crate) fn set_leave_hook(&self, handle: ClientHandle, leave_hook: Option<String>) {
        if let Some((c, _s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.leave_cmd = leave_hook;
        }
    }

    /// set resolving status of the client
    pub(crate) fn set_resolving(&self, handle: ClientHandle, status: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.resolving = status;
        }
    }

    /// get the enter hook command
    pub(crate) fn get_enter_cmd(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(c, _)| c.cmd.clone())
    }

    /// get the leave hook command
    pub(crate) fn get_leave_cmd(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(c, _)| c.leave_cmd.clone())
    }

    /// returns all clients that are currently registered
    pub(crate) fn registered_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    /// returns all clients that are currently active
    pub(crate) fn active_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (_, s))| s.active)
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    pub(crate) fn set_active_addr(&self, handle: ClientHandle, addr: Option<SocketAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.active_addr = addr;
        }
    }

    pub(crate) fn set_incoming_addr(&self, handle: ClientHandle, addr: Option<SocketAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.incoming_addr = addr;
        }
    }

    pub(crate) fn incoming_addr(&self, handle: ClientHandle) -> Option<SocketAddr> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| s.incoming_addr)
    }

    /// Find any client (active or inactive) that has `ip` in its IP set.
    pub(crate) fn find_by_ip(&self, ip: IpAddr) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (_, s))| s.ips.contains(&ip).then_some(k as ClientHandle))
    }

    pub(crate) fn set_alive(&self, handle: ClientHandle, alive: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.alive = alive;
        }
    }

    /// smoothed round-trip time measured by the connection layer
    pub(crate) fn set_rtt_ms(&self, handle: ClientHandle, rtt_ms: Option<u32>) {
        let mut clients = self.clients.borrow_mut();
        let Some((_, state)) = clients.get_mut(handle as usize) else {
            return;
        };
        let previous = state.rtt_ms;
        let smoothed = match (state.rtt_ms, rtt_ms) {
            // Exponential moving average: 3/4 old, 1/4 new. Keeps the UI
            // from flickering on a single outlier while still tracking
            // real changes within a few ping rounds.
            (Some(old), Some(new)) => Some(((old as u64 * 3 + new as u64) / 4) as u32),
            (None, new) => new,
            (old, None) => old,
        };
        state.rtt_ms = smoothed;
        if let (Some(old), Some(new)) = (previous, smoothed) {
            let sample = old.abs_diff(new);
            state.jitter_ms = Some(match state.jitter_ms {
                Some(previous) => ((u64::from(previous) * 3 + u64::from(sample)) / 4) as u32,
                None => sample,
            });
        }
    }

    pub(crate) fn rtt_ms(&self, handle: ClientHandle) -> Option<u32> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| s.rtt_ms)
    }

    pub(crate) fn set_last_error(&self, handle: ClientHandle, error: Option<String>) {
        if let Some((_, state)) = self.clients.borrow_mut().get_mut(handle as usize) {
            state.last_error = error;
        }
    }

    pub(crate) fn set_loss_percent(&self, handle: ClientHandle, loss_percent: Option<u8>) {
        if let Some((_, state)) = self.clients.borrow_mut().get_mut(handle as usize) {
            state.loss_percent = loss_percent;
        }
    }

    /// Timestamp (Unix ms) of the last `Pong` received on the outgoing link.
    ///
    /// This is the frontend's proof of a live round-trip: `alive` only reports
    /// whether emulation is enabled on the peer and was set optimistically
    /// right after the handshake, so it must not be used to claim "Connected".
    /// `None` means "no round-trip observed on the current transport".
    pub(crate) fn set_last_pong_at(&self, handle: ClientHandle, timestamp_ms: Option<u64>) {
        if let Some((_, state)) = self.clients.borrow_mut().get_mut(handle as usize) {
            state.last_pong_at = timestamp_ms;
        }
    }

    /// whether the client is currently marked active (receiving our input)
    pub(crate) fn is_active(&self, handle: ClientHandle) -> bool {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
    }

    pub(crate) fn set_peer_hello(
        &self,
        handle: ClientHandle,
        commit: Option<[u8; 8]>,
        proto_version: u8,
        capabilities: u32,
    ) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.peer_commit = commit;
            s.peer_proto_version = proto_version;
            s.peer_capabilities = capabilities;
            s.peer_caps_known = true;
        }
    }

    /// Record capabilities learned from a `Pong`, before any `Hello` arrives.
    pub(crate) fn set_peer_capabilities(&self, handle: ClientHandle, capabilities: u32) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.peer_capabilities = capabilities;
            s.peer_caps_known = true;
        }
    }

    /// Capabilities advertised by the peer, if it reported any.
    pub(crate) fn peer_capabilities(&self, handle: ClientHandle) -> Option<u32> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| s.peer_caps_known.then_some(s.peer_capabilities))
    }

    /// Whether the peer advertised `cap` in its `Hello`. Peers that have not
    /// completed the handshake yet never match.
    pub(crate) fn peer_supports(&self, handle: ClientHandle, cap: u32) -> bool {
        self.peer_capabilities(handle)
            .is_some_and(|caps| caps & cap != 0)
    }

    pub(crate) fn active_addr(&self, handle: ClientHandle) -> Option<SocketAddr> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| s.active_addr)
    }

    /// Remember the address whose handshake last succeeded for this client.
    ///
    /// Used by the connection layer to keep reconnects on the address family
    /// that actually works (stable IPv4 or IPv6) and to use the other DNS
    /// answer only as failover.
    pub(crate) fn record_working_addr(&self, handle: ClientHandle, addr: SocketAddr) {
        if self.clients.borrow().contains(handle as usize) {
            self.preferred.borrow_mut().insert(handle, addr);
        }
    }

    /// Address that last established a session for this client, if any.
    pub(crate) fn preferred_addr(&self, handle: ClientHandle) -> Option<SocketAddr> {
        self.preferred.borrow().get(&handle).copied()
    }

    /// Forget the pinned working address (e.g. after a config/hostname change).
    pub(crate) fn clear_preferred_addr(&self, handle: ClientHandle) {
        self.preferred.borrow_mut().remove(&handle);
    }

    pub(crate) fn alive(&self, handle: ClientHandle) -> bool {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.alive)
            .unwrap_or(false)
    }

    pub(crate) fn get_port(&self, handle: ClientHandle) -> Option<u16> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.port)
    }

    pub(crate) fn get_ips(&self, handle: ClientHandle) -> Option<HashSet<IpAddr>> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.ips.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lan_mouse_ipc::Position;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
    }

    fn config_client(pos: Position, active: bool) -> ConfigClient {
        ConfigClient {
            ips: HashSet::from([ip(10)]),
            hostname: Some("peer".to_string()),
            port: 4242,
            pos,
            active,
            enter_hook: None,
            leave_hook: None,
            sensitivity: 100,
            fingerprint: None,
        }
    }

    #[test]
    fn add_with_config_initializes_state() {
        let manager = ClientManager::default();
        let handle = manager.add_with_config(config_client(Position::Right, true));
        let (config, state) = manager.get_state(handle).expect("client");
        assert_eq!(config.hostname.as_deref(), Some("peer"));
        assert_eq!(config.pos, Position::Right);
        assert!(state.active);
        assert_eq!(state.ips, HashSet::from([ip(10)]));
    }

    #[test]
    fn activate_and_deactivate_report_state_changes() {
        let manager = ClientManager::default();
        let handle = manager.add_client();
        assert!(manager.activate_client(handle));
        assert!(!manager.activate_client(handle), "already active");
        assert!(manager.is_active(handle));
        assert!(manager.deactivate_client(handle));
        assert!(!manager.deactivate_client(handle), "already inactive");
        assert!(!manager.is_active(handle));
    }

    #[test]
    fn client_at_only_matches_active_clients() {
        let manager = ClientManager::default();
        let inactive = manager.add_with_config(config_client(Position::Left, false));
        let active = manager.add_with_config(config_client(Position::Right, true));
        assert_eq!(manager.client_at(Position::Left), None);
        assert_eq!(manager.client_at(Position::Right), Some(active));
        assert_ne!(inactive, active);
    }

    #[test]
    fn get_client_matches_active_ip_addresses() {
        let manager = ClientManager::default();
        let handle = manager.add_with_config(config_client(Position::Left, true));
        let addr = SocketAddr::new(ip(10), 4242);
        assert_eq!(manager.get_client(addr), Some(handle));
        assert_eq!(manager.get_client(SocketAddr::new(ip(11), 4242)), None);

        manager.deactivate_client(handle);
        assert_eq!(
            manager.get_client(addr),
            None,
            "inactive clients never match"
        );
    }

    #[test]
    fn set_port_keeps_active_addr_in_sync() {
        let manager = ClientManager::default();
        let handle = manager.add_with_config(config_client(Position::Left, true));
        manager.set_active_addr(handle, Some(SocketAddr::new(ip(10), 4242)));
        manager.set_port(handle, 5000);
        assert_eq!(
            manager.active_addr(handle),
            Some(SocketAddr::new(ip(10), 5000))
        );
        assert_eq!(manager.get_port(handle), Some(5000));
    }

    #[test]
    fn ipv6_addresses_never_reach_the_client_state() {
        // Lan Mouse is IPv4-only (product decision): a legacy config with an
        // AAAA address must not make the UI/CLI show an IPv6 endpoint.
        let mut config = config_client(Position::Left, true);
        config.ips = HashSet::from([ip(10), "fd00::10".parse().expect("v6")]);
        let manager = ClientManager::default();
        let handle = manager.add_with_config(config);
        let (_, state) = manager.get_state(handle).expect("client");
        assert_eq!(state.ips, HashSet::from([ip(10)]));

        manager.set_dns_ips(handle, vec!["fd00::11".parse().expect("v6"), ip(11)]);
        let (_, state) = manager.get_state(handle).expect("client");
        assert_eq!(state.ips, HashSet::from([ip(10), ip(11)]));
        assert!(
            state.dns_ips.iter().all(IpAddr::is_ipv4),
            "AAAA answers must be filtered before they reach the state"
        );
    }

    #[test]
    fn set_hostname_clears_resolution_state() {
        let manager = ClientManager::default();
        let handle = manager.add_with_config(config_client(Position::Left, true));
        manager.set_dns_ips(handle, vec![ip(11)]);
        manager.set_active_addr(handle, Some(SocketAddr::new(ip(11), 4242)));
        assert!(manager.get_ips(handle).expect("ips").contains(&ip(11)));

        assert!(manager.set_hostname(handle, Some("renamed".to_string())));
        let (config, state) = manager.get_state(handle).expect("client");
        assert_eq!(config.hostname.as_deref(), Some("renamed"));
        assert!(state.dns_ips.is_empty());
        assert_eq!(state.active_addr, None);
        assert_eq!(state.ips, HashSet::from([ip(10)]), "only fixed ips remain");
    }

    #[test]
    fn setting_the_same_hostname_is_a_no_op() {
        let manager = ClientManager::default();
        let handle = manager.add_with_config(config_client(Position::Left, true));
        assert!(!manager.set_hostname(handle, Some("peer".to_string())));
    }

    #[test]
    fn set_pos_reports_capture_change_only_for_active_clients() {
        let manager = ClientManager::default();
        let active = manager.add_with_config(config_client(Position::Left, true));
        let inactive = manager.add_with_config(config_client(Position::Top, false));
        assert!(manager.set_pos(active, Position::Bottom));
        assert!(!manager.set_pos(active, Position::Bottom), "unchanged");
        assert!(!manager.set_pos(inactive, Position::Right));
        assert_eq!(manager.get_pos(active), Some(Position::Bottom));
    }

    #[test]
    fn rtt_uses_an_exponential_moving_average() {
        let manager = ClientManager::default();
        let handle = manager.add_client();
        manager.set_rtt_ms(handle, Some(100));
        assert_eq!(manager.get_state(handle).unwrap().1.rtt_ms, Some(100));
        manager.set_rtt_ms(handle, Some(200));
        assert_eq!(manager.get_state(handle).unwrap().1.rtt_ms, Some(125));
        manager.set_rtt_ms(handle, Some(200));
        assert_eq!(manager.get_state(handle).unwrap().1.rtt_ms, Some(143));
        manager.set_rtt_ms(handle, None);
        assert_eq!(
            manager.get_state(handle).unwrap().1.rtt_ms,
            Some(143),
            "no sample keeps the previous value"
        );
    }

    #[test]
    fn capability_queries_require_a_hello() {
        use lan_mouse_proto::capabilities::{CLIPBOARD, ENTER_ACK};

        let manager = ClientManager::default();
        let handle = manager.add_client();

        // No handshake yet: never assume a peer supports an extension.
        assert_eq!(manager.peer_capabilities(handle), None);
        assert!(!manager.peer_supports(handle, ENTER_ACK));
        assert!(!manager.peer_supports(handle, CLIPBOARD));

        // A legacy peer decodes with version 0 and no capabilities.
        manager.set_peer_hello(handle, Some(*b"legacy00"), 0, 0);
        assert_eq!(manager.peer_capabilities(handle), Some(0));
        assert!(!manager.peer_supports(handle, ENTER_ACK));
        assert!(!manager.peer_supports(handle, CLIPBOARD));

        // A peer from this fork advertises both extensions.
        manager.set_peer_hello(handle, Some(*b"fork0000"), 1, ENTER_ACK | CLIPBOARD);
        assert!(manager.peer_supports(handle, ENTER_ACK));
        assert!(manager.peer_supports(handle, CLIPBOARD));
        assert!(!manager.peer_supports(handle, lan_mouse_proto::capabilities::TIME_SYNC));
    }

    #[test]
    fn peer_hello_and_reset_are_stored() {
        let manager = ClientManager::default();
        let handle = manager.add_client();
        manager.set_peer_hello(handle, Some(*b"abcd1234"), 1, 0b101);
        let (_, state) = manager.get_state(handle).expect("client");
        assert_eq!(state.peer_commit, Some(*b"abcd1234"));
        assert_eq!(state.peer_proto_version, 1);
        assert_eq!(state.peer_capabilities, 0b101);

        manager.set_peer_hello(handle, None, 0, 0);
        let (_, state) = manager.get_state(handle).expect("client");
        assert_eq!(state.peer_commit, None);
        assert_eq!(state.peer_proto_version, 0);
    }

    #[test]
    fn registered_and_active_lists_track_state() {
        let manager = ClientManager::default();
        let a = manager.add_with_config(config_client(Position::Left, true));
        let b = manager.add_with_config(config_client(Position::Right, false));
        assert_eq!(manager.registered_clients().len(), 2);
        assert_eq!(manager.active_clients(), vec![a]);
        manager.activate_client(b);
        let mut active = manager.active_clients();
        active.sort();
        assert_eq!(active, vec![a, b]);
    }

    #[test]
    fn remove_client_returns_its_state() {
        let manager = ClientManager::default();
        let handle = manager.add_with_config(config_client(Position::Left, true));
        let (config, state) = manager.remove_client(handle).expect("removed");
        assert_eq!(config.pos, Position::Left);
        assert!(state.active);
        assert!(manager.get_state(handle).is_none());
        assert!(manager.remove_client(handle).is_none());
    }

    #[test]
    fn deactivate_clears_live_connection_state() {
        let manager = ClientManager::default();
        let handle = manager.add_with_config(config_client(Position::Left, true));
        manager.set_active_addr(handle, Some(SocketAddr::new(ip(10), 4242)));
        manager.set_alive(handle, true);
        manager.set_rtt_ms(handle, Some(15));
        manager.set_last_pong_at(handle, Some(1_700_000_000_123));
        assert!(manager.deactivate_client(handle));
        let (_, state) = manager.get_state(handle).expect("client");
        assert!(!state.active);
        assert_eq!(state.active_addr, None);
        assert!(!state.alive);
        assert_eq!(state.rtt_ms, None);
        assert_eq!(
            state.last_pong_at, None,
            "a deactivated link carries no round-trip proof"
        );
    }

    #[test]
    fn last_pong_at_starts_empty_and_tracks_the_link() {
        let manager = ClientManager::default();
        let handle = manager.add_client();

        // Default/legacy state: no Pong observed yet.
        let (_, state) = manager.get_state(handle).expect("client");
        assert_eq!(state.last_pong_at, None);

        // A Pong on the current transport records its wall-clock timestamp.
        manager.set_last_pong_at(handle, Some(1_700_000_000_123));
        let (_, state) = manager.get_state(handle).expect("client");
        assert_eq!(state.last_pong_at, Some(1_700_000_000_123));

        // Losing the transport clears it: the frontend must fall back to
        // "no proof" instead of showing a stale "Connected".
        manager.set_last_pong_at(handle, None);
        let (_, state) = manager.get_state(handle).expect("client");
        assert_eq!(state.last_pong_at, None);
    }

    #[test]
    fn last_pong_at_ignores_unknown_handles() {
        let manager = ClientManager::default();
        // Must be a no-op, not a panic (same as the other setters).
        manager.set_last_pong_at(9, Some(42));
        assert!(manager.get_state(9).is_none());
    }

    #[test]
    fn upgrade_hostname_from_discovery_replaces_ip_or_blank_only() {
        let manager = ClientManager::default();
        let handle = manager.add_client();
        manager.set_fix_ips(handle, vec![ip(10)]);
        manager.set_hostname(handle, Some("192.168.1.10".to_string()));
        manager.set_active_addr(handle, Some(SocketAddr::new(ip(10), 4242)));

        // Replacing an IP-only hostname preserves active_addr and upgrades the name.
        assert!(manager.upgrade_hostname_from_discovery(handle, "workstation"));
        let (config, state) = manager.get_state(handle).expect("client");
        assert_eq!(config.hostname.as_deref(), Some("workstation"));
        assert_eq!(state.active_addr, Some(SocketAddr::new(ip(10), 4242)));

        // Once the client has a real hostname, discovery does not overwrite it.
        assert!(!manager.upgrade_hostname_from_discovery(handle, "other-name"));
        let (config, _) = manager.get_state(handle).expect("client");
        assert_eq!(config.hostname.as_deref(), Some("workstation"));
    }

    #[test]
    fn find_by_ip_matches_inactive_and_active_clients() {
        let manager = ClientManager::default();
        let inactive = manager.add_with_config(config_client(Position::Left, false));
        assert_eq!(manager.find_by_ip(ip(10)), Some(inactive));
        assert_eq!(manager.find_by_ip(ip(99)), None);
    }

    #[test]
    fn working_address_is_remembered_per_client() {
        let manager = ClientManager::default();
        let a = manager.add_client();
        let b = manager.add_client();
        let v4 = SocketAddr::new(ip(10), 4242);
        let v6 = SocketAddr::new("fd00::10".parse().unwrap(), 4242);

        assert_eq!(manager.preferred_addr(a), None);
        manager.record_working_addr(a, v4);
        manager.record_working_addr(b, v6);
        assert_eq!(manager.preferred_addr(a), Some(v4));
        assert_eq!(manager.preferred_addr(b), Some(v6));

        // A later fallback success replaces the remembered address.
        manager.record_working_addr(a, v6);
        assert_eq!(manager.preferred_addr(a), Some(v6));

        manager.clear_preferred_addr(a);
        assert_eq!(manager.preferred_addr(a), None);
    }

    #[test]
    fn working_address_does_not_leak_into_a_reused_handle() {
        let manager = ClientManager::default();
        let removed = manager.add_with_config(config_client(Position::Left, true));
        manager.record_working_addr(removed, SocketAddr::new(ip(10), 4242));
        manager.remove_client(removed);

        // Slab may hand out the same handle again; the old session's address
        // must not become the new client's preferred one.
        let recreated = manager.add_with_config(config_client(Position::Right, true));
        assert_eq!(manager.preferred_addr(recreated), None);
    }

    #[test]
    fn record_working_addr_ignores_unknown_handles() {
        let manager = ClientManager::default();
        manager.record_working_addr(7, SocketAddr::new(ip(10), 4242));
        assert_eq!(manager.preferred_addr(7), None);
        assert!(manager.get_state(7).is_none());
    }
}
