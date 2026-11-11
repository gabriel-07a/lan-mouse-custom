use crate::{
    audit::{AuditLog, AuditRecord},
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    config::{Config, ConfigClient},
    connect::LanMouseConnection,
    crypto,
    devices::{Device, DeviceSource, DeviceStore, PolicyBundle},
    discovery,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    listen::{LanMouseListener, ListenerCreationError, PairingWindow},
    service_core::{
        BidirectionalAction, bidirectional_action, duplicate_reverse_clients, return_targets,
    },
};
use futures::StreamExt;
use lan_mouse_ipc::{
    AddressRuleView, AsyncFrontendListener, Capabilities, ClientHandle, ClipboardStatus,
    DeviceView, Diagnostics, DirectionPolicy, DiscoveredDevice, FrontendEvent, FrontendRequest,
    HistoryEntry, IpcError, IpcListenerCreationError, PendingView, Position, Status,
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
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify, task::spawn_local};
use tokio_util::sync::CancellationToken;

/// Background network discovery results.
#[derive(Debug)]
enum DiscoveryEvent {
    Scan(Vec<DiscoveredDevice>),
    Probe {
        ip: IpAddr,
        lan_mouse: bool,
    },
    /// the pairing window closed
    Pairing(bool),
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
    /// append-only audit log for grants, revokes, pairing and sessions
    audit: AuditLog,
    /// known devices, their policies and pending requests (shared with the listener)
    devices: Arc<RwLock<DeviceStore>>,
    /// authorized public key sha256 fingerprints (derived from `devices`)
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// generation counter for the pairing window; stale timers compare it so
    /// opening the window twice cannot close the new window early
    pairing_generation: Rc<Cell<u64>>,
    /// fingerprints learned from outgoing connections (TOFU), so a change can
    /// be persisted without writing the config on every state update
    learned_fingerprints: HashMap<ClientHandle, String>,
    /// clients the service added on its own for an incoming device. They are
    /// merged into the user-configured client once its certificate fingerprint
    /// is learned, so one logical peer never shows up as two devices.
    auto_reverse_clients: HashSet<ClientHandle>,
    /// devices authorized with [`FrontendRequest::AuthorizeDevice`] whose
    /// reverse client entry must be created once their address is known
    pending_reverse: HashMap<String, (String, Position)>,
    /// results of network scans / probes, produced by background tasks
    discovery_rx: Receiver<DiscoveryEvent>,
    discovery_tx: Sender<DiscoveryEvent>,
    /// devices announced by peers between scans
    announced_rx: Receiver<discovery::Announced>,
    announced_tx: Sender<discovery::Announced>,
    /// last known devices, keyed by address
    discovered: HashMap<IpAddr, DiscoveredDevice>,
    /// cancels the background discovery listener
    discovery_cancel: CancellationToken,
    /// address rules that may connect without a prompt, carrying permissions;
    /// shared with the listener so policy changes apply to new handshakes
    trusted_nets: Arc<RwLock<Vec<crate::trusted::AddressRule>>>,
    /// whether incoming connections are accepted (kiosk mode shares this with
    /// the listener)
    accept_incoming: Arc<RwLock<bool>>,
    /// open pairing window: the first device accepted inside it gets its
    /// configured permissions
    pairing_until: Arc<RwLock<Option<PairingWindow>>>,
    /// fingerprints adopted during the current/previous pairing window, for
    /// the post-pairing review
    pairing_adopted: Vec<String>,
    /// certificate used for explicit Lan Mouse probes
    cert: webrtc_dtls::crypto::Certificate,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// notifications when an outgoing connection changes live state (addr, alive, rtt, hello)
    conn_state_rx: Receiver<ClientHandle>,
    /// current port
    port: u16,
    /// UDP discovery/announcement port
    discovery_port: u16,
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
    /// Return hand-offs whose `Leave` has not been confirmed by the peer yet.
    return_requests: ReturnHandoff,
    /// Monotonic serial for return hand-off requests (non-zero).
    return_serial: u32,
    /// Last permission bits announced by each peer, so identical AuthRequests
    /// do not produce an audit record (or a state broadcast) every ping round.
    peer_permissions_seen: HashMap<String, (u8, u8)>,
    /// A frontend asked the service to shut down (CLI/installer upgrade path).
    /// The run loop breaks and the regular teardown runs once.
    shutdown_requested: bool,
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

