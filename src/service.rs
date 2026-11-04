use crate::{
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    config::{Config, ConfigClient},
    connect::LanMouseConnection,
    crypto, discovery,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    listen::{LanMouseListener, ListenerCreationError},
};
use futures::StreamExt;
use lan_mouse_ipc::{
    AsyncFrontendListener, ClientHandle, ClipboardStatus, Diagnostics, DiscoveredDevice,
    FrontendEvent, FrontendRequest, IpcError, IpcListenerCreationError, Position, Status,
};
use lan_mouse_proto::{CAPABILITIES, PROTO_VERSION};
use local_channel::mpsc::{Receiver, Sender};
use log;
use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::{Arc, RwLock},
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify, task::spawn_local};
use tokio_util::sync::CancellationToken;

/// Background network discovery results.
#[derive(Debug)]
enum DiscoveryEvent {
    Scan(Vec<DiscoveredDevice>),
    Probe { ip: IpAddr, lan_mouse: bool },
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// clipboard synchronization
    clipboard: crate::clipboard::ClipboardTask,
    /// user-controlled clipboard toggle (shared with the clipboard task)
    clipboard_enabled: Rc<Cell<bool>>,
    /// clipboard size limits (shared with the clipboard task)
    clipboard_limits: Rc<Cell<input_clipboard::Limits>>,
    /// latest clipboard status reported by the task
    clipboard_status: ClipboardStatus,
    /// clipboard status updates from the task
    clipboard_status_rx: Receiver<ClipboardStatus>,
    /// clipboard peer disconnects from the emulation task
    clipboard_peer_events: local_channel::mpsc::Sender<std::net::SocketAddr>,
    /// whether the clipboard status channel is still open
    clipboard_status_open: bool,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// devices authorized with [`FrontendRequest::AuthorizeDevice`] whose
    /// reverse client entry must be created once their address is known
    pending_reverse: HashMap<String, (String, Position)>,
    /// results of network scans / probes, produced by background tasks
    discovery_rx: Receiver<DiscoveryEvent>,
    discovery_tx: Sender<DiscoveryEvent>,
    /// cancels the background discovery listener
    discovery_cancel: CancellationToken,
    /// addresses that may connect without an authorization prompt
    trusted_nets: Arc<RwLock<Vec<crate::trusted::TrustedNet>>>,
    /// raw allowlist entries as written by the user
    trusted_ips: Vec<String>,
    /// certificate used for explicit Lan Mouse probes
    cert: webrtc_dtls::crypto::Certificate,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    next_trigger_handle: u64,
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        let trusted_ips = config.trusted_ips();
        let trusted_nets = Arc::new(RwLock::new(crate::trusted::parse_all(&trusted_ips)));
        // listener + connection
        let listener = LanMouseListener::new(
            config.port(),
            cert.clone(),
            authorized_keys.clone(),
            trusted_nets.clone(),
        )
        .await?;
        let mut conn = LanMouseConnection::new(
            cert.clone(),
            client_manager.clone(),
            config.connect_grace_ms(),
        );
        let clipboard_sender = conn.sender();
        let clipboard_from_connection = conn.take_clipboard_rx();

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let capture = Capture::new(
            capture_backend,
            conn,
            config.release_bind(),
            config.motion_batch_ms(),
        );
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let (clipboard_from_listener_tx, clipboard_from_listener_rx) =
            local_channel::mpsc::channel();
        let emulation = Emulation::new(emulation_backend, listener, clipboard_from_listener_tx);
        let clipboard_enabled_initial = config.clipboard_enabled();
        let clipboard_enabled = Rc::new(Cell::new(clipboard_enabled_initial));
        let clipboard_limits = Rc::new(Cell::new(config.clipboard_limits()));
        let (clipboard_status_tx, clipboard_status_rx) = local_channel::mpsc::channel();
        let (clipboard_peer_tx, clipboard_peer_rx) = local_channel::mpsc::channel();
        let clipboard = crate::clipboard::ClipboardTask::new(
            client_manager.clone(),
            clipboard_sender,
            emulation.reply_handle(),
            clipboard_from_listener_rx,
            clipboard_from_connection.expect("clipboard receiver"),
            clipboard_peer_rx,
            clipboard_limits.clone(),
            clipboard_enabled.clone(),
            clipboard_status_tx,
        );

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let (discovery_tx, discovery_rx) = local_channel::mpsc::channel();
        let port = config.port();
        let service = Self {
            config,
            capture,
            clipboard,
            clipboard_enabled,
            clipboard_limits,
            clipboard_status: ClipboardStatus {
                enabled: clipboard_enabled_initial,
                ..Default::default()
            },
            clipboard_status_rx,
            clipboard_peer_events: clipboard_peer_tx,
            clipboard_status_open: true,
            emulation,
            frontend_listener,
            resolver,
            authorized_keys,
            pending_reverse: Default::default(),
            discovery_rx,
            discovery_tx,
            discovery_cancel: CancellationToken::new(),
            trusted_nets,
            trusted_ips,
            cert: cert.clone(),
            public_key_fingerprint,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            next_trigger_handle: 0,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }

