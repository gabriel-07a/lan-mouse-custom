use std::{
    cell::RefCell,
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    rc::Rc,
};

use slab::Slab;

use lan_mouse_ipc::{ClientConfig, ClientHandle, ClientState, Position};

use crate::config::ConfigClient;

#[derive(Clone, Default)]
pub struct ClientManager {
    clients: Rc<RefCell<Slab<(ClientConfig, ClientState)>>>,
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
        let state = ClientState {
            active: config_client.active,
            ips: HashSet::from_iter(config.fix_ips.iter().cloned()),
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
            s.dns_ips = dns_ips
        }
        self.update_ips(handle);
    }

    fn update_ips(&self, handle: ClientHandle) {
        if let Some((c, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.ips = c
                .fix_ips
                .iter()
                .cloned()
                .chain(s.dns_ips.iter().cloned())
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
        let smoothed = match (state.rtt_ms, rtt_ms) {
            // Exponential moving average: 3/4 old, 1/4 new. Keeps the UI
            // from flickering on a single outlier while still tracking
            // real changes within a few ping rounds.
            (Some(old), Some(new)) => Some(((old as u64 * 3 + new as u64) / 4) as u32),
            (None, new) => new,
            (old, None) => old,
        };
        state.rtt_ms = smoothed;
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
        assert!(manager.deactivate_client(handle));
        let (_, state) = manager.get_state(handle).expect("client");
        assert!(!state.active);
        assert_eq!(state.active_addr, None);
        assert!(!state.alive);
        assert_eq!(state.rtt_ms, None);
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
}