        let mut device_store = DeviceStore::default();
        device_store.seed(config.devices(), config.denied_devices());
        let devices = Arc::new(RwLock::new(device_store));
        let authorized_keys = Arc::new(RwLock::new(Self::authorized_map(&devices)));
        let trusted_nets = Arc::new(RwLock::new(config.address_rules()));
        let pairing_until: Arc<RwLock<Option<PairingWindow>>> = Default::default();
        let accept_incoming = Arc::new(RwLock::new(config.accept_incoming()));
        // listener + connection
        let listener = LanMouseListener::new(
            config.port(),
            cert.clone(),
            authorized_keys.clone(),
            trusted_nets.clone(),
            pairing_until.clone(),
            devices.clone(),
            accept_incoming.clone(),
        )
        .await?;
        let mut conn = LanMouseConnection::new(
            cert.clone(),
            client_manager.clone(),
            config.connect_grace_ms(),
        );
        conn.set_device_store(devices.clone());
        let clipboard_sender = conn.sender();
        let clipboard_from_connection = conn.take_clipboard_rx();
        let conn_state_rx = conn.take_state_rx().expect("connection state receiver");

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let capture = Capture::new(
            capture_backend,
            conn,
            config.release_bind(),
            config.motion_batch_ms(),
            config.connect_grace_ms(),
        );
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let (clipboard_from_listener_tx, clipboard_from_listener_rx) =
            local_channel::mpsc::channel();
        let emulation = Emulation::new(
            emulation_backend,
            listener,
            clipboard_from_listener_tx,
            Some(devices.clone()),
        );
        emulation.set_absolute_sensitivity(config.absolute_pointer_sensitivity());
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
            devices.clone(),
        );

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let (discovery_tx, discovery_rx) = local_channel::mpsc::channel();
        let (announced_tx, announced_rx) = local_channel::mpsc::channel();
        let port = config.port();
        let discovery_port = config.discovery_port();
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
            audit: AuditLog::new(AuditLog::default_path()),
            devices,
            authorized_keys,
            pairing_generation: Default::default(),
            learned_fingerprints: Default::default(),
            auto_reverse_clients: Default::default(),
            pending_reverse: Default::default(),
            discovery_rx,
            discovery_tx,
            announced_rx,
            announced_tx,
            discovered: Default::default(),
            discovery_cancel: CancellationToken::new(),
            trusted_nets,
            accept_incoming,
            pairing_until,
            pairing_adopted: Default::default(),
            cert: cert.clone(),
            public_key_fingerprint,
            client_manager,
            conn_state_rx,
            frontend_event_pending: Default::default(),
            port,
            discovery_port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            next_trigger_handle: 0,
            return_requests: Default::default(),
            return_serial: 0,
            peer_permissions_seen: Default::default(),
            shutdown_requested: false,
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

        // Answer discovery beacons for the whole session. Active subnet
        // scanning is user-triggered (`ScanNetwork`) so a large/VPN-heavy
        // network does not get probed every time the service starts.
        let discovery_port = self.discovery_port;
        spawn_local(discovery::beacon_task(
            self.port,
            discovery_port,
            self.discovery_cancel.clone(),
            self.public_key_fingerprint.clone(),
            self.announced_tx.clone(),
        ));

        // Retry unanswered return hand-offs: DTLS application data is not
        // retransmitted, so a single lost `Leave` must not be able to latch the
        // remote-control state forever.
        let mut return_retry = tokio::time::interval(RETURN_RETRY_INTERVAL);
        return_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        return_retry.tick().await;
        // systemd `stop`, desktop logout and the Windows service wrapper use
        // SIGTERM; without this the process died without releasing keys or
        // tearing the backends down.
        #[cfg(unix)]
        let mut sigterm = {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(signal) => Some(signal),
                Err(e) => {
                    log::warn!("could not install the SIGTERM handler: {e}");
                    None
                }
            }
        };

        loop {
            tokio::select! {
                request = self.frontend_listener.next() => {
                    self.handle_frontend_request(request);
                    if self.shutdown_requested {
                        break;
                    }
                }
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                event = self.discovery_rx.recv() => self.handle_discovery_event(event),
                event = self.announced_rx.recv() => {
                    if let Some(mut announced) = event {
                        self.apply_discovered_hostname(&announced.device);
                        self.annotate_discovered(&mut announced.device);
                        self.discovered.insert(announced.ip, announced.device);
                        self.broadcast_network();
                    }
                }
                handle = self.conn_state_rx.recv() => {
                    if let Some(handle) = handle {
                        self.broadcast_client(handle);
                        // A peer's AuthRevoke deactivates the client in the
                        // connection layer only. Drop the matching outgoing
                        // capture barrier here too, otherwise the local pointer
                        // keeps retrying a link that the user already closed
                        // (release/re-grab loop at the screen edge).
                        if !self.client_manager.is_active(handle) {
                            self.capture.destroy(handle);
                        }
                        // A successful outgoing connection with an unpinned
                        // client learns the peer certificate (TOFU) and pins
                        // it, so later connections cannot be impersonated.
                        if let Some(fingerprint) = self.client_manager.fingerprint(handle) {
                            if self.learned_fingerprints.get(&handle) != Some(&fingerprint) {
                                let name = self
                                    .client_manager
                                    .get_state(handle)
                                    .and_then(|(config, _)| config.hostname)
                                    .unwrap_or_else(|| fingerprint.clone());
                                // The user chose to connect to this peer, so it
                                // is a trusted identity. Register it with the
                                // bidirectional defaults: otherwise the peer's
                                // reverse connection would raise a second
                                // permission prompt on this machine ("accept
                                // once" should mean both directions).
                                self.audit.event(
                                    "cert_learned",
                                    Some(&fingerprint),
                                    Some(&name),
                                    None,
                                    Some("source=tofu"),
                                );
                                self.ensure_device(
                                    &fingerprint,
                                    name,
                                    DeviceSource::Legacy,
                                    DirectionPolicy::allow_all(),
                                    DirectionPolicy::allow_all(),
                                );
                                self.learned_fingerprints.insert(handle, fingerprint.clone());
                                // The incoming session may have been adopted by
                                // an auto-added reverse client before this
                                // certificate was known; merge it now that the
                                // configured client and the peer are linked.
                                self.merge_auto_reverse_client(handle, &fingerprint);
                                self.refresh_maps();
                                self.save_config();
                                self.broadcast_devices();
                            }
                            self.apply_outgoing_policy(&fingerprint);
                        }
                    }
                }
                status = self.clipboard_status_rx.recv(), if self.clipboard_status_open => {
                    match status {
                        Some(status) => self.handle_clipboard_status(status),
                        None => self.clipboard_status_open = false,
                    }
                },
                _ = self.config.changed() => self.handle_config_change(),
                _ = return_retry.tick() => self.retry_return_requests(),
                _ = async {
                    // SIGTERM (systemd stop / graphical session logout). On
                    // platforms without it this branch never completes.
                    #[cfg(unix)]
                    wait_for_sigterm(&mut sigterm).await;
                    #[cfg(not(unix))]
                    std::future::pending::<()>().await;
                } => break,
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
            FrontendRequest::Hello { ipc_version } => {
                if ipc_version != lan_mouse_ipc::IPC_VERSION {
                    log::warn!(
                        "frontend speaks IPC v{ipc_version}, service speaks v{}",
                        lan_mouse_ipc::IPC_VERSION
                    );
                }
                self.notify_frontend(FrontendEvent::Hello {
                    ipc_version: lan_mouse_ipc::IPC_VERSION,
                });
            }
            FrontendRequest::ListDevices => self.broadcast_devices(),
            FrontendRequest::ListPending => self.broadcast_pending(),
            FrontendRequest::SetDevicePolicy {
                fingerprint,
                incoming,
                outgoing,
            } => self.set_device_policy(fingerprint, incoming, outgoing),
            FrontendRequest::RenameDevice { fingerprint, name } => {
                self.rename_device(fingerprint, name)
            }
            FrontendRequest::RevokeDevice {
                fingerprint,
                disconnect,
            } => self.revoke_device(fingerprint, disconnect),
            FrontendRequest::RespondPending {
                fingerprint,
                accept,
                name,
                position,
                incoming,
                outgoing,
            } => self.respond_pending(fingerprint, accept, name, position, incoming, outgoing),
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
            } => match crypto::normalize_fingerprint(&fingerprint) {
                Some(fingerprint) => {
                    self.add_authorized_key(name.clone(), fingerprint.clone());
                    self.pending_reverse.insert(fingerprint, (name, position));
                    self.save_config();
                }
                None => self.notify_frontend(FrontendEvent::Error(format!(
                    "invalid fingerprint: {fingerprint}"
                ))),
            },
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
            FrontendRequest::UpdateSensitivity(handle, sensitivity) => {
                self.update_sensitivity(handle, sensitivity);
                self.save_config();
            }
            FrontendRequest::SetAbsoluteSensitivity(percent) => {
                self.set_absolute_sensitivity(percent);
                self.save_config();
            }
            FrontendRequest::UpdateFingerprint(handle, fingerprint) => {
                let fingerprint = fingerprint.and_then(|fp| {
                    let normalized = crypto::normalize_fingerprint(&fp);
                    if normalized.is_none() {
                        self.notify_frontend(FrontendEvent::Error(format!(
                            "invalid fingerprint: {fp}"
                        )));
                    }
                    normalized
                });
                if self.client_manager.set_fingerprint(handle, fingerprint) {
                    self.broadcast_client(handle);
                    self.save_config();
                }
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook);
                self.save_config();
            }
            FrontendRequest::UpdateLeaveHook(handle, leave_hook) => {
                self.update_leave_hook(handle, leave_hook);
                self.save_config();
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
            FrontendRequest::SetClipboardEnabled(enabled) => self.set_clipboard_enabled(enabled),
            FrontendRequest::ScanNetwork => self.start_scan(),
            FrontendRequest::ProbeDevice { ip, port } => self.start_probe(ip, port),
            FrontendRequest::TrustAddress(entry) => {
                self.trust_address(entry);
                self.save_config();
            }
            FrontendRequest::AddAddressRule {
                address,
                incoming,
                outgoing,
            } => {
                self.add_address_rule(address, incoming, outgoing);
                self.save_config();
            }
            FrontendRequest::ListAddressRules => self.broadcast_address_rules(),
            FrontendRequest::ListHistory { limit } => self.broadcast_history(limit),
            FrontendRequest::ExportPolicy => self.export_policy(),
            FrontendRequest::ImportPolicy { json } => self.import_policy(json),
            FrontendRequest::DenyDevice {
                fingerprint,
                expires_in,
            } => self.deny_device(fingerprint, expires_in),
            FrontendRequest::GrantSession {
                fingerprint,
                name,
                position,
                incoming,
            } => self.grant_session(fingerprint, name, position, incoming),
            FrontendRequest::UntrustAddress(entry) => {
                self.untrust_address(&entry);
                self.save_config();
            }
            FrontendRequest::StartPairing(seconds) => self.start_pairing(
                seconds,
                DirectionPolicy::allow_all(),
                DirectionPolicy::allow_all(),
            ),
            FrontendRequest::StartPairingScoped {
                seconds,
                incoming,
                outgoing,
            } => self.start_pairing(seconds, incoming, outgoing),
            // Graceful shutdown requested over IPC (installer upgrade /
            // `lan-mouse cli shutdown`). The flag is consumed by the run loop,
            // so this is idempotent with SIGINT/SIGTERM: whichever arrives
            // first breaks the loop and the teardown below runs exactly once.
            FrontendRequest::Shutdown => {
                log::info!("shutdown requested over the frontend IPC");
                self.shutdown_requested = true;
            }
        }
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        // Never persist the runtime active state: only an explicit
        // activate_on_startup flag from the config file may reconnect on the
        // next start. Otherwise quitting/crashing while connected made the app
        // reconnect to the peer by itself - which must never happen.
        let startup_flags: Vec<bool> = clients
            .iter()
            .map(|(c, _)| {
                let ips: HashSet<_> = c.fix_ips.iter().copied().collect();
                self.config.activate_on_startup_for(
                    c.fingerprint.as_deref(),
                    c.hostname.as_deref(),
                    &ips,
                )
            })
            .collect();
        let clients = clients
            .into_iter()
            .zip(startup_flags)
            .map(|((c, _s), startup)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: startup,
                enter_hook: c.cmd,
                leave_hook: c.leave_cmd,
                sensitivity: c.sensitivity,
                fingerprint: c.fingerprint,
            })
            .collect();
        self.config.set_clients(clients);
        let rules = self.trusted_nets.read().expect("lock").clone();
        self.config.set_address_rules(rules);
        let devices = self
            .devices
            .read()
            .expect("lock")
            .persistent_devices()
            .cloned()
            .collect::<Vec<_>>();
        self.config.set_devices(devices);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        self.config
            .set_denied_devices(self.devices.read().expect("lock").denied());
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
        // Rebuild the device store from disk. Policies edited by hand in the
        // config file take effect immediately.
        let mut store = DeviceStore::default();
        store.seed(self.config.devices(), self.config.denied_devices());
        *self.devices.write().expect("lock") = store;
        *self.trusted_nets.write().expect("lock") = self.config.address_rules();
        *self.accept_incoming.write().expect("lock") = self.config.accept_incoming();
        self.refresh_maps();
        for handle in self.client_manager.active_clients() {
            if let Some(fingerprint) = self.client_manager.fingerprint(handle) {
                self.apply_outgoing_policy(&fingerprint);
            }
        }

        // Size limits can change at runtime. The enabled toggle is owned by
        // the frontend/CLI, so an external config edit only takes effect on
        // the next start.
        self.clipboard_limits.set(self.config.clipboard_limits());
        // The sensitivity can be edited in config.toml while the service runs.
        self.emulation
            .set_absolute_sensitivity(self.config.absolute_pointer_sensitivity());
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
                // A revoked device must not keep producing permission dialogs
                // (and pending entries) just because it keeps retrying. The
                // listener already rejects it; drop the event here as well so
                // the frontend is not asked again.
                if self.devices.read().expect("lock").is_denied(&fingerprint) {
                    log::debug!("ignoring connection request from denied device {fingerprint}");
                    return;
                }
                // Track the request so it can be reviewed later (also from the
                // CLI or after a GUI restart) instead of showing a one-shot
                // dialog.
                let name = self.peer_display_name(addr.ip(), &fingerprint);
                let sas = crypto::sas_code(&self.public_key_fingerprint, &fingerprint);

                // A certificate change looks like a new device at an address
                // that used to have another certificate, or a new fingerprint
                // reusing the display name of a known device.
                let previous_fingerprint = self
                    .client_with_ip(addr.ip())
                    .and_then(|handle| self.client_manager.fingerprint(handle))
                    .filter(|previous| previous != &fingerprint)
                    .or_else(|| {
                        let trimmed = name.trim();
                        if trimmed.is_empty() || trimmed.parse::<IpAddr>().is_ok() {
                            return None;
                        }
                        self.devices
                            .read()
                            .expect("lock")
                            .devices()
                            .find(|device| {
                                device.fingerprint != fingerprint
                                    && device.name.eq_ignore_ascii_case(trimmed)
                            })
                            .map(|device| device.fingerprint.clone())
                    });
                if let Some(previous) = &previous_fingerprint {
                    self.audit.event(
                        "cert_changed",
                        Some(&fingerprint),
                        Some(&name),
                        Some(&addr.to_string()),
                        Some(&format!("previous={previous}")),
                    );
                    self.notify_frontend(FrontendEvent::CertificateChanged {
                        old_fingerprint: previous.clone(),
                        new_fingerprint: fingerprint.clone(),
                        name: name.clone(),
                        addr,
                        sas: sas.clone(),
                    });
                }

                let is_new = self.devices.write().expect("lock").pending_upsert(
                    &fingerprint,
                    addr,
                    Some(name.clone()),
                    previous_fingerprint.clone(),
                );
                if is_new {
                    log::info!("connection request from {addr} ({fingerprint})");
                }
                // Only open the approval dialog for a genuinely new request (or
                // when the identity of an existing request changed). Re-emitting
                // ConnectionAttempt for every 2 s handshake retry made the modal
                // reopen continuously and looked like "it keeps asking".
                if is_new || previous_fingerprint.is_some() {
                    self.notify_frontend(FrontendEvent::ConnectionAttempt {
                        addr,
                        fingerprint: fingerprint.clone(),
                        sas,
                        previous_fingerprint,
                    });
                }
                self.broadcast_pending();
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                // A peer is about to drive this machine: make sure the
                // (lazily created) emulation backend exists so input can be
                // injected and the capability is advertised back to the peer.
                // The absolute-pointer tracker is armed by the listener before
                // the `Ack` (and before any queued input), with the exact
                // crossing coordinate; arming it here again would warp the
                // cursor after the first motion had already been injected.
                self.emulation.warm_up();
                // The peer is driving this machine: local barrier detection
                // must not react to the injected pointer motion (ping-pong at
                // the shared border). Knowing *which* edge the peer entered
                // through also lets the capture side still detect the return
                // hand-off at exactly that edge.
                self.capture.set_remote_peer(Some(pos));
                // Resolve the client entry before the fingerprint is moved
                // into the session registry below.
                let handle = self.client_for_peer(addr, &fingerprint);
                // Defense in depth: one identity must never keep two capture
                // zones. A transport switch (IPv4 <-> IPv6, reconnect) ends the
                // old session before the new one is registered.
                if let Some(stale) = self.incoming_addr_for_fingerprint(&fingerprint) {
                    if stale != addr {
                        if let Some(stale) = self.remove_incoming(stale) {
                            self.notify_frontend(FrontendEvent::IncomingDisconnected(stale));
                        }
                    }
                }
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
                if let Some(handle) = handle {
                    self.attach_incoming(handle, addr);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                self.return_requests.forget(addr);
                self.expire_session_grants(addr);
                let _ = self.clipboard_peer_events.send(addr);
                if let Some(handle) = self.client_for_incoming(addr) {
                    // The connection closed on the peer side. Because a
                    // connection is bidirectional, keep it fully closed: stop
                    // the outgoing link too instead of letting it redial the
                    // peer that just hung up.
                    if self.client_manager.is_active(handle) {
                        log::info!(
                            "incoming session from {addr} ended: deactivating the outgoing link of client {handle}"
                        );
                        self.deactivate_client(handle);
                    }
                    if self.client_manager.incoming_addr(handle).is_some() {
                        self.client_manager.set_incoming_addr(handle, None);
                        self.broadcast_client(handle);
                    }
                }
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
                if self.incoming_conns.is_empty() {
                    self.capture.set_remote_peer(None);
                    self.emulation.remote_leave();
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    // Persist the runtime port change: peers dial the port
                    // they were configured with, so after a restart the
                    // service must come back on the same port (P2-09).
                    self.config.set_port(port);
                    self.save_config();
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::AbsoluteSensitivity(
                    self.config.absolute_pointer_sensitivity(),
                ));
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify => {
                // Release the local capture, but keep the remote-peer
                // suppression latched: it is cleared by the return hand-off
                // (PeerLeft) or by the session ending. Clearing it here opened
                // a window where injected motion was accepted as local input,
                // which is what made both machines enter/leave/enter at the
                // shared edge.
                self.capture.release();
            }
            EmulationEvent::EdgeReturn(pos) => {
                log::debug!("handoff: return requested through {pos:?}");
                self.request_return(pos);
            }
            EmulationEvent::PeerLeft { addr, fingerprint } => {
                // The peer confirmed it released its capture: the retried
                // return hand-off is complete.
                log::debug!("handoff: peer {addr} released, local control returned");
                self.return_requests.confirmed(addr);
                self.emulation.remote_leave();
                self.peer_released(addr, &fingerprint);
            }
            EmulationEvent::AutoAuthorized {
                addr,
                fingerprint,
                from_pairing,
                incoming,
                outgoing,
            } => {
                self.emulation.warm_up();
                // The listener trusted the certificate (allowlisted address
                // or open pairing window). It only inserted the fingerprint
                // after the handshake succeeded; persist it here so the next
                // connection matches by certificate instead of by address.
                if from_pairing {
                    // Track the adopted device for the post-pairing review,
                    // then close the single-use window.
                    self.pairing_adopted.push(fingerprint.clone());
                    self.end_pairing();
                }
                let name = self.peer_display_name(addr.ip(), &fingerprint);
                let source = if from_pairing {
                    DeviceSource::Pairing
                } else {
                    DeviceSource::AddressRule
                };
                self.ensure_device(&fingerprint, name.clone(), source, incoming, outgoing);
                self.refresh_maps();
                self.audit.event(
                    "grant_created",
                    Some(&fingerprint),
                    Some(&name),
                    Some(&addr.to_string()),
                    Some(if from_pairing {
                        "source=pairing"
                    } else {
                        "source=address_rule"
                    }),
                );
                log::info!("auto-authorized {addr} ({fingerprint})");
                if let Some(handle) = self.client_with_fingerprint(&fingerprint) {
                    // One logical peer = one client entry. An incoming
                    // connection must reconnect to the configured client
                    // instead of becoming a second "reverse" device.
                    self.attach_incoming(handle, addr);
                } else if self
                    .devices
                    .read()
                    .expect("lock")
                    .outgoing(&fingerprint)
                    .grants_anything()
                {
                    self.add_reverse_client(
                        name,
                        addr.ip(),
                        Position::Left,
                        Some(fingerprint.clone()),
                    );
                    if let Some(handle) = self.client_with_fingerprint(&fingerprint) {
                        self.attach_incoming(handle, addr);
                    }
                }
                self.save_config();
                self.broadcast_devices();
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::Connected { addr, fingerprint } => {
                // Pre-warm for a peer that may drive this machine next.
                self.emulation.warm_up();
                // The peer that was just authorized also becomes a configured
                // client, so it can control this device too.
                let name = self.peer_display_name(addr.ip(), &fingerprint);
                self.ensure_device(
                    &fingerprint,
                    name,
                    DeviceSource::Manual,
                    DirectionPolicy::allow_all(),
                    DirectionPolicy::allow_all(),
                );
                self.refresh_maps();
                let outgoing = self.devices.read().expect("lock").outgoing(&fingerprint);
                if let Some((name, _position)) = self.pending_reverse.remove(&fingerprint) {
                    // The user picked a position while authorizing the device.
                    // When the peer is already a configured client the existing
                    // entry (and its position) is kept: changing it here was
                    // what duplicated/deactivated the configured row.
                    if let Some(handle) = self.client_with_fingerprint(&fingerprint) {
                        let resolved_name = if name.trim().is_empty() {
                            self.peer_display_name(addr.ip(), &fingerprint)
                        } else {
                            name
                        };
                        let _ = self
                            .client_manager
                            .upgrade_hostname_from_discovery(handle, &resolved_name);
                        self.attach_incoming(handle, addr);
                    } else if outgoing.grants_anything() {
                        let resolved_name = if name.trim().is_empty() {
                            self.peer_display_name(addr.ip(), &fingerprint)
                        } else {
                            name
                        };
                        self.add_reverse_client(
                            resolved_name,
                            addr.ip(),
                            _position,
                            Some(fingerprint.clone()),
                        );
                        if let Some(handle) = self.client_with_fingerprint(&fingerprint) {
                            self.attach_incoming(handle, addr);
                        }
                    }
                } else if let Some(handle) = self.client_with_fingerprint(&fingerprint) {
                    self.attach_incoming(handle, addr);
                } else if outgoing.grants_anything() {
                    let name = self.peer_display_name(addr.ip(), &fingerprint);
                    self.add_reverse_client(
                        name,
                        addr.ip(),
                        Position::Left,
                        Some(fingerprint.clone()),
                    );
                    if let Some(handle) = self.client_with_fingerprint(&fingerprint) {
                        self.attach_incoming(handle, addr);
                    }
                }
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerPermissions {
                addr,
                fingerprint,
                incoming_caps,
                outgoing_caps,
            } => {
                // AuthRequest is repeated by every peer ping round; only a
                // real change of the announced bits is worth an audit record
                // (and a blocking file open) or a full client state broadcast.
                // The user's live audit log was 4148/4189 identical
                // peer_permissions records produced this way.
                let caps = (incoming_caps, outgoing_caps);
                if self.peer_permissions_seen.get(&fingerprint) == Some(&caps) {
                    return;
                }
                self.peer_permissions_seen.insert(fingerprint.clone(), caps);
                // Belt and braces: the source-level dedupe above already limits
                // this to real changes, and the audit helper suppresses the same
                // key inside a short window even if two changes race in.
                self.audit.event_state_change(
                    &format!("peer_permissions:{fingerprint}:{incoming_caps:x}:{outgoing_caps:x}"),
                    Duration::from_secs(10),
                    "peer_permissions",
                    Some(&fingerprint),
                    None,
                    Some(&addr.to_string()),
                    Some(&format!(
                        "peer_incoming={incoming_caps:#x} peer_outgoing={outgoing_caps:#x}"
                    )),
                );
                if let Some(handle) = self.client_for_peer(addr, &fingerprint) {
                    self.client_manager.set_peer_permissions(
                        handle,
                        Some(incoming_caps),
                        Some(outgoing_caps),
                    );
                    self.broadcast_client(handle);
                }
            }
            EmulationEvent::PeerAuthRevoked { addr, reason } => {
                self.audit.event(
                    "peer_auth_revoked",
                    None,
                    None,
                    Some(&addr.to_string()),
                    Some(&format!("reason={reason}")),
                );
                let fingerprint = self
                    .incoming_conn_info
                    .values()
                    .find(|incoming| incoming.addr == addr)
                    .map(|incoming| incoming.fingerprint.clone());
                if let Some(handle) = self.client_for_incoming(addr) {
                    self.client_manager
                        .set_peer_permissions(handle, Some(0), Some(0));
                    self.broadcast_client(handle);
                }
                if let Some(fingerprint) = fingerprint {
                    log::info!(
                        "peer at {addr} explicitly disconnected; deactivating client(s) for {fingerprint}"
                    );
                    self.deactivate_fingerprint(&fingerprint);
                }
            }
            EmulationEvent::PeerHello {
                addr,
                commit,
                proto_version,
                capabilities,
            } => {
                // Map the peer's source addr back to its client handle
                // and stamp the version.
                if let Some(handle) = self.client_for_incoming(addr) {
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
                if let Some(addr) = self
                    .incoming_conn_info
                    .get(&handle)
                    .map(|incoming| incoming.addr)
                {
                    self.request_handoff(addr);
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
        self.notify_frontend(FrontendEvent::TrustedAddresses(
            self.trusted_address_strings(),
        ));
        self.notify_frontend(FrontendEvent::AddressRules(self.address_rule_views()));
        self.notify_frontend(FrontendEvent::ClipboardStatus(
            self.clipboard_status.clone(),
        ));
        let diagnostics = self.diagnostics();
        self.notify_frontend(FrontendEvent::Diagnostics(diagnostics));
        let pairing_active = self
            .pairing_until
            .read()
            .expect("lock")
            .as_ref()
            .is_some_and(PairingWindow::is_open);
        self.notify_frontend(FrontendEvent::PairingActive(pairing_active));
        self.broadcast_devices();
        self.broadcast_pending();
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
            last_connection_error: self
                .client_manager
                .get_client_states()
                .iter()
                .find_map(|(_, _, state)| state.last_error.clone()),
        }
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    /// Ask the peer that is driving us to release its capture because the
    /// remote pointer was pushed past `pos` (the edge it originally entered
    /// through). This is the return hand-off: without it the peer keeps the
    /// local pointer grabbed on its side of the shared edge.
    fn request_return(&mut self, pos: Position) {
        let incoming: Vec<(SocketAddr, Position)> = self
            .incoming_conn_info
            .values()
            .map(|incoming| (incoming.addr, incoming.pos))
            .collect();
        let targets = return_targets(&incoming, pos);
        if let Some(addr) = targets.fallback {
            log::warn!(
                "return hand-off through {pos:?} did not match the live edge; using the only incoming session {addr}"
            );
            log::info!("handing control back: remote pointer left through {pos:?} ({addr})");
            self.request_handoff(addr);
            return;
        }
        if targets.matching.is_empty() {
            log::debug!("return hand-off through {pos:?}, but no incoming session matches it");
            return;
        }
        for addr in targets.matching {
            log::info!("handing control back: remote pointer left through {pos:?} ({addr})");
            self.request_handoff(addr);
        }
    }

    /// Ask the peer at addr to release its capture, retrying until it
    /// confirms with Leave (see ReturnHandoff).
    fn request_handoff(&mut self, addr: SocketAddr) {
        let serial = self.next_return_serial();
        self.return_requests.request(addr, serial, Instant::now());
        // DTLS application data is not retransmitted, so a single lost request
        // stalls the hand-off until the retry timer. Send the first request
        // twice: the peer answers both copies with the same `Leave(serial)`
        // confirmation (the serial makes the request idempotent), so an
        // isolated packet loss costs nothing visible.
        self.emulation.send_leave_request(addr, serial);
        self.emulation.send_leave_request(addr, serial);
    }

    fn next_return_serial(&mut self) -> u32 {
        // 0 is reserved for "plain release confirmation" (what peers have always
        // sent); a non-zero serial marks a retried request.
        self.return_serial = self.return_serial.wrapping_add(1);
        if self.return_serial == 0 {
            self.return_serial = 1;
        }
        self.return_serial
    }

    /// Retry return hand-offs that the peer has not confirmed yet, and clean up
    /// the ones that exhausted their retry budget.
    fn retry_return_requests(&mut self) {
        let due = self.return_requests.due(Instant::now());
        for (addr, serial) in due {
            if !self.incoming_conns.contains(&addr) {
                self.return_requests.forget(addr);
                continue;
            }
            log::debug!("retrying return hand-off request to {addr} (serial {serial})");
            self.emulation.send_leave_request(addr, serial);
        }
        for addr in self.return_requests.take_exhausted() {
            self.abort_return_handoff(addr);
        }
    }

    /// Stop a return hand-off that the peer never confirmed.
    ///
    /// A v0.2.4 peer only sends `Leave` while it still holds an active capture,
    /// so if its single `Leave(0)` confirmation was lost it cannot answer a
    /// retried request. Retry exhaustion must not leave the remote-control
    /// suppression latched (stuck cursor): terminate the relationship the same
    /// way the silence timeout does, by closing the incoming session so the
    /// peer's watchdog resets its capture. The resulting `Disconnected` event
    /// repeats this cleanup idempotently.
    ///
    /// Deliberately not just `set_remote_peer(None)`: re-enabling local capture
    /// while the peer might still hold the grab is the documented feedback-loop
    /// risk.
    fn abort_return_handoff(&mut self, addr: SocketAddr) {
        log::warn!(
            "return hand-off to {addr} unanswered after {RETURN_MAX_ATTEMPTS} tries; \
             closing the incoming session to avoid a stuck cursor"
        );
        // Close the incoming session WITHOUT the explicit-disconnect signal:
        // a forced "user disconnect" (AuthRevoke) makes the peer deactivate the
        // device until someone re-enables it manually, which is far worse than
        // the stuck-cursor case this recovers from. The peer's own watchdog
        // resets its capture when the session goes away.
        self.emulation.close_session(addr);
        // The emulation backend must stop absolute tracking for this peer too,
        // otherwise a stale tracker could keep firing return requests.
        self.emulation.remote_leave();
        // Do not depend on the asynchronous Closed/Disconnected round-trip
        // for the local suppression: if that event were lost too, the cursor
        // would stay stuck, which is exactly the bug being fixed here.
        if self.remove_incoming(addr).is_some() {
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
        }
        if self.incoming_conns.is_empty() {
            self.capture.set_remote_peer(None);
        }
    }

    /// The peer released its capture (``ProtoEvent::Leave``).
    ///
    /// Drop its incoming capture zone(s) and clear the remote-control
    /// suppression once no connection is driving this machine. Keeping the
    /// zone around used to leave the local capture disabled after the first
    /// hand-off, especially when a duplicate IPv4/IPv6 session for the same
    /// peer was still registered.
    fn peer_released(&mut self, addr: SocketAddr, fingerprint: &str) {
        log::info!("peer at {addr} released its capture (return hand-off)");
        self.remove_incoming(addr);
        if !fingerprint.is_empty() {
            let stale: Vec<SocketAddr> = self
                .incoming_conn_info
                .values()
                .filter(|incoming| incoming.fingerprint == fingerprint)
                .map(|incoming| incoming.addr)
                .collect();
            for stale_addr in stale {
                self.remove_incoming(stale_addr);
            }
        }
        if self.incoming_conns.is_empty() {
            self.capture.set_remote_peer(None);
        }
    }

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        // One logical peer must hold at most one incoming transport. A peer
        // that switches between IPv4 and IPv6 (or a reconnect racing the old
        // session) used to leave two capture zones behind; the stale one kept
        // `remote_active` latched so the local capture stayed suppressed and
        // the cursor was trapped on the peer's screen.
        let superseded: Vec<(ClientHandle, SocketAddr)> = self
            .incoming_conn_info
            .iter()
            .filter(|(_, incoming)| {
                incoming.addr == addr
                    || (!fingerprint.is_empty() && incoming.fingerprint == fingerprint)
            })
            .map(|(handle, incoming)| (*handle, incoming.addr))
            .collect();
        for (handle, stale_addr) in superseded {
            self.capture.destroy(handle);
            self.incoming_conn_info.remove(&handle);
            if stale_addr != addr {
                log::info!(
                    "replacing stale incoming transport {stale_addr} of {fingerprint} with {addr}"
                );
                self.incoming_conns.remove(&stale_addr);
                self.notify_frontend(FrontendEvent::IncomingDisconnected(stale_addr));
            }
        }

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
        self.return_requests.forget(addr);
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

    /// Identity allowlist derived from the devices whose incoming direction
    /// is enabled. This is what the listener's certificate verifier checks.
    fn authorized_map(devices: &RwLock<DeviceStore>) -> HashMap<String, String> {
        devices
            .read()
            .expect("lock")
            .devices()
            .filter(|device| device.incoming.allowed)
            .map(|device| (device.fingerprint.clone(), device.name.clone()))
            .collect()
    }

    /// Re-derive the listener-facing maps from the device store and notify.
    fn refresh_maps(&mut self) {
        let authorized = Self::authorized_map(&self.devices);
        *self.authorized_keys.write().expect("lock") = authorized.clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(authorized));
    }

    fn device_snapshot(&self) -> Vec<DeviceView> {
        let store = self.devices.read().expect("lock");
        let mut devices = store
            .devices()
            .map(|device| {
                let link = self
                    .client_manager
                    .find_by_fingerprint(&device.fingerprint)
                    .and_then(|handle| {
                        let (config, state) = self.client_manager.get_state(handle)?;
                        Some((config, state))
                    });
                let incoming_session = self
                    .incoming_conn_info
                    .values()
                    .any(|incoming| incoming.fingerprint == device.fingerprint);
                let (position, addresses, port, active, connected) = match link {
                    Some((config, state)) => (
                        config.pos,
                        config.fix_ips.to_vec(),
                        config.port,
                        state.active,
                        state.alive || state.incoming_addr.is_some() || incoming_session,
                    ),
                    None => (
                        Position::Left,
                        Vec::new(),
                        lan_mouse_ipc::DEFAULT_PORT,
                        false,
                        incoming_session,
                    ),
                };
                DeviceView {
                    fingerprint: device.fingerprint.clone(),
                    name: device.name.clone(),
                    position,
                    addresses,
                    port,
                    incoming: device.incoming,
                    outgoing: device.outgoing,
                    active,
                    connected,
                    source: device.source.as_str().to_string(),
                }
            })
            .collect::<Vec<_>>();
        devices.sort_by_key(|device| device.name.to_lowercase());
        devices
    }

    fn broadcast_devices(&mut self) {
        let devices = self.device_snapshot();
        self.notify_frontend(FrontendEvent::Devices(devices));
    }

    fn broadcast_pending(&mut self) {
        let mut pending: Vec<PendingView> = self.devices.write().expect("lock").pending_views();
        for view in pending.iter_mut() {
            view.sas = crypto::sas_code(&self.public_key_fingerprint, &view.fingerprint);
        }
        self.notify_frontend(FrontendEvent::PendingRequests(pending));
    }

    /// Register a device if it is not known yet; keeps the existing record
    /// (name, policies) otherwise. Returns `true` when it was created.
    fn ensure_device(
        &mut self,
        fingerprint: &str,
        name: String,
        source: DeviceSource,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    ) -> bool {
        let mut store = self.devices.write().expect("lock");
        if store.is_known(fingerprint) {
            return false;
        }
        let name = if name.trim().is_empty() {
            fingerprint.to_string()
        } else {
            name
        };
        store.put(Device {
            fingerprint: fingerprint.to_string(),
            name,
            incoming,
            outgoing,
            source,
        });
        true
    }

    fn disconnect_fingerprint(&mut self, fingerprint: &str) {
        let live: Vec<SocketAddr> = self
            .incoming_conn_info
            .values()
            .filter(|incoming| incoming.fingerprint == fingerprint)
            .map(|incoming| incoming.addr)
            .collect();
        for addr in live {
            self.emulation.disconnect(addr);
        }
        // Sessions that never registered a capture zone are not in
        // incoming_conn_info; close them by identity too, otherwise a revoke or
        // deny of a connected peer leaves its DTLS session alive (the peer
        // keeps its client active and its cursor captured).
        self.emulation.disconnect_fingerprint(fingerprint);
    }

    /// Push the outgoing policy of a device to the capture task and stop the
    /// link if the direction was revoked.
    fn apply_outgoing_policy(&mut self, fingerprint: &str) {
        let policy = match self.devices.read().expect("lock").device(fingerprint) {
            Some(device) => device.outgoing,
            // A link whose fingerprint was learned but whose client still has
            // no device record keeps the legacy permissive behavior.
            None => return,
        };
        let Some(handle) = self.client_manager.find_by_fingerprint(fingerprint) else {
            return;
        };
        self.capture.set_outgoing_caps(handle, policy.capabilities);
        if self.client_manager.is_active(handle) && !policy.grants_anything() {
            log::info!("deactivating client {handle}: outgoing direction revoked");
            self.deactivate_client(handle);
        }
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        let Some(fingerprint) = crypto::normalize_fingerprint(&fp) else {
            log::warn!("ignoring malformed fingerprint `{fp}`");
            self.notify_frontend(FrontendEvent::Error(format!("invalid fingerprint: {fp}")));
            return;
        };
        // An explicit authorization is the user allowing this identity again.
        self.allow_device_reconnect(&fingerprint);
        let audit_name = if desc.trim().is_empty() {
            fingerprint.clone()
        } else {
            desc.clone()
        };
        {
            let mut store = self.devices.write().expect("lock");
            match store.device_mut(&fingerprint) {
                Some(device) => {
                    // An explicit authorization grants full incoming access.
                    device.incoming = DirectionPolicy::allow_all();
                    if !desc.trim().is_empty() {
                        device.name = desc;
                    }
                }
                None => {
                    let name = if desc.trim().is_empty() {
                        fingerprint.clone()
                    } else {
                        desc
                    };
                    store.put(Device::new(fingerprint.clone(), name));
                }
            }
        }
        self.refresh_maps();
        self.audit.event(
            "grant_created",
            Some(&fingerprint),
            Some(&audit_name),
            None,
            Some("source=manual"),
        );
        self.broadcast_devices();
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.revoke_device(fp, true);
    }

    fn set_device_policy(
        &mut self,
        fingerprint: String,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    ) {
        let Some(fingerprint) = crypto::normalize_fingerprint(&fingerprint) else {
            self.notify_frontend(FrontendEvent::Error(format!(
                "invalid fingerprint: {fingerprint}"
            )));
            return;
        };
        // Setting a policy is the user explicitly allowing the device again:
        // lift the post-disconnect reconnect block (and grant handshake budget).
        self.allow_device_reconnect(&fingerprint);
        {
            let mut store = self.devices.write().expect("lock");
            if store.device(&fingerprint).is_none() {
                let name = self
                    .client_manager
                    .find_by_fingerprint(&fingerprint)
                    .and_then(|handle| self.client_manager.get_state(handle))
                    .and_then(|(config, _)| config.hostname)
                    .unwrap_or_else(|| fingerprint.clone());
                store.put(Device {
                    fingerprint: fingerprint.clone(),
                    name,
                    incoming: DirectionPolicy::allow_all(),
                    outgoing: DirectionPolicy::allow_all(),
                    source: DeviceSource::Manual,
                });
            }
            store.set_policy(&fingerprint, incoming, outgoing);
        }
        log::info!(
            "device {fingerprint}: incoming={} [{}], outgoing={} [{}]",
            incoming.allowed,
            incoming.capabilities.summary(),
            outgoing.allowed,
            outgoing.capabilities.summary()
        );
        self.refresh_maps();
        self.audit.event(
            "grant_updated",
            Some(&fingerprint),
            self.devices.read().expect("lock").name_of(&fingerprint),
            None,
            Some(&format!(
                "incoming={} [{}] outgoing={} [{}]",
                incoming.allowed,
                incoming.capabilities.summary(),
                outgoing.allowed,
                outgoing.capabilities.summary()
            )),
        );
        if !incoming.allowed {
            self.disconnect_fingerprint(&fingerprint);
        }
        self.apply_outgoing_policy(&fingerprint);
        self.save_config();
        self.broadcast_devices();
    }

    fn rename_device(&mut self, fingerprint: String, name: String) {
        let Some(fingerprint) = crypto::normalize_fingerprint(&fingerprint) else {
            self.notify_frontend(FrontendEvent::Error(format!(
                "invalid fingerprint: {fingerprint}"
            )));
            return;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        if !self
            .devices
            .write()
            .expect("lock")
            .rename(&fingerprint, name.clone())
        {
            return;
        }
        // Keep the link display in sync with the device name.
        if let Some(handle) = self.client_manager.find_by_fingerprint(&fingerprint) {
            self.update_hostname(handle, Some(name));
        }
        self.refresh_maps();
        self.save_config();
        self.broadcast_devices();
    }

    /// Revoke a device: forget its identity, deny the fingerprint and drop
    /// live sessions. The outgoing link is kept (deactivated) so it can be
    /// re-authorized without retyping the address.
    fn revoke_device(&mut self, fp: String, disconnect: bool) {
        let Some(fingerprint) = crypto::normalize_fingerprint(&fp) else {
            log::warn!("ignoring malformed fingerprint `{fp}`");
            return;
        };
        self.devices
            .write()
            .expect("lock")
            .deny(fingerprint.clone());
        self.devices
            .write()
            .expect("lock")
            .pending_remove(&fingerprint);
        self.refresh_maps();
        if disconnect {
            self.disconnect_fingerprint(&fingerprint);
        }
        if let Some(handle) = self.client_manager.find_by_fingerprint(&fingerprint) {
            self.deactivate_client(handle);
        }
        self.audit.event(
            "revoke",
            Some(&fingerprint),
            None,
            None,
            Some(if disconnect {
                "reason=user disconnect=true"
            } else {
                "reason=user"
            }),
        );
        self.save_config();
        self.broadcast_devices();
        self.broadcast_pending();
        log::info!("revoked device {fingerprint}");
    }

    /// Deny a device for `expires_in` seconds (`None` = permanent).
    fn deny_device(&mut self, fp: String, expires_in: Option<u64>) {
        let Some(fingerprint) = crypto::normalize_fingerprint(&fp) else {
            self.notify_frontend(FrontendEvent::Error(format!("invalid fingerprint: {fp}")));
            return;
        };
        let expires = expires_in.map(|seconds| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_add(seconds)
        });
        self.devices.write().expect("lock").deny_until(
            fingerprint.clone(),
            expires,
            Some("user".to_string()),
        );
        self.refresh_maps();
        self.disconnect_fingerprint(&fingerprint);
        if let Some(handle) = self.client_manager.find_by_fingerprint(&fingerprint) {
            self.deactivate_client(handle);
        }
        self.audit.event(
            "deny",
            Some(&fingerprint),
            None,
            None,
            Some(&match expires {
                Some(expires) => format!("reason=user expires={expires}"),
                None => "reason=user".to_string(),
            }),
        );
        self.save_config();
        self.broadcast_devices();
        self.broadcast_pending();
        log::info!("denied device {fingerprint} (expires={expires:?})");
    }

    /// Grant a device for the current session only.
    ///
    /// The record is never persisted and is removed as soon as the peer
    /// disconnects, so the user is asked again next time.
    fn grant_session(
        &mut self,
        fp: String,
        name: Option<String>,
        position: Option<Position>,
        incoming: DirectionPolicy,
    ) {
        let Some(fingerprint) = crypto::normalize_fingerprint(&fp) else {
            self.notify_frontend(FrontendEvent::Error(format!("invalid fingerprint: {fp}")));
            return;
        };
        // An explicit allow is the user lifting a post-disconnect block.
        self.allow_device_reconnect(&fingerprint);
        let pending = self
            .devices
            .write()
            .expect("lock")
            .pending_remove(&fingerprint);
        let name = name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| pending.as_ref().and_then(|pending| pending.name.clone()))
            .unwrap_or_else(|| fingerprint.clone());
        self.devices.write().expect("lock").put_ephemeral(Device {
            fingerprint: fingerprint.clone(),
            name: name.clone(),
            incoming,
            // A session grant never configures outgoing control.
            outgoing: DirectionPolicy::deny(),
            source: DeviceSource::Manual,
        });
        self.refresh_maps();
        self.audit.event(
            "session_grant",
            Some(&fingerprint),
            Some(&name),
            pending
                .as_ref()
                .map(|pending| pending.addr.to_string())
                .as_deref(),
            Some(&format!(
                "incoming=[{}] outgoing=none",
                incoming.capabilities.summary()
            )),
        );
        let _ = position;
        // Session grants are not persisted, so save only rewrites the
        // persistent part of the config.
        self.save_config();
        self.broadcast_devices();
        self.broadcast_pending();
        log::info!("granted session access to {fingerprint} ({name})");
    }

    /// Drop session-only grants whose peer disconnected.
    fn expire_session_grants(&mut self, addr: SocketAddr) {
        let fingerprint = self
            .incoming_conn_info
            .values()
            .find(|incoming| incoming.addr == addr)
            .map(|incoming| incoming.fingerprint.clone());
        let Some(fingerprint) = fingerprint else {
            return;
        };
        let removed = self
            .devices
            .write()
            .expect("lock")
            .remove_ephemeral(&fingerprint);
        if !removed {
            return;
        }
        log::info!("session grant for {fingerprint} expired (peer disconnected)");
        self.audit.event(
            "session_grant_expired",
            Some(&fingerprint),
            None,
            Some(&addr.to_string()),
            Some("reason=disconnected"),
        );
        self.refresh_maps();
        self.save_config();
        self.broadcast_devices();
    }

    fn respond_pending(
        &mut self,
        fingerprint: String,
        accept: bool,
        name: Option<String>,
        position: Option<Position>,
        incoming: Option<DirectionPolicy>,
        outgoing: Option<DirectionPolicy>,
    ) {
        let Some(fingerprint) = crypto::normalize_fingerprint(&fingerprint) else {
            self.notify_frontend(FrontendEvent::Error(format!(
                "invalid fingerprint: {fingerprint}"
            )));
            return;
        };
        let pending = self
            .devices
            .write()
            .expect("lock")
            .pending_remove(&fingerprint);
        let previous_fingerprint = pending
            .as_ref()
            .and_then(|pending| pending.previous_fingerprint.clone());
        if !accept {
            self.devices
                .write()
                .expect("lock")
                .deny(fingerprint.clone());
            self.audit.event(
                "deny",
                Some(&fingerprint),
                pending
                    .as_ref()
                    .and_then(|pending| pending.name.clone())
                    .as_deref(),
                pending
                    .as_ref()
                    .map(|pending| pending.addr.to_string())
                    .as_deref(),
                Some("source=manual"),
            );
            self.refresh_maps();
            // Denying a device also ends any live session and stops the
            // outgoing link instead of leaving it active.
            self.disconnect_fingerprint(&fingerprint);
            if let Some(handle) = self.client_manager.find_by_fingerprint(&fingerprint) {
                self.deactivate_client(handle);
            }
            self.save_config();
            self.broadcast_devices();
            self.broadcast_pending();
            log::info!("connection request from {fingerprint} denied");
            return;
        }

        let name = name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| pending.as_ref().and_then(|pending| pending.name.clone()))
            .unwrap_or_else(|| fingerprint.clone());
        let incoming = incoming.unwrap_or_else(DirectionPolicy::allow_all);
        let outgoing = outgoing.unwrap_or_else(DirectionPolicy::allow_all);
        self.audit.event(
            "grant_created",
            Some(&fingerprint),
            Some(&name),
            pending
                .as_ref()
                .map(|pending| pending.addr.to_string())
                .as_deref(),
            Some(&format!(
                "source=manual incoming=[{}] outgoing=[{}]",
                incoming.capabilities.summary(),
                outgoing.capabilities.summary()
            )),
        );
        {
            let mut store = self.devices.write().expect("lock");
            store.put(Device {
                fingerprint: fingerprint.clone(),
                name: name.clone(),
                incoming,
                outgoing,
                source: DeviceSource::Manual,
            });
        }
        // Certificate rotation: accepting the new identity must not leave the
        // old one trusted/configured. Re-pin matching clients and revoke the
        // previous fingerprint so a stale certificate cannot reconnect.
        if let Some(previous) = previous_fingerprint {
            if previous == fingerprint {
                // Nothing to rotate; keep the normal approval path below.
            } else {
                self.devices.write().expect("lock").remove(&previous);
                self.authorized_keys
                    .write()
                    .expect("lock")
                    .remove(&previous);
                self.disconnect_fingerprint(&previous);
                for handle in self.client_manager.all_handles_for_fingerprint(&previous) {
                    self.deactivate_client_and_peer(handle, false);
                    self.client_manager
                        .set_fingerprint(handle, Some(fingerprint.clone()));
                    self.client_manager.set_hostname(handle, Some(name.clone()));
                    self.activate_client(handle);
                    self.broadcast_client(handle);
                }
                self.audit.event(
                    "cert_rotated",
                    Some(&fingerprint),
                    Some(&name),
                    pending
                        .as_ref()
                        .map(|pending| pending.addr.to_string())
                        .as_deref(),
                    Some(&format!("previous={previous} action=revoke_old")),
                );
                log::info!("rotated device identity {previous} -> {fingerprint}");
            }
        }
        self.refresh_maps();
        // Only set up the reverse direction when it is actually allowed; the
        // peer connection then creates the link instead of it appearing
        // unconditionally.
        if outgoing.grants_anything() {
            self.pending_reverse.insert(
                fingerprint.clone(),
                (name, position.unwrap_or(Position::Left)),
            );
        }
        log::info!(
            "approved device {fingerprint}: incoming [{}], outgoing [{}]",
            incoming.capabilities.summary(),
            outgoing.capabilities.summary()
        );
        self.save_config();
        self.broadcast_devices();
        self.broadcast_pending();
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    /// Address rule entries, rendered for the legacy address list event.
    fn trusted_address_strings(&self) -> Vec<String> {
        self.trusted_nets
            .read()
            .expect("lock")
            .iter()
            .map(|rule| rule.net.to_string())
            .collect()
    }

    /// Structured address rules with their permissions.
    fn address_rule_views(&self) -> Vec<AddressRuleView> {
        self.trusted_nets
            .read()
            .expect("lock")
            .iter()
            .map(|rule| AddressRuleView {
                address: rule.net.to_string(),
                incoming: rule.incoming,
                outgoing: rule.outgoing,
                broad: rule.net.is_broad(),
            })
            .collect()
    }

    fn broadcast_address_rules(&mut self) {
        self.notify_frontend(FrontendEvent::TrustedAddresses(
            self.trusted_address_strings(),
        ));
        self.notify_frontend(FrontendEvent::AddressRules(self.address_rule_views()));
    }

    /// Serialize the persistent permission policy for backup/transfer.
    fn export_policy(&mut self) {
        let rules = self.trusted_nets.read().expect("lock").clone();
        let bundle = self.devices.read().expect("lock").export_bundle(&rules);
        match serde_json::to_string_pretty(&bundle) {
            Ok(json) => {
                self.audit.event("policy_exported", None, None, None, None);
                self.notify_frontend(FrontendEvent::PolicyExport { json });
            }
            Err(e) => self.notify_frontend(FrontendEvent::Error(format!(
                "could not serialize policy: {e}"
            ))),
        }
    }

    /// Replace the permission policy from a JSON bundle.
    fn import_policy(&mut self, json: String) {
        let bundle: PolicyBundle = match serde_json::from_str(&json) {
            Ok(bundle) => bundle,
            Err(e) => {
                self.notify_frontend(FrontendEvent::Error(format!("invalid policy file: {e}")));
                return;
            }
        };
        let mut rules = self.trusted_nets.read().expect("lock").clone();
        let result = self
            .devices
            .write()
            .expect("lock")
            .import_bundle(&bundle, &mut rules);
        match result {
            Ok(()) => {
                *self.trusted_nets.write().expect("lock") = rules;
                self.refresh_maps();
                self.audit.event(
                    "policy_imported",
                    None,
                    None,
                    None,
                    Some(&format!(
                        "devices={} rules={} denied={}",
                        bundle.devices.len(),
                        bundle.address_rules.len(),
                        bundle.denied.len()
                    )),
                );
                self.save_config();
                self.broadcast_devices();
                self.broadcast_address_rules();
                log::info!(
                    "imported policy: {} device(s), {} rule(s), {} denied",
                    bundle.devices.len(),
                    bundle.address_rules.len(),
                    bundle.denied.len()
                );
            }
            Err(e) => self.notify_frontend(FrontendEvent::Error(format!("invalid policy: {e}"))),
        }
    }

    /// Emit the most recent persisted audit entries, newest first.
    fn broadcast_history(&mut self, limit: u32) {
        let limit = limit.clamp(1, 500) as usize;
        let history = self
            .audit
            .recent(limit)
            .into_iter()
            .map(history_entry_from_record)
            .collect::<Vec<_>>();
        self.notify_frontend(FrontendEvent::ConnectionHistory(history));
        self.broadcast_pending();
    }

    /// Add an address rule (IP or CIDR) with explicit permissions.
    ///
    /// A device adopted through the rule gets exactly these permissions; the
    /// user can still narrow or revoke it per device afterwards.
    fn add_address_rule(
        &mut self,
        address: String,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    ) {
        let address = address.trim().to_string();
        let Some(net) = crate::trusted::TrustedNet::parse(&address) else {
            log::warn!("ignoring invalid address rule `{address}`");
            self.notify_frontend(FrontendEvent::Error(format!(
                "invalid address rule: {address}"
            )));
            return;
        };
        if net.is_broad() {
            log::warn!(
                "address rule `{address}` is very broad: every certificate in it \
                 will be authorized without a prompt"
            );
            self.notify_frontend(FrontendEvent::Error(format!(
                "warning: address rule {address} is very broad"
            )));
        }
        {
            let mut rules = self.trusted_nets.write().expect("lock");
            if rules.iter().any(|rule| rule.net == net) {
                return;
            }
            rules.push(crate::trusted::AddressRule::new(net, incoming, outgoing));
        }
        let rules = self.trusted_nets.read().expect("lock").clone();
        self.config.set_address_rules(rules);
        self.audit.event(
            "address_rule_added",
            None,
            None,
            Some(&address),
            Some(&format!(
                "incoming=[{}] outgoing=[{}]",
                incoming.capabilities.summary(),
                outgoing.capabilities.summary()
            )),
        );
        log::info!(
            "address rule added: {address} (in [{}], out [{}])",
            incoming.capabilities.summary(),
            outgoing.capabilities.summary()
        );
        self.broadcast_address_rules();
    }

    /// Legacy request: add an address with full access.
    fn trust_address(&mut self, entry: String) {
        self.add_address_rule(
            entry,
            DirectionPolicy::allow_all(),
            DirectionPolicy::allow_all(),
        );
    }

    fn untrust_address(&mut self, entry: &str) {
        let entry = entry.trim();
        let Some(net) = crate::trusted::TrustedNet::parse(entry) else {
            self.notify_frontend(FrontendEvent::Error(format!(
                "invalid address rule: {entry}"
            )));
            return;
        };
        let before = self.trusted_nets.read().expect("lock").len();
        self.trusted_nets
            .write()
            .expect("lock")
            .retain(|rule| rule.net != net);
        if self.trusted_nets.read().expect("lock").len() == before {
            return;
        }
        let rules = self.trusted_nets.read().expect("lock").clone();
        self.config.set_address_rules(rules);
        self.audit
            .event("address_rule_removed", None, None, Some(entry), None);
        log::info!("address rule removed: {entry}");
        self.broadcast_address_rules();
    }

    /// Trust one new device for the next `seconds`, granting `incoming` and
    /// `outgoing` to whatever device is accepted.
    fn start_pairing(
        &mut self,
        seconds: u64,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    ) {
        let seconds = seconds.clamp(5, 600);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        *self.pairing_until.write().expect("lock") = Some(PairingWindow {
            until,
            incoming,
            outgoing,
        });
        self.pairing_adopted.clear();
        // Opening the window twice must not let the older timer close the new
        // window early: each timer checks the generation it was started with.
        let generation = self.pairing_generation.get().wrapping_add(1);
        self.pairing_generation.set(generation);
        self.audit.event(
            "pairing_open",
            None,
            None,
            None,
            Some(&format!(
                "seconds={seconds} incoming=[{}] outgoing=[{}]",
                incoming.capabilities.summary(),
                outgoing.capabilities.summary()
            )),
        );
        log::info!(
            "pairing mode open for {seconds}s (incoming [{}], outgoing [{}])",
            incoming.capabilities.summary(),
            outgoing.capabilities.summary()
        );
        self.notify_frontend(FrontendEvent::PairingActive(true));

        let pairing = self.pairing_until.clone();
        let generation_cell = self.pairing_generation.clone();
        let tx = self.discovery_tx.clone();
        spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            if generation_cell.get() != generation {
                return;
            }
            let still_open = pairing
                .write()
                .expect("lock")
                .take()
                .is_some_and(|window| window.is_open());
            if still_open {
                log::info!("pairing mode closed");
                let _ = tx.send(DiscoveryEvent::Pairing(false));
            }
        });
    }

    /// Close the pairing window (single device accepted, user/CLI request or
    /// timeout), emit the review of what was adopted and notify the frontend.
    fn end_pairing(&mut self) {
        let was_open = self
            .pairing_until
            .write()
            .expect("lock")
            .take()
            .is_some_and(|window| window.is_open());
        self.pairing_generation
            .set(self.pairing_generation.get().wrapping_add(1));
        if was_open {
            log::info!("pairing mode closed");
        }
        if !self.pairing_adopted.is_empty() {
            let adopted = std::mem::take(&mut self.pairing_adopted);
            let review = self
                .device_snapshot()
                .into_iter()
                .filter(|device| adopted.contains(&device.fingerprint))
                .collect::<Vec<_>>();
            self.audit.event(
                "pairing_close",
                None,
                None,
                None,
                Some(&format!(
                    "adopted={} ({})",
                    review.len(),
                    review
                        .iter()
                        .map(|device| device.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
            log::info!(
                "pairing review: {} device(s) adopted ({})",
                review.len(),
                review
                    .iter()
                    .map(|device| device.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            self.notify_frontend(FrontendEvent::PairingReview(review));
        }
        self.notify_frontend(FrontendEvent::PairingActive(false));
    }

    /// Kick off a background scan of the local subnets.
    fn start_scan(&self) {
        let tx = self.discovery_tx.clone();
        let discovery_port = self.discovery_port;
        spawn_local(async move {
            let devices = discovery::scan(discovery_port).await;
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
                for device in &devices {
                    self.apply_discovered_hostname(device);
                }
                for device in &mut devices {
                    self.annotate_discovered(device);
                }
                self.discovered.clear();
                for device in &devices {
                    self.discovered.insert(device.ip, device.clone());
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
                self.broadcast_network();
            }
            Some(DiscoveryEvent::Probe { ip, lan_mouse }) => {
                self.notify_frontend(FrontendEvent::ProbeResult { ip, lan_mouse });
            }
            Some(DiscoveryEvent::Pairing(active)) => {
                if active {
                    self.notify_frontend(FrontendEvent::PairingActive(true));
                } else {
                    self.end_pairing();
                }
            }
            None => {}
        }
    }

    /// Enrich a discovery result with local knowledge the beacon cannot prove.
    fn annotate_discovered(&self, device: &mut DiscoveredDevice) {
        device.configured = self.client_with_ip(device.ip).is_some();
        device.fingerprint_verified = device
            .fingerprint
            .as_deref()
            .is_some_and(|fingerprint| self.devices.read().expect("lock").is_known(fingerprint));
        device.last_seen = Some(now_millis());
    }

    /// Send the current device table to the frontend, useful entries first.
    fn broadcast_network(&mut self) {
        // Discovery results are only a hint; stale entries from a peer that
        // went away must not stay in the Network tab forever.
        let cutoff = now_millis().saturating_sub(5 * 60 * 1000);
        self.discovered
            .retain(|_, device| device.last_seen.is_none_or(|last_seen| last_seen >= cutoff));
        let mut devices: Vec<DiscoveredDevice> = self.discovered.values().cloned().collect();
        for device in &mut devices {
            self.annotate_discovered(device);
        }
        devices.sort_by_key(|device| {
            let ip = match device.ip {
                IpAddr::V4(ip) => u32::from(ip),
                IpAddr::V6(ip) => ip.segments().iter().map(|s| *s as u32).sum(),
            };
            (!device.lan_mouse, !device.configured, ip)
        });
        self.notify_frontend(FrontendEvent::NetworkScan(devices));
    }

    /// Best human-readable name for a connecting peer: prefer its discovered
    /// network hostname, then any non-generic description stored for its key
    /// fingerprint, falling back to its IP address string.
    fn peer_display_name(&self, ip: IpAddr, fingerprint: &str) -> String {
        if let Some(host) = self
            .discovered
            .get(&ip)
            .and_then(|d| d.hostname.as_deref())
            .map(str::trim)
            .filter(|h| !h.is_empty())
        {
            return host.to_string();
        }
        if let Some(desc) = self
            .authorized_keys
            .read()
            .ok()
            .and_then(|keys| keys.get(fingerprint).cloned())
        {
            let trimmed = desc.trim();
            if !trimmed.is_empty() && !trimmed.starts_with("trusted ") {
                return trimmed.to_string();
            }
        }
        ip.to_string()
    }

    /// If a discovered device reports a real hostname and we have a configured
    /// client for that IP whose hostname is still blank or just the IP string,
    /// upgrade the client's hostname in-place so the UI shows the computer name.
    fn apply_discovered_hostname(&mut self, device: &DiscoveredDevice) {
        let Some(hostname) = device.hostname.as_deref() else {
            return;
        };
        // Address first, then certificate identity, then the display name:
        // a saved client created by pairing may have no configured address and
        // only its hostname, which is exactly the case where DNS failure made
        // Connect fail until the device was deleted and re-added.
        let handle = self
            .client_with_ip(device.ip)
            .or_else(|| {
                device
                    .fingerprint
                    .as_deref()
                    .and_then(|fingerprint| self.client_with_fingerprint(fingerprint))
            })
            .or_else(|| {
                self.client_manager
                    .registered_clients()
                    .into_iter()
                    .find(|handle| {
                        self.client_manager
                            .get_state(*handle)
                            .and_then(|(config, _)| config.hostname.clone())
                            .is_some_and(|name| name.eq_ignore_ascii_case(hostname))
                    })
            });
        let Some(handle) = handle else {
            return;
        };
        let mut changed = self
            .client_manager
            .upgrade_hostname_from_discovery(handle, hostname);
        if self
            .client_manager
            .learn_address_from_discovery(handle, device.ip)
        {
            log::info!(
                "learned address {} for saved client {handle} from discovery",
                device.ip
            );
            changed = true;
        }
        if changed {
            self.broadcast_client(handle);
            self.save_config();
        }
    }

    /// Create and activate a client entry for a peer that was just authorized.
    ///
    /// The peer address comes from the accepted connection, so the user never
    /// has to type it. An existing entry for the same address is reused
    /// instead of creating a duplicate.
    fn add_reverse_client(
        &mut self,
        name: String,
        ip: IpAddr,
        position: Position,
        fingerprint: Option<String>,
    ) {
        // Identity first: the certificate is the device. An entry that is
        // already configured for this fingerprint is reused as-is instead of
        // creating a second row (which used to deactivate the configured one
        // through the position collision).
        if let Some(fingerprint) = fingerprint.as_deref() {
            if let Some(handle) = self.client_with_fingerprint(fingerprint) {
                log::info!("reverse client for {ip} already configured (id {handle})");
                self.attach_incoming(handle, SocketAddr::new(ip, 0));
                return;
            }
        }

        // An empty name means "use the discovered hostname or address": the
        // peer's hostname might be known via LAN discovery beacons.
        let name = if name.trim().is_empty() {
            self.discovered
                .get(&ip)
                .and_then(|d| d.hostname.clone())
                .filter(|h| !h.trim().is_empty())
                .unwrap_or_else(|| ip.to_string())
        } else {
            name
        };

        // Legacy entries configured only by address (no certificate pinned
        // yet) are reused, but never when they are pinned to another
        // certificate: that would let a peer at a reused address hijack it.
        if let Some(handle) = self.client_with_ip(ip) {
            let compatible = reverse_client_compatible(
                fingerprint.as_deref(),
                self.client_manager.fingerprint(handle).as_deref(),
            );
            if compatible {
                log::info!("reverse client for {ip} already exists (id {handle})");
                let _ = self
                    .client_manager
                    .upgrade_hostname_from_discovery(handle, &name);
                if fingerprint.is_some() && self.client_manager.fingerprint(handle).is_none() {
                    self.client_manager.set_fingerprint(handle, fingerprint);
                }
                self.attach_incoming(handle, SocketAddr::new(ip, 0));
                self.save_config();
                return;
            }
        }

        let handle = self.client_manager.add_client();
        self.client_manager.set_hostname(handle, Some(name.clone()));
        self.client_manager.set_fix_ips(handle, vec![ip]);
        self.client_manager.set_pos(handle, position);
        self.client_manager.set_fingerprint(handle, fingerprint);
        self.auto_reverse_clients.insert(handle);
        log::info!("adding client {handle} for authorized device {name} ({ip}, {position})");
        let (config, state) = self.client_manager.get_state(handle).expect("client state");
        self.notify_frontend(FrontendEvent::Created(handle, config, state));

        // Never guess the peer's listen port: dialling DEFAULT_PORT created a
        // phantom connection to the wrong service. Without a discovered port
        // the entry stays inactive until the user fills in the address.
        match self.discovered.get(&ip).and_then(|device| device.port) {
            Some(port) => {
                self.client_manager.set_port(handle, port);
                self.activate_client(handle);
            }
            None => log::info!(
                "reverse client {handle} created inactive: listen port of {ip} is unknown"
            ),
        }
        self.save_config();
    }

    fn client_with_ip(&self, ip: IpAddr) -> Option<ClientHandle> {
        self.client_manager.find_by_ip(ip)
    }

    /// Client that represents this certificate identity, if any.
    fn client_with_fingerprint(&self, fingerprint: &str) -> Option<ClientHandle> {
        self.client_manager.find_by_fingerprint(fingerprint)
    }

    /// Incoming session address already registered for this identity.
    fn incoming_addr_for_fingerprint(&self, fingerprint: &str) -> Option<SocketAddr> {
        self.incoming_conn_info
            .values()
            .find(|incoming| incoming.fingerprint == fingerprint)
            .map(|incoming| incoming.addr)
    }

    /// Client entry whose incoming session is `addr`, if any.
    ///
    /// Incoming addresses are matched through the session registry first: the
    /// peer's certificate identity and its route (IPv4/IPv6, ephemeral source
    /// port) are different things and the route alone must not pick a client.
    fn client_for_incoming(&self, addr: SocketAddr) -> Option<ClientHandle> {
        self.client_manager
            .registered_clients()
            .into_iter()
            .find(|handle| self.client_manager.incoming_addr(*handle) == Some(addr))
            .or_else(|| self.client_with_ip(addr.ip()))
    }

    /// Client entry for an incoming peer, preferring the certificate identity.
    fn client_for_peer(&self, addr: SocketAddr, fingerprint: &str) -> Option<ClientHandle> {
        if !fingerprint.is_empty() {
            if let Some(handle) = self.client_with_fingerprint(fingerprint) {
                return Some(handle);
            }
        }
        self.client_for_incoming(addr)
    }

    /// Register an incoming session on an existing client entry.
    ///
    /// Reusing the configured client is what keeps one logical peer from being
    /// shown as two devices. Its position and port are left untouched: an
    /// incoming connection only proves the peer is reachable.
    fn attach_incoming(&mut self, handle: ClientHandle, addr: SocketAddr) {
        let mut changed = false;
        if self.client_manager.incoming_addr(handle) != Some(addr) {
            self.client_manager.set_incoming_addr(handle, Some(addr));
            changed = true;
        }
        // The peer just proved it can reach this machine. Learn its source
        // address so the outgoing half of the link can dial it even when the
        // configured hostname does not resolve (mDNS/DNS failure); this is the
        // difference between "Peer connected" and a half-open bidirectional
        // link that shows "outgoing connection still being established".
        if self.client_manager.add_peer_ip(handle, addr.ip()) {
            log::info!(
                "learned peer address {} for client {handle} from incoming session",
                addr.ip()
            );
            changed = true;
        }
        if changed {
            self.broadcast_client(handle);
        }
        // Connecting is bidirectional by definition: if the peer reached us,
        // this machine must be able to reach the peer too.
        self.ensure_bidirectional(handle);
        // Kick a fresh outgoing attempt immediately instead of waiting for the
        // reconnect backoff: the incoming session just gave us a reachable
        // address (the previous attempts may have failed only because DNS was
        // down). `start_connecting` is gated on the client being active.
        self.capture.connect(handle);
    }

    /// An established incoming session means the peer is connected to this
    /// machine. The configured outgoing link must then be active as well, so a
    /// connection is bidirectional automatically instead of requiring the old
    /// "Also connect" button.
    fn ensure_bidirectional(&mut self, handle: ClientHandle) {
        let auto_reverse = self.auto_reverse_clients.contains(&handle);
        let known_port = if auto_reverse {
            // An auto-created reverse client has no known listen port yet.
            // Dialing the default port would create a phantom connection, so
            // stay receiving-only until discovery or the user supplies the
            // address.
            self.client_manager
                .get_state(handle)
                .and_then(|(config, _)| {
                    config
                        .fix_ips
                        .first()
                        .and_then(|ip| self.discovered.get(ip).and_then(|device| device.port))
                })
        } else {
            None
        };
        match bidirectional_action(
            self.client_manager.is_active(handle),
            auto_reverse,
            known_port,
        ) {
            BidirectionalAction::AlreadyActive => {}
            BidirectionalAction::NeedsPort => log::info!(
                "incoming session established but the listen port of the peer is unknown; keeping client {handle} receiving-only"
            ),
            BidirectionalAction::Activate { port } => {
                if let Some(port) = port {
                    self.client_manager.set_port(handle, port);
                }
                log::info!(
                    "incoming session established: activating client {handle} back (bidirectional)"
                );
                self.set_client_active(handle, true);
            }
        }
    }

    /// Drop an auto-added reverse client once the configured client for the
    /// same certificate learns its fingerprint (TOFU).
    ///
    /// A peer that connects before its outgoing link is up used to leave the
    /// incoming session attached to an auto-created client on the default
    /// port, while the configured client stayed deactivated by the position
    /// collision — two rows for one peer and a stale capture zone.
    fn merge_auto_reverse_client(&mut self, handle: ClientHandle, fingerprint: &str) {
        let candidates: Vec<(ClientHandle, Option<String>)> = self
            .auto_reverse_clients
            .iter()
            .map(|other| (*other, self.client_manager.fingerprint(*other)))
            .collect();
        let duplicates = duplicate_reverse_clients(&candidates, handle, fingerprint);
        if duplicates.is_empty() {
            return;
        }
        for duplicate in duplicates {
            let incoming = self.client_manager.incoming_addr(duplicate);
            if let Some(addr) = incoming {
                if self.client_manager.incoming_addr(handle).is_none() {
                    self.client_manager.set_incoming_addr(handle, Some(addr));
                }
            }
            log::info!(
                "merging auto-added reverse client {duplicate} into configured client {handle} \
                 (same certificate {fingerprint})"
            );
            self.remove_client(duplicate);
        }
        self.broadcast_client(handle);
        self.save_config();
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    /// Lift the post-disconnect reconnect block for an identity (explicit user
    /// action) and grant a one-shot handshake budget so the deliberate
    /// reconnect is not delayed by the per-IP handshake limiter.
    fn allow_device_reconnect(&mut self, fingerprint: &str) {
        let mut store = self.devices.write().expect("lock");
        if let Some(addr) = store.allow_reconnect(fingerprint) {
            store.grant_handshake_budget(addr);
        }
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            // The user explicitly asked for this client again: allow the
            // identity to reconnect (lift the post-disconnect block).
            if let Some(fingerprint) = self.client_manager.fingerprint(handle) {
                self.allow_device_reconnect(&fingerprint);
            }
            self.activate_client(handle);
        } else {
            self.deactivate_client_and_peer(handle, true);
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

    /// Explicit user disconnect (or a peer's explicit disconnect): unlike
    /// internal deactivations (position change, another client at the same
    /// edge), this must tear the live link down on both machines. The client is
    /// marked inactive first so the reconnect loop cannot revive it; the
    /// outgoing transport sends `AuthRevoke` before closing and all incoming
    /// sessions for the same identity are closed too.
    fn deactivate_client_and_peer(&mut self, handle: ClientHandle, user_initiated: bool) {
        log::debug!("explicitly disconnecting client {handle}");
        let fingerprint = self.client_manager.fingerprint(handle);
        // A peer that controls this machine uses a reverse (inactive) client
        // entry; the full teardown below is what stops it.
        let incoming_addr = self.client_manager.incoming_addr(handle);
        let had_incoming = incoming_addr.is_some();
        // Mark inactive first so the reconnect loop cannot revive the link.
        let was_active = self.client_manager.deactivate_client(handle);
        // Close the outgoing transport with an explicit AuthRevoke (only when
        // there was one; a receiving-only client has no outgoing transport).
        if was_active {
            self.capture.disconnect(handle);
        }
        // A live incoming session can exist for a client that is itself
        // inactive (the peer controls us; our configured client points back).
        // The full teardown must run in that case too: otherwise closing the
        // connection on this machine left the peer controlling it.
        if let Some(addr) = self.client_manager.incoming_addr(handle) {
            self.emulation.remote_leave();
            if let Some(addr) = self.remove_incoming(addr) {
                self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            }
        }
        if let Some(fingerprint) = fingerprint.as_deref() {
            if !fingerprint.is_empty() {
                // Closes every remaining live session of this identity (and
                // sends AuthRevoke to the peer).
                self.disconnect_fingerprint(fingerprint);
            }
        }
        self.capture.destroy(handle);
        self.client_manager.set_incoming_addr(handle, None);
        if self.incoming_conns.is_empty() {
            self.capture.set_remote_peer(None);
        }
        if user_initiated && had_incoming {
            if let Some(fingerprint) = fingerprint.as_deref() {
                if !fingerprint.is_empty() {
                    // Reject the peer's next handshake at the listener: a peer
                    // that ignores the revoke (or lost it) must not be able to
                    // re-establish a session and inject input behind the UI's
                    // back. The user lifts this by allowing the device again.
                    let peer_ip = incoming_addr
                        .map(|addr| addr.ip())
                        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
                    self.devices
                        .write()
                        .expect("lock")
                        .suppress_reconnect(fingerprint.to_string(), peer_ip);
                    self.notify_frontend(FrontendEvent::Error(format!(
                        "device {} disconnected locally; allow it again in Devices to reconnect",
                        &fingerprint[..fingerprint.len().min(16)]
                    )));
                    log::info!(
                        "blocking reconnects from {fingerprint} until the device is allowed again"
                    );
                }
            }
        }
        self.broadcast_client(handle);
        log::info!("explicitly disconnected client {handle}");
    }

    /// Deactivate every configured/local client entry belonging to `fingerprint`.
    /// Used when the peer explicitly disconnects and must stop reconnecting.
    fn deactivate_fingerprint(&mut self, fingerprint: &str) {
        let handles = self.client_manager.all_handles_for_fingerprint(fingerprint);
        for handle in handles {
            self.deactivate_client_and_peer(handle, false);
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        // Outgoing direction gate: controlling the peer is only possible when
        // the outgoing policy grants at least pointer or keyboard. Clients
        // without a known fingerprint keep the legacy permissive behavior.
        let outgoing_caps = match self.client_manager.fingerprint(handle) {
            Some(fingerprint) => {
                let policy = self.devices.read().expect("lock").outgoing(&fingerprint);
                if !policy.grants_anything() {
                    self.notify_frontend(FrontendEvent::Error(
                        "outgoing control is not allowed for this device".to_string(),
                    ));
                    log::warn!("not activating client {handle}: outgoing direction disabled");
                    return;
                }
                policy.capabilities
            }
            None => Capabilities::ALL,
        };

        // Create the (possibly portal-backed) emulation backend now, while the
        // user is configuring the link, so the first crossing does not pay the
        // backend-creation cost.
        self.emulation.warm_up();

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                let same_peer = matches!(
                    (
                        self.client_manager.fingerprint(handle),
                        self.client_manager.fingerprint(other),
                    ),
                    (Some(a), Some(b)) if a == b
                );
                if same_peer {
                    // Two entries of the same certificate at one position: do
                    // not tear down the user-configured link for the service's
                    // own duplicate.
                    log::warn!("not deactivating {other}: it is the same peer as {handle}");
                } else {
                    self.deactivate_client(other);
                }
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            self.capture.create(handle, pos, CaptureType::Default);
            self.capture.set_outgoing_caps(handle, outgoing_caps);
            self.capture.set_sensitivity(
                handle,
                self.client_manager.get_sensitivity(handle) as f64 / 100.0,
            );
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
        self.learned_fingerprints.remove(&handle);
        self.auto_reverse_clients.remove(&handle);
        // Remove the entry first so the connection layer's reconnect loop sees
        // an inactive client, then close the live transport. Without this a
        // deleted/reloaded client kept its DTLS socket and ping/receive tasks
        // running, and the slab can reuse the handle for a new client which
        // would then silently inherit the stale transport.
        let was_active = self
            .client_manager
            .remove_client(handle)
            .map(|(_, state)| state.active)
            .unwrap_or(false);
        self.capture.drop_transport(handle);
        if was_active {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        // If the live route is no longer part of the client's address set, a
        // transport to it would keep the UI claiming an endpoint that no
        // longer exists. Drop it; the reconnect loop (client still active)
        // dials the addresses that remain.
        if let Some(active_addr) = self.client_manager.active_addr(handle) {
            let still_configured = self
                .client_manager
                .get_ips(handle)
                .is_some_and(|ips| ips.contains(&active_addr.ip()));
            if !still_configured && self.client_manager.is_active(handle) {
                log::info!("client {handle}: live address {active_addr} was removed, reconnecting");
                self.capture.drop_transport(handle);
            }
        }
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
            // set_hostname cleared active_addr because the resolved endpoint
            // may be different now. A live transport to the old endpoint must
            // not be presented as the new one: drop it and let the reconnect
            // loop dial the new DNS result.
            if self.client_manager.is_active(handle) {
                self.capture.drop_transport(handle);
            }
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        let changed = self.client_manager.get_port(handle) != Some(port);
        self.client_manager.set_port(handle, port);
        if changed && self.client_manager.is_active(handle) {
            // The live transport still talks to the old port while
            // set_port() rewrites active_addr to the new one; reconnect.
            self.capture.drop_transport(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if !self.client_manager.set_pos(handle, pos) {
            self.broadcast_client(handle);
            return;
        }
        // Only the OUTGOING capture barrier follows this client position.
        //
        // A live incoming session's entry edge is decided by the peer
        // (ProtoEvent::Enter carries the peer's own configuration). Moving the
        // incoming capture zone from a local position edit broke the return
        // hand-off: the injected pointer then arrived through an edge with no
        // matching zone, so the cursor stayed trapped on this machine.
        // Move the outgoing capture barrier in place. The transport must stay
        // up: deactivating/reactivating the client cleared active_addr and the
        // UI fell back to "waiting approval" even though the link was alive.
        if self.client_manager.is_active(handle) {
            if let Some(other) = self.client_manager.client_at(pos) {
                if other != handle {
                    let same_peer = matches!(
                        (
                            self.client_manager.fingerprint(handle),
                            self.client_manager.fingerprint(other),
                        ),
                        (Some(a), Some(b)) if a == b
                    );
                    if same_peer {
                        log::warn!("not deactivating {other}: it is the same peer as {handle}");
                    } else {
                        self.deactivate_client(other);
                    }
                }
            }
            self.capture.destroy(handle);
            self.capture
                .create(handle, pos, crate::capture::CaptureType::Default);
            self.capture.set_sensitivity(
                handle,
                self.client_manager.get_sensitivity(handle) as f64 / 100.0,
            );
            self.capture.connect(handle);
        }
        self.broadcast_client(handle);
    }

    fn set_absolute_sensitivity(&mut self, percent: u16) {
        let percent = percent.clamp(10, 500);
        self.config.set_absolute_pointer_sensitivity(percent);
        self.emulation.set_absolute_sensitivity(percent);
        self.notify_frontend(FrontendEvent::AbsoluteSensitivity(percent));
    }

    fn update_sensitivity(&mut self, handle: ClientHandle, sensitivity: u16) {
        let sensitivity = sensitivity.clamp(10, 500);
        if self.client_manager.set_sensitivity(handle, sensitivity) {
            self.capture
                .set_sensitivity(handle, sensitivity as f64 / 100.0);
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
            // The hook is a shell command line. Windows has no sh(1), so use
            // the command interpreter there instead of silently failing.
            #[cfg(windows)]
            let mut command = {
                let mut command = Command::new("cmd");
                command.args(["/C", cmd.as_str()]);
                command
            };
            #[cfg(not(windows))]
            let mut command = {
                let mut command = Command::new("sh");
                command.arg("-c").arg(cmd.as_str());
                command
            };
            let mut child = match command.spawn() {
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

/// Whether an existing client entry may be reused for an incoming peer.
///
/// The certificate fingerprint is the identity. An entry with a pinned
/// fingerprint is only compatible with the same fingerprint; an entry with no
/// pin (legacy address-only configuration) may adopt the incoming peer, which
/// is how the first connection pins the client.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn reverse_client_compatible(incoming: Option<&str>, pinned: Option<&str>) -> bool {
    match (incoming, pinned) {
        (_, None) => true,
        (Some(incoming), Some(pinned)) => incoming == pinned,
        (None, Some(_)) => false,
    }
}

/// Resolves when the process receives SIGTERM (systemd `stop`, desktop
/// logout, service manager). `None` means the handler could not be installed;
/// the future then never completes so the select arm is inert.
#[cfg(unix)]
async fn wait_for_sigterm(signal: &mut Option<tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// How often an unanswered return hand-off request is retried.
const RETURN_RETRY_INTERVAL: Duration = Duration::from_millis(300);
/// Maximum number of retransmissions per hand-off episode. A peer that is
/// alive but never confirms (legacy build) must not be spammed forever.
const RETURN_MAX_ATTEMPTS: u32 = 8;

/// Return hand-offs waiting for the peer's `Leave` confirmation.
///
/// DTLS application data is not retransmitted, so one lost `Leave` used to
/// latch the peer's remote-control suppression until the next user movement
/// (cursor stuck on the peer's screen). The request is retried with a non-zero
/// serial; the peer re-confirms when it receives a retried request, and the
/// confirmation (`PeerLeft`) clears the entry.
#[derive(Debug, Default)]
struct ReturnHandoff {
    pending: HashMap<SocketAddr, HandoffRequest>,
    /// Sessions whose request exhausted its retry budget. The service closes
    /// the incoming session for each exactly once (drained by
    /// [`ReturnHandoff::take_exhausted`]).
    exhausted: Vec<SocketAddr>,
}

#[derive(Debug)]
struct HandoffRequest {
    serial: u32,
    attempts: u32,
    last_sent: Instant,
}

impl ReturnHandoff {
    /// Record a (re)newed request for `addr`. A retried request keeps the same
    /// serial; a new episode (new capture crossing) uses a new one.
    fn request(&mut self, addr: SocketAddr, serial: u32, now: Instant) {
        match self.pending.get_mut(&addr) {
            Some(existing) => {
                if existing.serial == serial {
                    // Same episode: another direct send counts against the
                    // retry budget, so a peer that never confirms cannot be
                    // spammed forever.
                    existing.attempts = existing.attempts.saturating_add(1);
                } else {
                    // A new crossing is a new episode: give it a fresh retry
                    // budget instead of inheriting the previous one's near
                    // exhaustion (worst case one direct send + one retry).
                    existing.serial = serial;
                    existing.attempts = 1;
                }
                existing.last_sent = now;
            }
            None => {
                self.pending.insert(
                    addr,
                    HandoffRequest {
                        serial,
                        attempts: 1,
                        last_sent: now,
                    },
                );
            }
        }
    }

    /// The peer confirmed the release (or its session ended).
    fn confirmed(&mut self, addr: SocketAddr) {
        self.pending.remove(&addr);
        self.exhausted.retain(|candidate| *candidate != addr);
    }

    /// Drop the state for a session that is gone.
    fn forget(&mut self, addr: SocketAddr) {
        self.pending.remove(&addr);
        self.exhausted.retain(|candidate| *candidate != addr);
    }

    /// Requests that are due for a retry. Exhausted ones are dropped from
    /// `pending` and reported through [`Self::take_exhausted`] so the caller
    /// can close their session.
    fn due(&mut self, now: Instant) -> Vec<(SocketAddr, u32)> {
        let mut due = Vec::new();
        let Self { pending, exhausted } = self;
        pending.retain(|addr, request| {
            if request.attempts >= RETURN_MAX_ATTEMPTS {
                exhausted.push(*addr);
                return false;
            }
            if now.duration_since(request.last_sent) >= RETURN_RETRY_INTERVAL {
                request.attempts = request.attempts.saturating_add(1);
                request.last_sent = now;
                due.push((*addr, request.serial));
            }
            true
        });
        due
    }

    /// Sessions that exhausted their retry budget since the last call.
    fn take_exhausted(&mut self) -> Vec<SocketAddr> {
        std::mem::take(&mut self.exhausted)
    }

    #[cfg(test)]
    fn is_pending(&self, addr: SocketAddr) -> bool {
        self.pending.contains_key(&addr)
    }
}

/// Convert a persisted audit record into the IPC history entry.
fn history_entry_from_record(record: AuditRecord) -> HistoryEntry {
    HistoryEntry {
        ts: record.ts,
        event: record.event,
        fingerprint: record.fingerprint,
        name: record.name,
        addr: record.addr,
        detail: record.detail,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RETURN_MAX_ATTEMPTS, RETURN_RETRY_INTERVAL, ReturnHandoff, reverse_client_compatible,
    };
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Instant;

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, last)), 4242)
    }

    #[test]
    fn a_confirmed_handoff_is_forgotten() {
        let mut handoff = ReturnHandoff::default();
        let now = Instant::now();
        handoff.request(addr(1), 7, now);
        assert!(handoff.is_pending(addr(1)));
        handoff.confirmed(addr(1));
        assert!(!handoff.is_pending(addr(1)));
        assert!(handoff.due(now + RETURN_RETRY_INTERVAL).is_empty());
    }

    #[test]
    fn a_handoff_is_retried_only_after_the_interval() {
        let mut handoff = ReturnHandoff::default();
        let now = Instant::now();
        handoff.request(addr(1), 7, now);
        assert!(
            handoff.due(now).is_empty(),
            "the request was just sent directly; no immediate retry"
        );
        assert!(
            handoff.due(now + RETURN_RETRY_INTERVAL / 2).is_empty(),
            "not due yet"
        );
        assert_eq!(
            handoff.due(now + RETURN_RETRY_INTERVAL),
            vec![(addr(1), 7)],
            "the same serial is retried (the peer re-confirms on a retried request)"
        );
    }

    #[test]
    fn handoff_retries_are_bounded() {
        let mut handoff = ReturnHandoff::default();
        let mut now = Instant::now();
        handoff.request(addr(1), 1, now);
        let mut retries = 0;
        for _ in 0..100 {
            now += RETURN_RETRY_INTERVAL;
            retries += handoff.due(now).len();
        }
        assert_eq!(
            retries as u32,
            RETURN_MAX_ATTEMPTS - 1,
            "the first send is not a retry; retries must stop after the cap"
        );
        assert!(
            !handoff.is_pending(addr(1)),
            "an exhausted hand-off is dropped"
        );
    }

    /// Exhaustion must be reported to the service exactly once: it closes the
    /// incoming session (the only safe way to clear a latched remote-control
    /// suppression without risking a feedback loop).
    #[test]
    fn exhaustion_is_reported_exactly_once() {
        let mut handoff = ReturnHandoff::default();
        let mut now = Instant::now();
        handoff.request(addr(1), 1, now);
        for _ in 0..100 {
            now += RETURN_RETRY_INTERVAL;
            let _ = handoff.due(now);
        }
        assert_eq!(
            handoff.take_exhausted(),
            vec![addr(1)],
            "the session cleanup must run exactly once"
        );
        assert!(
            handoff.take_exhausted().is_empty(),
            "a later tick must not clean the same session again"
        );
        assert!(!handoff.is_pending(addr(1)));
    }

    /// A new crossing (new serial) gets a fresh retry budget instead of the
    /// leftovers of the previous episode.
    #[test]
    fn a_new_crossing_resets_the_retry_budget() {
        let mut handoff = ReturnHandoff::default();
        let mut now = Instant::now();
        handoff.request(addr(1), 1, now);
        // Spend almost the whole budget on the first episode.
        for _ in 0..(RETURN_MAX_ATTEMPTS - 2) {
            now += RETURN_RETRY_INTERVAL;
            let _ = handoff.due(now);
        }
        handoff.request(addr(1), 2, now);
        let mut retries = 0;
        for _ in 0..100 {
            now += RETURN_RETRY_INTERVAL;
            retries += handoff.due(now).len();
        }
        assert_eq!(
            retries as u32,
            RETURN_MAX_ATTEMPTS - 1,
            "the new episode must be retried for the full budget"
        );
    }

    #[test]
    fn a_later_crossing_starts_a_new_episode() {
        let mut handoff = ReturnHandoff::default();
        let mut now = Instant::now();
        handoff.request(addr(1), 1, now);
        for _ in 0..100 {
            now += RETURN_RETRY_INTERVAL;
            let _ = handoff.due(now);
        }
        // The user crosses again: a fresh request must be retried again.
        handoff.request(addr(1), 2, now);
        assert_eq!(handoff.due(now + RETURN_RETRY_INTERVAL), vec![(addr(1), 2)]);
    }

    #[test]
    fn handoffs_are_tracked_per_session() {
        let mut handoff = ReturnHandoff::default();
        let now = Instant::now();
        handoff.request(addr(1), 1, now);
        handoff.request(addr(2), 2, now);
        handoff.confirmed(addr(1));
        let mut due = handoff.due(now + RETURN_RETRY_INTERVAL);
        due.sort_by_key(|(addr, _)| addr.port());
        assert_eq!(
            due,
            vec![(addr(2), 2)],
            "only the unconfirmed session retries"
        );
    }

    #[test]
    fn an_unpinned_legacy_client_can_adopt_a_peer() {
        // Configured by address only: the first handshake pins the identity.
        assert!(reverse_client_compatible(Some("aa"), None));
        assert!(reverse_client_compatible(None, None));
    }

    #[test]
    fn a_pinned_client_only_matches_the_same_certificate() {
        assert!(reverse_client_compatible(Some("aa"), Some("aa")));
        assert!(!reverse_client_compatible(Some("bb"), Some("aa")));
        // Never replace a pin with a peer that somehow lost its fingerprint.
        assert!(!reverse_client_compatible(None, Some("aa")));
    }
}