        // Answer discovery beacons for the whole session and list what is
        // currently reachable, so the device table is populated when the
        // window opens.
        let discovery_port = discovery::DEFAULT_DISCOVERY_PORT;
        spawn_local(discovery::beacon_task(
            self.port,
            discovery_port,
            self.discovery_cancel.clone(),
        ));
        self.start_scan();

        loop {
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                event = self.discovery_rx.recv() => self.handle_discovery_event(event),
                status = self.clipboard_status_rx.recv(), if self.clipboard_status_open => {
                    match status {
                        Some(status) => self.handle_clipboard_status(status),
                        None => self.clipboard_status_open = false,
                    }
                },
                _ = self.config.changed() => self.handle_config_change(),
                r = signal::ctrl_c() => break r.expect("failed to wait for CTRL+C"),
            }
        }

        log::info!("terminating service ...");
        self.discovery_cancel.cancel();
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating clipboard ...");
        self.clipboard.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;

        Ok(())
    }

    fn handle_frontend_request(&mut self, request: Option<Result<FrontendRequest, IpcError>>) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        match request {
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::AuthorizeDevice {
                fingerprint,
                name,
                position,
            } => {
                self.add_authorized_key(name.clone(), fingerprint.clone());
                self.pending_reverse.insert(fingerprint, (name, position));
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook)
            }
            FrontendRequest::UpdateLeaveHook(handle, leave_hook) => {
                self.update_leave_hook(handle, leave_hook)
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
            FrontendRequest::SetClipboardEnabled(enabled) => self.set_clipboard_enabled(enabled),
            FrontendRequest::ScanNetwork => self.start_scan(),
            FrontendRequest::ProbeDevice { ip, port } => self.start_probe(ip, port),
            FrontendRequest::TrustAddress(entry) => {
                self.trust_address(entry);
                self.save_config();
            }
            FrontendRequest::UntrustAddress(entry) => {
                self.untrust_address(&entry);
                self.save_config();
            }
        }
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
                leave_hook: c.leave_cmd,
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);

        // Size limits can change at runtime. The enabled toggle is owned by
        // the frontend/CLI, so an external config edit only takes effect on
        // the next start.
        self.clipboard_limits.set(self.config.clipboard_limits());
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { addr, fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { addr, fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                let _ = self.clipboard_peer_events.send(addr);
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify => self.capture.release(),
            EmulationEvent::AutoAuthorized { addr, fingerprint } => {
                // The listener already trusted the certificate because the
                // address is allowlisted; persist it so the next connection
                // matches by fingerprint instead of by address.
                let keys = self.authorized_keys.read().expect("lock").clone();
                self.config.set_authorized_keys(keys);
                self.save_config();
                log::info!("auto-authorized {addr} ({fingerprint}) from a trusted address");
                let keys = self.authorized_keys.read().expect("lock").clone();
                self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::Connected { addr, fingerprint } => {
                // The peer that was just authorized also becomes a configured
                // client, so it can control this device too.
                if let Some((name, position)) = self.pending_reverse.remove(&fingerprint) {
                    self.add_reverse_client(name, addr.ip(), position);
                }
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerHello {
                addr,
                commit,
                proto_version,
                capabilities,
            } => {
                // Map the peer's source addr back to its client handle
                // and stamp the version. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.set_peer_hello(
                        handle,
                        Some(commit),
                        proto_version,
                        capabilities,
                    );
                    self.broadcast_client(handle);
                }
            }
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                self.spawn_hook_command(handle, HookKind::Enter);
            }
            ICaptureEvent::ClientLeft(handle) => {
                log::info!("leaving client {handle} ...");
                self.spawn_hook_command(handle, HookKind::Leave);
            }
        }
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn sync_frontend(&mut self) {
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        self.notify_frontend(FrontendEvent::TrustedAddresses(self.trusted_ips.clone()));
        self.notify_frontend(FrontendEvent::ClipboardStatus(
            self.clipboard_status.clone(),
        ));
        let diagnostics = self.diagnostics();
        self.notify_frontend(FrontendEvent::Diagnostics(diagnostics));
    }

    fn handle_clipboard_status(&mut self, status: ClipboardStatus) {
        // The service owns the toggle: a status produced before the task saw
        // a recent toggle must not flip it back.
        self.clipboard_status.backend = status.backend;
        self.clipboard_status.available = status.available;
        self.clipboard_status.applied = status.applied;
        self.clipboard_status.last_error = status.last_error;
        self.clipboard_status.enabled = self.clipboard_enabled.get();
        self.notify_frontend(FrontendEvent::ClipboardStatus(
            self.clipboard_status.clone(),
        ));
    }

    fn set_clipboard_enabled(&mut self, enabled: bool) {
        self.clipboard_enabled.set(enabled);
        self.clipboard_status.enabled = enabled;
        if !enabled {
            self.clipboard_status.last_error = None;
        }
        self.config.set_clipboard_enabled(enabled);
        self.notify_frontend(FrontendEvent::ClipboardStatus(
            self.clipboard_status.clone(),
        ));
        self.save_config();
    }

    /// Runtime information used by the frontend and `lan-mouse doctor`.
    pub(crate) fn diagnostics(&self) -> Diagnostics {
        Diagnostics {
            capture_backend: self.capture.backend().map(|b| b.to_string()),
            emulation_backend: self.emulation.backend().map(|b| b.to_string()),
            session_type: std::env::var("XDG_SESSION_TYPE").ok(),
            desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
            port: self.port,
            proto_version: PROTO_VERSION,
            capabilities: CAPABILITIES,
            clipboard_backend: self.clipboard_status.backend.clone(),
            clipboard_enabled: self.clipboard_status.enabled,
        }
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture.create(handle, pos, CaptureType::EnterOnly);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
            },
        );
    }

    fn update_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let incoming = self
            .incoming_conn_info
            .iter_mut()
            .find(|(_, i)| i.addr == addr)
            .map(|(_, i)| i)
            .expect("no such client");
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    /// Add an address (IP or CIDR) to the allowlist.
    fn trust_address(&mut self, entry: String) {
        let entry = entry.trim().to_string();
        if crate::trusted::TrustedNet::parse(&entry).is_none() {
            log::warn!("ignoring invalid trusted address `{entry}`");
            return;
        }
        if self.trusted_ips.iter().any(|e| e == &entry) {
            return;
        }
        self.trusted_ips.push(entry.clone());
        self.refresh_trusted_nets();
        self.config.set_trusted_ips(self.trusted_ips.clone());
        log::info!("trusted address added: {entry}");
        self.notify_frontend(FrontendEvent::TrustedAddresses(self.trusted_ips.clone()));
    }

    fn untrust_address(&mut self, entry: &str) {
        let before = self.trusted_ips.len();
        self.trusted_ips.retain(|e| e != entry);
        if self.trusted_ips.len() == before {
            return;
        }
        self.refresh_trusted_nets();
        self.config.set_trusted_ips(self.trusted_ips.clone());
        log::info!("trusted address removed: {entry}");
        self.notify_frontend(FrontendEvent::TrustedAddresses(self.trusted_ips.clone()));
    }

    fn refresh_trusted_nets(&self) {
        let nets = crate::trusted::parse_all(&self.trusted_ips);
        *self.trusted_nets.write().expect("lock") = nets;
    }

    /// Kick off a background scan of the local subnets.
    fn start_scan(&self) {
        let tx = self.discovery_tx.clone();
        spawn_local(async move {
            let devices = discovery::scan(discovery::DEFAULT_DISCOVERY_PORT).await;
            let _ = tx.send(DiscoveryEvent::Scan(devices));
        });
    }

    /// Explicitly test a host for a Lan Mouse listener.
    fn start_probe(&self, ip: IpAddr, port: u16) {
        let tx = self.discovery_tx.clone();
        let cert = self.cert.clone();
        spawn_local(async move {
            let lan_mouse = discovery::probe_lan_mouse(
                SocketAddr::new(ip, port),
                cert,
                std::time::Duration::from_secs(3),
            )
            .await;
            let _ = tx.send(DiscoveryEvent::Probe { ip, lan_mouse });
        });
    }

    fn handle_discovery_event(&mut self, event: Option<DiscoveryEvent>) {
        match event {
            Some(DiscoveryEvent::Scan(mut devices)) => {
                for device in &mut devices {
                    device.configured = self.client_with_ip(device.ip).is_some();
                }
                // Devices that run Lan Mouse and devices that are already
                // configured are the useful ones; keep them on top of the
                // (possibly long) list of plain hosts on the network.
                devices.sort_by_key(|device| {
                    let ip = match device.ip {
                        IpAddr::V4(ip) => u32::from(ip),
                        IpAddr::V6(ip) => ip.segments().iter().map(|s| *s as u32).sum(),
                    };
                    (!device.lan_mouse, !device.configured, ip)
                });
                log::debug!("network scan found {} device(s)", devices.len());
                self.notify_frontend(FrontendEvent::NetworkScan(devices));
            }
            Some(DiscoveryEvent::Probe { ip, lan_mouse }) => {
                self.notify_frontend(FrontendEvent::ProbeResult { ip, lan_mouse });
            }
            None => {}
        }
    }

    /// Create and activate a client entry for a peer that was just authorized.
    ///
    /// The peer address comes from the accepted connection, so the user never
    /// has to type it. An existing entry for the same address is reused
    /// instead of creating a duplicate.
    fn add_reverse_client(&mut self, name: String, ip: IpAddr, position: Position) {
        // An empty name means "use the address": the peer's hostname is not
        // known before the connection is authenticated.
        let name = if name.trim().is_empty() {
            ip.to_string()
        } else {
            name
        };
        if let Some(handle) = self.client_with_ip(ip) {
            log::info!("reverse client for {ip} already exists (id {handle})");
            self.update_pos(handle, position);
            self.set_client_active(handle, true);
            self.save_config();
            return;
        }

        let handle = self.client_manager.add_client();
        self.client_manager.set_hostname(handle, Some(name.clone()));
        self.client_manager.set_fix_ips(handle, vec![ip]);
        self.client_manager.set_pos(handle, position);
        log::info!("adding client {handle} for authorized device {name} ({ip}, {position})");
        let (config, state) = self.client_manager.get_state(handle).expect("client state");
        self.notify_frontend(FrontendEvent::Created(handle, config, state));
        self.activate_client(handle);
        self.save_config();
    }

    fn client_with_ip(&self, ip: IpAddr) -> Option<ClientHandle> {
        self.client_manager
            .get_client_states()
            .into_iter()
            .find(|(_, _, state)| state.ips.contains(&ip))
            .map(|(handle, _, _)| handle)
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            self.capture.create(handle, pos, CaptureType::Default);
            /* pre-establish the connection so the first crossing is instant */
            self.capture.connect(handle);
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    fn update_leave_hook(&mut self, handle: ClientHandle, leave_hook: Option<String>) {
        self.client_manager.set_leave_hook(handle, leave_hook);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    fn spawn_hook_command(&self, handle: ClientHandle, kind: HookKind) {
        let cmd = match kind {
            HookKind::Enter => self.client_manager.get_enter_cmd(handle),
            HookKind::Leave => self.client_manager.get_leave_cmd(handle),
        };
        let Some(cmd) = cmd else { return };
        tokio::task::spawn_local(async move {
            log::info!("spawning {kind} hook for client {handle}");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute {kind} hook for client {handle}: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{kind} hook for client {handle} ({cmd}) exited successfully");
                    } else {
                        log::warn!("{kind} hook for client {handle} ({cmd}) exited with {s}");
                    }
                }
                Err(e) => log::warn!("{kind} hook for client {handle} ({cmd}): {e}"),
            }
        });
    }
}

#[derive(Clone, Copy, Debug)]
enum HookKind {
    Enter,
    Leave,
}

impl std::fmt::Display for HookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookKind::Enter => f.write_str("enter"),
            HookKind::Leave => f.write_str("leave"),
        }
    }
}
