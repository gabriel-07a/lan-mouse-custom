use futures::{StreamExt, future};
use std::{
    collections::HashSet,
    io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions},
};
use async_trait::async_trait;

use reis::{
    ei::{
        self, Button, Keyboard, Pointer, PointerAbsolute, Scroll, button::ButtonState,
        handshake::ContextType, keyboard::KeyState,
    },
    event::{self, Connection, DeviceCapability, DeviceEvent, EiEvent, Region, SeatEvent},
    tokio::EiConvertEventStream,
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::{
    error::EmulationError,
    portal_token::{REMOTE_DESKTOP_TOKEN_FILE, read_named_token, write_named_token},
};

use super::{
    EdgeReturnNotifier, Emulation, EmulationHandle, PointerEdge, error::LibeiEmulationCreationError,
};

/// Absolute pointer device advertised by the compositor, with the EIS region
/// that defines our coordinate space.
#[derive(Clone)]
struct AbsolutePointer {
    device: ei::Device,
    pointer: PointerAbsolute,
    /// changes whenever the compositor adds/replaces the absolute device
    generation: u64,
    /// device regions in logical pixels (x0, y0, x1, y1)
    regions: Vec<(f32, f32, f32, f32)>,
}

#[derive(Clone, Default)]
struct Devices {
    pointer: Arc<RwLock<Option<(ei::Device, ei::Pointer)>>>,
    /// set when the compositor exposes an absolute pointer device (libei
    /// PI_ABSOLUTE): lets us position the pointer exactly and detect the
    /// return hand-off at the entry edge
    absolute: Arc<RwLock<Option<AbsolutePointer>>>,
    absolute_generation: Arc<AtomicU64>,
    /// monotonically increasing sequence for ei_device.start_emulating
    resume_sequence: Arc<AtomicU64>,
    /// EIS devices the compositor paused; absolute events must not be sent then
    paused: Arc<RwLock<HashSet<ei::Device>>>,
    scroll: Arc<RwLock<Option<(ei::Device, ei::Scroll)>>>,
    button: Arc<RwLock<Option<(ei::Device, ei::Button)>>>,
    keyboard: Arc<RwLock<Option<(ei::Device, ei::Keyboard)>>>,
}

impl Devices {
    /// Forget every interface belonging to a device the compositor removed.
    ///
    /// The EIS objects are destroyed with the device; sending events on a
    /// destroyed object id is a protocol violation and can tear down the whole
    /// EIS connection (all input emulation then fails). Fresh interfaces arrive
    /// with the next `DeviceAdded`.
    fn forget_device(&self, device: &ei::Device) {
        fn clear<T>(slot: &RwLock<Option<(ei::Device, T)>>, device: &ei::Device) {
            let mut slot = slot.write().unwrap_or_else(|e| e.into_inner());
            // The high-level Device wrapper is keyed by the ei_device object,
            // so equality is object identity.
            if slot.as_ref().is_some_and(|(stored, _)| stored == device) {
                *slot = None;
            }
        }
        clear(&self.pointer, device);
        clear(&self.scroll, device);
        clear(&self.button, device);
        clear(&self.keyboard, device);
        self.paused
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(device);
        {
            let mut absolute = self
                .absolute
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if absolute
                .as_ref()
                .is_some_and(|stored| &stored.device == device)
            {
                *absolute = None;
            }
        }
    }
}

pub(crate) struct LibeiEmulation {
    context: ei::Context,
    conn: event::Connection,
    devices: Devices,
    ei_task: JoinHandle<()>,
    error: Arc<Mutex<Option<EmulationError>>>,
    libei_error: Arc<AtomicBool>,
    _remote_desktop: RemoteDesktop,
    session: Session<RemoteDesktop>,
    /// edge the peer entered through, while it drives this machine
    remote_edge: Option<PointerEdge>,
    /// normalised crossing coordinate along `remote_edge`, when the peer sent
    /// an `EnterAt`; `None` keeps the legacy edge-centre anchor
    entry_along: Option<f32>,
    /// exact absolute-position tracker once armed
    absolute: Option<AbsoluteTracker>,
    /// service notifier for the return hand-off
    edge_return: Option<EdgeReturnNotifier>,
    /// warning about a missing absolute pointer is logged once per activation
    absolute_warned: bool,
    /// monotonic clock (EI frame timestamps must use CLOCK_MONOTONIC)
    clock: Instant,
    /// user sensitivity multiplier for the absolute pointer (1.0 = 100%)
    absolute_sensitivity: f64,
    /// receiver pointer acceleration settings mirrored for absolute injection
    pointer_settings: PointerSettings,
    /// previous motion timestamp, used to estimate the pointer velocity
    last_motion_time: Option<u32>,
    /// local arrival time of the previous motion event (backends that do not
    /// populate the wire timestamp, e.g. Windows, report time = 0)
    last_motion_arrival: Option<Instant>,
    /// temporary LAN_MOUSE_ABSOLUTE_SCALE override, applied on every gain update
    absolute_env_scale: f64,
}

/// Pixels the pointer must be pushed past the entry edge before the hand-off
/// fires. The absolute tracker knows the exact position, so this can be tiny.
const RETURN_MARGIN_PX: f64 = 3.0;

/// Pointer settings of this Wayland session.
///
/// The relative injection path is accelerated by libinput; absolute injection
/// is linear unless we mirror that acceleration. Mirroring it makes the
/// absolute pointer feel like the previous relative path on any screen
/// resolution or scale, because both apply the same velocity-dependent curve.
#[derive(Clone, Copy, Debug)]
struct PointerSettings {
    /// GNOME mouse speed setting (-1.0 ..= 1.0)
    speed: f64,
    /// adaptive acceleration (default) or flat (speed only)
    adaptive: bool,
}

impl PointerSettings {
    fn detect() -> Self {
        // The local mouse speed setting is about *this* machine's mouse and
        // must not slow down input received from another machine. Only the
        // acceleration profile is mirrored (adaptive adds a curve, flat keeps
        // the mapping linear), so 100% sensitivity is exactly 1:1.
        let profile = read_gsettings("org.gnome.desktop.peripherals.mouse", "accel-profile")
            .unwrap_or_else(|| "adaptive".to_string());
        let adaptive = !profile.trim_matches('\'').eq_ignore_ascii_case("flat");
        Self {
            speed: 0.0,
            adaptive,
        }
    }

    async fn detect_async() -> Self {
        tokio::task::spawn_blocking(Self::detect)
            .await
            .unwrap_or(Self {
                speed: 0.0,
                adaptive: true,
            })
    }

    /// Velocity-dependent acceleration factor, mirroring libinput's adaptive
    /// pointer acceleration (slow movements stay 1:1 so fine control works,
    /// fast movements get the same boost the relative path receives).
    fn acceleration(&self, velocity: f64) -> f64 {
        let base = 2f64.powf(self.speed).clamp(0.3, 3.0);
        if !self.adaptive {
            return base;
        }
        // counts/millisecond; typical mice: slow ~0.05, fast ~2
        const LOW: f64 = 0.10;
        const HIGH: f64 = 2.0;
        const MAX_BOOST: f64 = 2.2;
        let ramp = ((velocity - LOW) / (HIGH - LOW)).clamp(0.0, 1.0);
        base * (1.0 + (MAX_BOOST - 1.0) * ramp * ramp)
    }
}

fn read_gsettings(schema: &str, key: &str) -> Option<String> {
    let output = std::process::Command::new("gsettings")
        .args(["get", schema, key])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Effective delta multiplier: monitor normalisation * user sensitivity *
/// environment override. Shared by arming and live updates so a live change
/// can never drop the env override.
fn effective_delta_scale(base_scale: f64, sensitivity: f64, env_scale: f64) -> f64 {
    base_scale * sensitivity * env_scale
}

/// Convert a raw timestamp difference into milliseconds.
///
/// Windows sends milliseconds, the Linux/libei capture sends microseconds;
/// values >= 1000 are treated as microseconds.
fn dt_from_raw(previous: Option<u32>, time: u32) -> Option<f64> {
    previous
        .map(|previous| time.wrapping_sub(previous))
        .map(|raw| {
            if raw >= 1000 {
                raw as f64 / 1000.0
            } else {
                raw as f64
            }
        })
        .filter(|dt| dt.is_finite() && *dt > 0.0)
        // Real gaps (paused mouse, heavy frame) keep their duration instead of
        // getting a fake 8 ms interval (which would boost right after a pause).
        .map(|dt| dt.clamp(0.5, 100.0))
}

/// Temporary sensitivity override from the environment (1.0 = none).
fn read_env_scale() -> f64 {
    std::env::var("LAN_MOUSE_ABSOLUTE_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.05 && *value < 20.0)
        .unwrap_or(1.0)
}

/// Distance (logical px) the cursor starts inside the entry edge. Big enough
/// that hand-off jitter cannot fire immediately, small enough that the cursor
/// visibly appears on the side the peer came from.
const EDGE_ANCHOR_INSET: f64 = 24.0;

/// Largest per-event delta accepted from the wire. Bigger values are treated
/// as corrupted packets: one bogus delta must not jump the cursor across the
/// screen and trigger a false hand-off.
const MAX_DELTA_PX: f64 = 4096.0;

/// Exact edge-return detector for absolute pointer injection.
///
/// The pointer is warped to a known anchor on entry and every injected motion
/// delta updates the tracked position. Because the same position is sent to
/// the compositor as an absolute motion event, the tracked position *is* the
/// real cursor position: crossing the entry edge is unambiguous.
///
/// The tracked position lives in the EIS layout coordinate space (all monitor
/// regions share it), so the pointer can roam across monitors; only the entry
/// edge of the region that was entered triggers the hand-off.
#[derive(Clone, Debug)]
struct AbsoluteTracker {
    edge: PointerEdge,
    /// absolute device generation this tracker was armed against
    generation: u64,
    /// monitor-normalisation * user sensitivity; multiplied into deltas
    delta_scale: f64,
    /// monitor normalisation only (1/display-scale); kept so a live
    /// sensitivity change can recompute delta_scale without re-arming
    base_scale: f64,
    /// monitor regions in logical pixels: (x0, y0, x1, y1), half-open
    regions: Vec<(f32, f32, f32, f32)>,
    /// region touching the entry edge (anchor + return threshold)
    entry: (f32, f32, f32, f32),
    layout_min_x: f32,
    layout_min_y: f32,
    layout_max_x: f32,
    layout_max_y: f32,
    x: f64,
    y: f64,
    fired: bool,
}

impl AbsoluteTracker {
    #[cfg_attr(not(test), allow(dead_code))]
    fn new(
        edge: PointerEdge,
        generation: u64,
        regions: Vec<(f32, f32, f32, f32)>,
        base_scale: f64,
        gain: f64,
    ) -> Option<Self> {
        Self::new_with_entry(edge, generation, regions, base_scale, gain, None)
    }

    /// Arm the tracker, optionally at the exact normalised crossing coordinate
    /// (`along`). `None` keeps the legacy edge-centre anchor used with peers
    /// that do not send `EnterAt`.
    fn new_with_entry(
        edge: PointerEdge,
        generation: u64,
        regions: Vec<(f32, f32, f32, f32)>,
        base_scale: f64,
        gain: f64,
        along: Option<f32>,
    ) -> Option<Self> {
        let layout_min_x = regions.iter().map(|r| r.0).fold(f32::INFINITY, f32::min);
        let layout_min_y = regions.iter().map(|r| r.1).fold(f32::INFINITY, f32::min);
        let layout_max_x = regions
            .iter()
            .map(|r| r.2)
            .fold(f32::NEG_INFINITY, f32::max);
        let layout_max_y = regions
            .iter()
            .map(|r| r.3)
            .fold(f32::NEG_INFINITY, f32::max);
        let (entry, x, y) = match along {
            Some(along) => entry_point_at(
                &regions,
                edge,
                along,
                (layout_min_x, layout_min_y, layout_max_x, layout_max_y),
            )?,
            None => {
                let entry = entry_region_bounds(&regions, edge)?;
                let inset_x = EDGE_ANCHOR_INSET.min(f64::from((entry.2 - entry.0) / 2.0));
                let inset_y = EDGE_ANCHOR_INSET.min(f64::from((entry.3 - entry.1) / 2.0));
                let x = match edge {
                    PointerEdge::Left => f64::from(entry.0) + inset_x,
                    PointerEdge::Right => f64::from(entry.2) - 1.0 - inset_x,
                    _ => f64::from(entry.0 + (entry.2 - entry.0) / 2.0),
                };
                let y = match edge {
                    PointerEdge::Top => f64::from(entry.1) + inset_y,
                    PointerEdge::Bottom => f64::from(entry.3) - 1.0 - inset_y,
                    _ => f64::from(entry.1 + (entry.3 - entry.1) / 2.0),
                };
                (entry, x, y)
            }
        };
        Some(Self {
            edge,
            generation,
            delta_scale: base_scale * gain,
            base_scale,
            regions,
            entry,
            layout_min_x,
            layout_min_y,
            layout_max_x,
            layout_max_y,
            x,
            y,
            fired: false,
        })
    }

    /// The entry region bounds (return threshold).
    fn entry_bounds(&self) -> (f32, f32, f32, f32) {
        self.entry
    }

    /// Clamp a real cursor position into the layout (half-open region bounds).
    fn clamped(&self, x: f64, y: f64) -> (f64, f64) {
        let max_x = f64::from(self.layout_max_x) - 1.0;
        let max_y = f64::from(self.layout_max_y) - 1.0;
        (
            x.clamp(f64::from(self.layout_min_x), max_x),
            y.clamp(f64::from(self.layout_min_y), max_y),
        )
    }

    /// Apply one injected delta and report the hand-off edge once.
    fn apply(&mut self, dx: f64, dy: f64) -> Option<PointerEdge> {
        if !dx.is_finite() || !dy.is_finite() {
            return None;
        }
        // Corrupted single deltas would jump the tracked cursor anywhere; drop
        // them instead of clamping (a clamp could still fire the hand-off).
        // Leave room for the mirrored boost: speed +1 doubles the base and the
        // curve adds up to 2.2x, so valid values can reach 4.4x the wire limit.
        if dx.abs() > MAX_DELTA_PX * 5.0 || dy.abs() > MAX_DELTA_PX * 5.0 {
            log::debug!("dropping implausible absolute delta ({dx}, {dy})");
            return None;
        }
        // HiDPI: the EIS region is in logical pixels while the peer's deltas
        // are physical-ish; dividing by the monitor scale keeps the physical
        // cursor speed identical to the previous relative injection.
        let (dx, dy) = (dx * self.delta_scale, dy * self.delta_scale);
        let raw_x = self.x + dx;
        let raw_y = self.y + dy;
        // The compositor clamps the real cursor into the layout; mirror that so
        // overshooting an edge and coming back behaves correctly.
        let (clamped_x, clamped_y) = self.clamped(raw_x, raw_y);
        // The position that will actually be sent: snapped into a real region.
        // Keeping the tracked position equal to the sent one prevents a desync
        // when the cursor is pushed into a gap between monitors.
        let (x, y) = self.snap(clamped_x, clamped_y);

        if self.fired {
            self.x = x;
            self.y = y;
            // Re-arm when the pointer is well inside the entry screen area.
            let inset = 20.0;
            if self.x > f64::from(self.entry.0) + inset
                && self.x < f64::from(self.entry.2) - inset
                && self.y > f64::from(self.entry.1) + inset
                && self.y < f64::from(self.entry.3) - inset
            {
                self.fired = false;
            }
            return None;
        }

        // The hand-off requires BOTH: the real cursor reached the entry edge
        // (snapped position in the edge band) AND the user's motion pushed past
        // it. The band check prevents a false fire when the cursor is pinned in
        // a hole between monitors while the invisible push overruns the layout.
        let (x0, y0, x1, y1) = self.entry;
        let near_edge = match self.edge {
            PointerEdge::Left => x <= f64::from(x0) + 1.0,
            PointerEdge::Right => x >= f64::from(x1) - 2.0,
            PointerEdge::Top => y <= f64::from(y0) + 1.0,
            PointerEdge::Bottom => y >= f64::from(y1) - 2.0,
        };
        let past_edge = match self.edge {
            PointerEdge::Left => raw_x < f64::from(x0) - RETURN_MARGIN_PX,
            PointerEdge::Right => raw_x > f64::from(x1) + RETURN_MARGIN_PX,
            PointerEdge::Top => raw_y < f64::from(y0) - RETURN_MARGIN_PX,
            PointerEdge::Bottom => raw_y > f64::from(y1) + RETURN_MARGIN_PX,
        };
        self.x = x;
        self.y = y;
        if near_edge && past_edge {
            self.fired = true;
            // Snap to the edge so reversing the push moves the cursor again
            // immediately (the overrun distance must not be swallowed).
            match self.edge {
                PointerEdge::Left => self.x = f64::from(x0),
                PointerEdge::Right => self.x = f64::from(x1) - 1.0,
                PointerEdge::Top => self.y = f64::from(y0),
                PointerEdge::Bottom => self.y = f64::from(y1) - 1.0,
            }
            return Some(self.edge);
        }
        None
    }

    /// The position to send to the compositor.
    ///
    /// Absolute events are only valid inside a region (half-open bounds); a
    /// position in a gap between monitors is clamped into the nearest region.
    fn absolute(&self) -> (f32, f32) {
        (self.x as f32, self.y as f32)
    }

    /// Normalised position along the entry edge where the pointer currently
    /// is. Used to let the host restore its cursor at the return coordinate.
    fn return_along(&self) -> Option<f32> {
        let (x0, y0, x1, y1) = self.entry;
        let along = match self.edge {
            PointerEdge::Left | PointerEdge::Right => {
                let span = f64::from((y1 - y0).max(1.0)) - 1.0;
                ((self.y - f64::from(y0)) / span).clamp(0.0, 1.0)
            }
            PointerEdge::Top | PointerEdge::Bottom => {
                let span = f64::from((x1 - x0).max(1.0)) - 1.0;
                ((self.x - f64::from(x0)) / span).clamp(0.0, 1.0)
            }
        };
        along.is_finite().then_some(along as f32)
    }

    /// Snap a layout position into the region that contains it, or the
    /// nearest one (used both for detection bookkeeping and the sent value).
    fn snap(&self, x: f64, y: f64) -> (f64, f64) {
        for region in &self.regions {
            let (x0, y0, x1, y1) = *region;
            let max_x = f64::from(x1) - 1.0;
            let max_y = f64::from(y1) - 1.0;
            if x >= f64::from(x0) && x <= max_x && y >= f64::from(y0) && y <= max_y {
                return (x, y);
            }
        }
        let mut best: Option<((f64, f64), f64)> = None;
        for region in &self.regions {
            let (x0, y0, x1, y1) = *region;
            let cx = x.clamp(f64::from(x0), f64::from(x1) - 1.0);
            let cy = y.clamp(f64::from(y0), f64::from(y1) - 1.0);
            let distance = (cx - x) * (cx - x) + (cy - y) * (cy - y);
            if best
                .as_ref()
                .is_none_or(|(_, best_distance)| distance < *best_distance)
            {
                best = Some(((cx, cy), distance));
            }
        }
        best.map(|(position, _)| position)
            .unwrap_or((f64::from(self.layout_min_x), f64::from(self.layout_min_y)))
    }
}

/// Convert EIS regions into (x0, y0, x1, y1) logical-pixel boxes.
/// Convert EIS regions into (x0, y0, x1, y1) logical-pixel boxes.
fn region_boxes(regions: &[Region]) -> Vec<(f32, f32, f32, f32)> {
    regions
        .iter()
        .filter(|region| region.width > 0 && region.height > 0)
        .map(|region| {
            (
                region.x as f32,
                region.y as f32,
                region.x.saturating_add(region.width) as f32,
                region.y.saturating_add(region.height) as f32,
            )
        })
        .collect()
}

/// Pick the region that touches the entry edge.
///
/// A union bounding box would include holes between non-contiguous monitors,
/// where the compositor silently drops absolute events. Anchoring and clamping
/// inside a single real region keeps the tracked position valid.
fn entry_region_bounds(
    regions: &[(f32, f32, f32, f32)],
    edge: PointerEdge,
) -> Option<(f32, f32, f32, f32)> {
    regions.iter().copied().min_by(|a, b| {
        let key = |r: &(f32, f32, f32, f32)| match edge {
            PointerEdge::Left => r.0,
            PointerEdge::Right => -r.2,
            PointerEdge::Top => r.1,
            PointerEdge::Bottom => -r.3,
        };
        key(a).total_cmp(&key(b))
    })
}

/// Pick the monitor region whose entry-edge segment contains the normalised
/// coordinate `along` and return its bounds plus the exact edge pixel.
///
/// Multi-monitor layouts are ordered along the edge; a fraction that lands in a
/// gap snaps to the nearest segment. `along` is clamped to `0..=1`.
type EdgeEntry = ((f32, f32, f32, f32), f64, f64);

fn entry_point_at(
    regions: &[(f32, f32, f32, f32)],
    edge: PointerEdge,
    along: f32,
    layout: (f32, f32, f32, f32),
) -> Option<EdgeEntry> {
    let (min_x, min_y, max_x, max_y) = layout;
    let along = f64::from(along.clamp(0.0, 1.0));
    let (span_min, span_max) = match edge {
        PointerEdge::Left | PointerEdge::Right => (min_y, max_y),
        PointerEdge::Top | PointerEdge::Bottom => (min_x, max_x),
    };
    if span_max <= span_min {
        return None;
    }
    let point = f64::from(span_min) + along * f64::from(span_max - span_min);
    let touches = |r: &(f32, f32, f32, f32)| match edge {
        PointerEdge::Left => r.0 <= min_x + 0.5,
        PointerEdge::Right => r.2 >= max_x - 0.5,
        PointerEdge::Top => r.1 <= min_y + 0.5,
        PointerEdge::Bottom => r.3 >= max_y - 0.5,
    };
    let mut best: Option<(EdgeEntry, f64)> = None;
    for r in regions.iter().copied().filter(|r| touches(r)) {
        let (lo, hi) = match edge {
            PointerEdge::Left | PointerEdge::Right => (r.1, r.3),
            PointerEdge::Top | PointerEdge::Bottom => (r.0, r.2),
        };
        let max_along = f64::from(hi - 1.0).max(f64::from(lo));
        let clamped = point.clamp(f64::from(lo), max_along);
        let distance = (clamped - point).abs();
        let (x, y) = match edge {
            PointerEdge::Left => (f64::from(r.0), clamped),
            PointerEdge::Right => (f64::from(r.2) - 1.0, clamped),
            PointerEdge::Top => (clamped, f64::from(r.1)),
            PointerEdge::Bottom => (clamped, f64::from(r.3) - 1.0),
        };
        if best
            .as_ref()
            .is_none_or(|(_, best_distance)| distance < *best_distance)
        {
            best = Some(((r, x, y), distance));
        }
    }
    best.map(|(entry, _)| entry)
}

async fn get_ei_fd() -> Result<(RemoteDesktop, Session<RemoteDesktop>, OwnedFd), ashpd::Error> {
    let remote_desktop = RemoteDesktop::new().await?;

    let restore_token = read_named_token(REMOTE_DESKTOP_TOKEN_FILE);
    log::debug!(
        "RemoteDesktop restore token: {}",
        if restore_token.is_some() {
            "present (the portal should skip its dialog)"
        } else {
            "missing (the portal will ask for permission)"
        }
    );

    log::debug!("creating session ...");
    let session = remote_desktop.create_session(Default::default()).await?;

    log::debug!("selecting devices ...");
    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token.as_deref());
    remote_desktop.select_devices(&session, options).await?;

    log::info!("requesting permission for input emulation");
    let start_response = remote_desktop
        .start(&session, None, Default::default())
        .await?
        .response()?;

    // The restore token is only valid once, we need to re-save it each time
    if let Some(token_str) = start_response.restore_token() {
        if let Err(e) = write_named_token(REMOTE_DESKTOP_TOKEN_FILE, token_str) {
            log::warn!("failed to save RemoteDesktop token: {}", e);
        }
    }

    let fd = remote_desktop
        .connect_to_eis(&session, Default::default())
        .await?;
    Ok((remote_desktop, session, fd))
}

impl LibeiEmulation {
    /// Warp the pointer to the centre of the EIS region and start tracking the
    /// injected deltas from that known anchor.
    ///
    /// With absolute injection the tracked position is the real cursor
    /// position, so crossing the entry edge can be detected exactly - this is
    /// what makes the automatic return hand-off work on Wayland, where the
    /// compositor does not report capture barriers for injected motion.
    fn start_absolute_tracking(&mut self) -> bool {
        let Some(edge) = self.remote_edge else {
            return false;
        };
        let absolute = self
            .devices
            .absolute
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(pointer) = absolute else {
            if !self.absolute_warned {
                self.absolute_warned = true;
                log::warn!(
                    "peer drives this machine through {edge:?} but the compositor has no absolute pointer; automatic edge return is disabled - use the release bind to take control back"
                );
            }
            return false;
        };
        let paused = self
            .devices
            .paused
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&pointer.device);
        if paused {
            log::debug!("absolute pointer paused by the compositor; waiting for resume");
            return false;
        }
        // Screen-relative mapping: one pointer count moves one logical pixel
        // (the fraction of the screen traversed matches the controller,
        // independent of resolution and monitor scale). The mirrored libinput
        // acceleration adds the velocity-dependent boost the relative path had.
        let base_scale = 1.0;
        // user override for fine tuning without recompiling
        let gain = self.absolute_sensitivity * self.absolute_env_scale;
        let delta_scale = effective_delta_scale(
            base_scale,
            self.absolute_sensitivity,
            self.absolute_env_scale,
        );
        let Some(tracker) = AbsoluteTracker::new_with_entry(
            edge,
            pointer.generation,
            pointer.regions.clone(),
            base_scale,
            gain,
            self.entry_along,
        ) else {
            log::warn!("absolute pointer has no usable region; automatic edge return disabled");
            return false;
        };
        let (x, y) = tracker.absolute();
        pointer.pointer.motion_absolute(x, y);
        pointer.device.frame(self.conn.serial(), self.now_micros());
        if let Err(e) = self.context.flush() {
            log::warn!("failed to flush the absolute pointer warp: {e}");
        }
        let (min_x, min_y, max_x, max_y) = tracker.entry_bounds();
        log::info!(
            "absolute pointer armed at ({x},{y}) region {min_x},{min_y}..{max_x},{max_y} gain {delta_scale:.2}x (accel mirrored); push the cursor past the {edge:?} edge to hand control back"
        );
        self.absolute = Some(tracker);
        true
    }

    /// Monotonic microseconds for EI frame timestamps.
    fn now_micros(&self) -> u64 {
        self.clock.elapsed().as_micros() as u64
    }

    /// Milliseconds since the previous motion event (clamped); 8 ms fallback.
    ///
    /// Capture backends do not agree on the timestamp unit: Windows sends
    /// milliseconds while the Linux/libei capture sends microseconds. A raw
    /// difference of 1000 or more is treated as microseconds (a 1000 Hz mouse
    /// reports 1000 us = 1 ms), so the velocity estimate works for both
    /// without a protocol change.
    fn motion_dt(&mut self, time: u32) -> f64 {
        let now = Instant::now();
        // Prefer the wire timestamp (Linux/libei capture is precise) but fall
        // back to the local arrival time when the backend does not provide one
        // (Windows/macOS capture report time = 0).
        let wire_dt = (time != 0)
            .then(|| dt_from_raw(self.last_motion_time, time))
            .flatten();
        let arrival_dt = self
            .last_motion_arrival
            .map(|previous| now.duration_since(previous).as_secs_f64() * 1000.0)
            .filter(|dt| dt.is_finite() && *dt > 0.0)
            .map(|dt| dt.clamp(0.5, 100.0));
        self.last_motion_time = (time != 0).then_some(time);
        self.last_motion_arrival = Some(now);
        wire_dt.or(arrival_dt).unwrap_or(8.0)
    }

    pub(crate) async fn new() -> Result<Self, LibeiEmulationCreationError> {
        let (_remote_desktop, session, eifd) = get_ei_fd().await?;
        let stream = UnixStream::from(eifd);
        stream.set_nonblocking(true)?;
        let context = ei::Context::new(stream)?;
        let (conn, events) = context
            .handshake_tokio("de.feschber.LanMouse", ContextType::Sender)
            .await?;
        let devices = Devices::default();
        let libei_error = Arc::new(AtomicBool::default());
        let error = Arc::new(Mutex::new(None));
        let ei_handler = ei_task(
            events,
            conn.clone(),
            context.clone(),
            devices.clone(),
            libei_error.clone(),
            error.clone(),
        );
        let ei_task = tokio::task::spawn_local(ei_handler);
        let pointer_settings = PointerSettings::detect_async().await;

        Ok(Self {
            context,
            conn,
            devices,
            ei_task,
            error,
            libei_error,
            _remote_desktop,
            session,
            remote_edge: None,
            entry_along: None,
            absolute: None,
            edge_return: None,
            absolute_warned: false,
            clock: Instant::now(),
            absolute_sensitivity: 1.0,
            pointer_settings,
            last_motion_time: None,
            last_motion_arrival: None,
            absolute_env_scale: read_env_scale(),
        })
    }
}

impl Drop for LibeiEmulation {
    fn drop(&mut self) {
        self.ei_task.abort();
    }
}

#[async_trait]
impl Emulation for LibeiEmulation {
    fn set_edge_return_notifier(&mut self, notifier: EdgeReturnNotifier) {
        self.edge_return = Some(notifier);
    }

    fn set_absolute_sensitivity(&mut self, gain: f64) {
        self.absolute_sensitivity = gain;
        if let Some(tracker) = self.absolute.as_mut() {
            tracker.delta_scale =
                effective_delta_scale(tracker.base_scale, gain, self.absolute_env_scale);
        }
    }

    async fn begin_remote_control(&mut self, edge: PointerEdge, along: Option<f32>) {
        self.remote_edge = Some(edge);
        self.entry_along = along;
        self.absolute = None;
        self.absolute_warned = false;
        self.last_motion_time = None;
        self.last_motion_arrival = None;
        self.start_absolute_tracking();
    }

    async fn end_remote_control(&mut self) {
        self.remote_edge = None;
        self.entry_along = None;
        self.absolute = None;
        self.absolute_warned = false;
        self.last_motion_time = None;
        self.last_motion_arrival = None;
    }

    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        // EI frame timestamps must be CLOCK_MONOTONIC microseconds.
        let now = self.now_micros();
        if self.libei_error.load(Ordering::SeqCst) {
            // don't break sending additional events but signal error
            let error = self.error.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(e) = error {
                return Err(e);
            }
        }
        match event {
            Event::Pointer(p) => match p {
                PointerEvent::Motion { time, dx, dy } => {
                    // A non-finite delta (corrupted packet) must not be
                    // forwarded into the EIS device state.
                    if !dx.is_finite() || !dy.is_finite() {
                        log::warn!("dropping non-finite motion: ({dx}, {dy})");
                        return Ok(());
                    }
                    // Sanity-check the raw delta before the acceleration boost:
                    // accelerating first could turn a legitimate fast event
                    // into an "implausibly large" one and drop it.
                    if dx.abs() > MAX_DELTA_PX || dy.abs() > MAX_DELTA_PX {
                        log::debug!("dropping implausible raw delta ({dx}, {dy})");
                        return Ok(());
                    }
                    // While a peer drives this machine and the compositor
                    // offers an absolute pointer, move it absolutely from a
                    // tracked position: that is exact (no acceleration/drift)
                    // and makes the edge-return detection trustworthy.
                    if self.remote_edge.is_some() {
                        if self.absolute.is_none() {
                            self.start_absolute_tracking();
                        }
                        // Only use the absolute device the tracker was armed
                        // against. A replaced/removed device falls back to
                        // relative injection (and re-arms on the next event).
                        let current = self
                            .devices
                            .absolute
                            .read()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clone();
                        let usable = match (&self.absolute, current) {
                            (Some(tracker), Some(pointer))
                                if tracker.generation == pointer.generation =>
                            {
                                let paused = self
                                    .devices
                                    .paused
                                    .read()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .contains(&pointer.device);
                                (!paused).then_some(pointer)
                            }
                            _ => None,
                        };
                        if let Some(pointer) = usable {
                            let mut tracker = self.absolute.take().expect("checked above");
                            // Mirror the receiver's pointer acceleration so
                            // the physical feel matches the relative path on
                            // every resolution/scale (slow = 1:1 fine control,
                            // fast = accelerated like before).
                            let dt = self.motion_dt(time);
                            let velocity = (dx * dx + dy * dy).sqrt() / dt;
                            let velocity = velocity.clamp(0.0, 1000.0);
                            let accel = self.pointer_settings.acceleration(velocity);
                            let fired = tracker.apply(dx * accel, dy * accel);
                            let (x, y) = tracker.absolute();
                            pointer.pointer.motion_absolute(x, y);
                            pointer.device.frame(self.conn.serial(), now);
                            self.context
                                .flush()
                                .map_err(|e| io::Error::new(e.kind(), e))?;
                            let return_along =
                                fired.is_some().then(|| tracker.return_along()).flatten();
                            self.absolute = Some(tracker);
                            if let Some(edge) = fired {
                                log::info!("pushed past the {edge:?} edge; handing control back");
                                if let Some(notifier) = &self.edge_return {
                                    let _ = notifier.send((edge, return_along));
                                }
                            }
                            return Ok(());
                        }
                        // device changed/paused: drop the stale tracker and
                        // keep injecting relatively until it is re-armed.
                        self.absolute = None;
                    }
                    // The wire carries f64 while EIS uses f32: a huge finite
                    // value would become infinity when narrowed.
                    let (dx, dy) = (dx as f32, dy as f32);
                    if !dx.is_finite() || !dy.is_finite() {
                        log::warn!("dropping out-of-range motion: ({dx}, {dy})");
                        return Ok(());
                    }
                    let pointer_device = self
                        .devices
                        .pointer
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, p)) = pointer_device.as_ref() {
                        p.motion_relative(dx, dy);
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    let button_device = self
                        .devices
                        .button
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, b)) = button_device.as_ref() {
                        b.button(
                            button,
                            match state {
                                0 => ButtonState::Released,
                                _ => ButtonState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    let Some((dx, dy)) = scroll_deltas(axis, value) else {
                        return Ok(());
                    };
                    let scroll_device = self
                        .devices
                        .scroll
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, s)) = scroll_device.as_ref() {
                        s.scroll(dx, dy);
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    let Some((dx, dy)) = discrete_scroll_deltas(axis, value) else {
                        return Ok(());
                    };
                    let scroll_device = self
                        .devices
                        .scroll
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, s)) = scroll_device.as_ref() {
                        s.scroll_discrete(dx, dy);
                        d.frame(self.conn.serial(), now);
                    }
                }
            },
            Event::Keyboard(k) => match k {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let keyboard_device = self
                        .devices
                        .keyboard
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, k)) = keyboard_device.as_ref() {
                        k.key(
                            key,
                            match state {
                                0 => KeyState::Released,
                                _ => KeyState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        self.context
            .flush()
            .map_err(|e| io::Error::new(e.kind(), e))?;
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {}
    async fn destroy(&mut self, _: EmulationHandle) {}

    async fn terminate(&mut self) {
        // A hung portal must not hang app shutdown: close on a best-effort
        // basis with a timeout, then abort the EIS task.
        match tokio::time::timeout(Duration::from_secs(2), self.session.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("session.close(): {e}"),
            Err(_) => log::warn!("session.close() timed out"),
        }
        self.ei_task.abort();
    }
}

/// Map a continuous scroll event to EIS (dx, dy) deltas.
///
/// Only the two axes the wire protocol defines are accepted; an unknown axis
/// from a corrupted packet is dropped instead of being treated as horizontal.
fn scroll_deltas(axis: u8, value: f64) -> Option<(f32, f32)> {
    if !value.is_finite() {
        return None;
    }
    let value = value as f32;
    // Narrowing can turn a huge-but-finite f64 (e.g. 1e300 from a corrupted
    // packet) into f32::INFINITY; EIS must never receive a non-finite delta.
    if !value.is_finite() {
        return None;
    }
    match axis {
        0 => Some((0.0, value)),
        1 => Some((value, 0.0)),
        _ => None,
    }
}

/// Map a discrete (120-unit) scroll event to EIS (dx, dy) deltas.
fn discrete_scroll_deltas(axis: u8, value: i32) -> Option<(i32, i32)> {
    match axis {
        0 => Some((0, value)),
        1 => Some((value, 0)),
        _ => None,
    }
}

async fn ei_task(
    mut events: EiConvertEventStream,
    conn: Connection,
    context: ei::Context,
    devices: Devices,
    libei_error: Arc<AtomicBool>,
    error: Arc<Mutex<Option<EmulationError>>>,
) {
    loop {
        match ei_event_handler(&mut events, &conn, &context, &devices).await {
            Ok(()) => {}
            Err(e) => {
                libei_error.store(true, Ordering::SeqCst);
                error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .replace(e);
                // wait for termination -> otherwise we will loop forever
                future::pending::<()>().await;
            }
        }
    }
}

async fn ei_event_handler(
    events: &mut EiConvertEventStream,
    conn: &Connection,
    context: &ei::Context,
    devices: &Devices,
) -> Result<(), EmulationError> {
    loop {
        let event = events.next().await.ok_or(EmulationError::EndOfStream)??;
        let capabilities = DeviceCapability::Pointer
            | DeviceCapability::PointerAbsolute
            | DeviceCapability::Keyboard
            | DeviceCapability::Touch
            | DeviceCapability::Scroll
            | DeviceCapability::Button;
        log::debug!("{event:?}");
        match event {
            EiEvent::Disconnected(e) => {
                log::debug!("ei disconnected: {e:?}");
                return Err(EmulationError::EndOfStream);
            }
            EiEvent::SeatAdded(e) => {
                e.seat().bind_capabilities(capabilities);
            }
            EiEvent::SeatRemoved(e) => {
                log::debug!("seat removed: {:?}", e.seat());
            }
            EiEvent::DeviceAdded(e) => {
                let device_type = e.device().device_type();
                log::debug!("device added: {device_type:?}");
                let device = e.device();
                if let Some(pointer) = e.device().interface::<Pointer>() {
                    devices
                        .pointer
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace((device.device().clone(), pointer));
                }
                if let Some(pointer) = e.device().interface::<PointerAbsolute>() {
                    let regions = region_boxes(e.device().regions());
                    if regions.is_empty() {
                        log::warn!(
                            "absolute pointer device has no usable region; automatic edge return disabled"
                        );
                    } else {
                        let generation = devices
                            .absolute_generation
                            .fetch_add(1, Ordering::SeqCst)
                            .wrapping_add(1);
                        log::info!(
                            "absolute pointer available ({} region(s)); automatic edge return enabled",
                            regions.len()
                        );
                        devices
                            .absolute
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .replace(AbsolutePointer {
                                device: device.device().clone(),
                                pointer,
                                generation,
                                regions,
                            });
                    }
                }
                if let Some(keyboard) = e.device().interface::<Keyboard>() {
                    devices
                        .keyboard
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace((device.device().clone(), keyboard));
                }
                if let Some(scroll) = e.device().interface::<Scroll>() {
                    devices
                        .scroll
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace((device.device().clone(), scroll));
                }
                if let Some(button) = e.device().interface::<Button>() {
                    devices
                        .button
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace((device.device().clone(), button));
                }
            }
            EiEvent::DeviceRemoved(e) => {
                log::debug!("device removed: {:?}", e.device().device_type());
                // Never send events on the destroyed EIS objects again; a
                // fresh interface set arrives with the next DeviceAdded.
                devices.forget_device(e.device().device());
            }
            EiEvent::DevicePaused(e) => {
                log::debug!("device paused: {:?}", e.device().device_type());
                devices
                    .paused
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(e.device().device().clone());
            }
            EiEvent::DeviceResumed(e) => {
                log::debug!("device resumed: {:?}", e.device().device_type());
                devices
                    .paused
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(e.device().device());
                let sequence = devices
                    .resume_sequence
                    .fetch_add(1, Ordering::SeqCst)
                    .wrapping_add(1);
                e.device()
                    .device()
                    .start_emulating(conn.serial(), sequence as u32);
            }
            EiEvent::KeyboardModifiers(e) => {
                log::debug!("modifiers: {e:?}");
            }
            // only for receiver context
            // EiEvent::Frame(_) => { },
            // EiEvent::DeviceStartEmulating(_) => { },
            // EiEvent::DeviceStopEmulating(_) => { },
            // EiEvent::PointerMotion(_) => { },
            // EiEvent::PointerMotionAbsolute(_) => { },
            // EiEvent::Button(_) => { },
            // EiEvent::ScrollDelta(_) => { },
            // EiEvent::ScrollStop(_) => { },
            // EiEvent::ScrollCancel(_) => { },
            // EiEvent::ScrollDiscrete(_) => { },
            // EiEvent::KeyboardKey(_) => { },
            // EiEvent::TouchDown(_) => { },
            // EiEvent::TouchUp(_) => { },
            // EiEvent::TouchMotion(_) => { },
            other => {
                // Receiver-only or unknown events must never panic the
                // service: an unexpected protocol event is not worth a crash.
                log::debug!("ignoring unexpected eis event: {other:?}");
            }
        }
        context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
    }
}

#[cfg(test)]
mod absolute_tracker_tests {
    use super::*;

    const W: f32 = 2560.0;
    const H: f32 = 1600.0;

    fn tracker(edge: PointerEdge) -> AbsoluteTracker {
        AbsoluteTracker::new(edge, 1, vec![(0.0, 0.0, W, H)], 1.0, 1.0).expect("single region")
    }

    fn dual_regions() -> Vec<(f32, f32, f32, f32)> {
        vec![(0.0, 0.0, 1920.0, 1080.0), (1920.0, 0.0, 3840.0, 1080.0)]
    }

    #[test]
    fn anchor_starts_at_the_entry_edge() {
        assert_eq!(tracker(PointerEdge::Left).absolute(), (24.0, 800.0));
        assert_eq!(
            tracker(PointerEdge::Right).absolute(),
            (W - 1.0 - 24.0, 800.0)
        );
        assert_eq!(tracker(PointerEdge::Top).absolute(), (1280.0, 24.0));
        assert_eq!(
            tracker(PointerEdge::Bottom).absolute(),
            (1280.0, H - 1.0 - 24.0)
        );
    }

    #[test]
    fn entry_coordinate_parks_at_the_crossing_point() {
        let regions = vec![(0.0, 0.0, W, H)];
        let left = AbsoluteTracker::new_with_entry(
            PointerEdge::Left,
            1,
            regions.clone(),
            1.0,
            1.0,
            Some(0.25),
        )
        .expect("left tracker");
        assert_eq!(left.absolute(), (0.0, 400.0));

        let right = AbsoluteTracker::new_with_entry(
            PointerEdge::Right,
            1,
            regions.clone(),
            1.0,
            1.0,
            Some(0.75),
        )
        .expect("right tracker");
        assert_eq!(right.absolute(), (W - 1.0, 1200.0));

        let top = AbsoluteTracker::new_with_entry(
            PointerEdge::Top,
            1,
            regions.clone(),
            1.0,
            1.0,
            Some(0.5),
        )
        .expect("top tracker");
        assert_eq!(top.absolute(), (1280.0, 0.0));

        let bottom =
            AbsoluteTracker::new_with_entry(PointerEdge::Bottom, 1, regions, 1.0, 1.0, Some(0.5))
                .expect("bottom tracker");
        assert_eq!(bottom.absolute(), (1280.0, H - 1.0));
    }

    #[test]
    fn entry_coordinate_selects_the_touched_monitor_and_clamps() {
        // Two monitors side by side; crossing the top edge at 75% must land on
        // the second monitor at its own relative position.
        let tracker = AbsoluteTracker::new_with_entry(
            PointerEdge::Top,
            1,
            dual_regions(),
            1.0,
            1.0,
            Some(0.75),
        )
        .expect("multi-monitor tracker");
        assert_eq!(tracker.absolute(), (2880.0, 0.0));

        // Out-of-range fractions are clamped instead of escaping the layout.
        let clamped = AbsoluteTracker::new_with_entry(
            PointerEdge::Left,
            1,
            vec![(0.0, 0.0, W, H)],
            1.0,
            1.0,
            Some(2.0),
        )
        .expect("clamped tracker");
        assert_eq!(clamped.absolute(), (0.0, H - 1.0));
    }

    #[test]
    fn exact_entry_coordinate_does_not_return_on_inward_motion() {
        // Parked exactly at the left edge pixel: the user's continued motion
        // into the screen must move the cursor, not hand control back.
        let mut tracker = AbsoluteTracker::new_with_entry(
            PointerEdge::Left,
            1,
            vec![(0.0, 0.0, W, H)],
            1.0,
            1.0,
            Some(0.5),
        )
        .expect("left tracker");
        assert_eq!(tracker.absolute(), (0.0, 800.0));
        assert_eq!(tracker.apply(100.0, 0.0), None);
        assert_eq!(tracker.apply(50.0, 10.0), None);
        // A real push back through the edge still hands control back.
        assert_eq!(tracker.apply(-160.0, 0.0), Some(PointerEdge::Left));

        let mut right = AbsoluteTracker::new_with_entry(
            PointerEdge::Right,
            1,
            vec![(0.0, 0.0, W, H)],
            1.0,
            1.0,
            Some(0.5),
        )
        .expect("right tracker");
        assert_eq!(right.absolute(), (W - 1.0, 800.0));
        assert_eq!(right.apply(-100.0, 0.0), None);
        assert_eq!(right.apply(110.0, 0.0), Some(PointerEdge::Right));
    }

    #[test]
    fn moving_inside_the_screen_never_returns() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(100.0, 0.0), None);
        assert_eq!(tracker.apply(50.0, 40.0), None);
        assert_eq!(tracker.apply(-80.0, -40.0), None);
        assert_eq!(tracker.apply(-40.0, 0.0), None);
        assert!(!tracker.fired);
    }

    #[test]
    fn pushing_past_the_entry_edge_fires_exactly_once() {
        let mut tracker = tracker(PointerEdge::Left);
        // anchored 24 px inside; a push back past the edge hands control back
        assert_eq!(tracker.apply(-30.0, 0.0), Some(PointerEdge::Left));
        // latched: a single hand-off per activation
        assert_eq!(tracker.apply(-50.0, 0.0), None);
    }

    #[test]
    fn every_edge_uses_its_own_axis_and_direction() {
        let cases = [
            (PointerEdge::Left, -30.0, 0.0),
            (PointerEdge::Right, 30.0, 0.0),
            (PointerEdge::Top, 0.0, -30.0),
            (PointerEdge::Bottom, 0.0, 30.0),
        ];
        for (edge, sx, sy) in cases {
            let mut tracker = tracker(edge);
            assert_eq!(tracker.apply(sx, sy), Some(edge), "{edge:?}");
        }
    }

    #[test]
    fn wrong_axis_does_not_fire() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(0.0, -800.0), None);
        assert_eq!(tracker.apply(0.0, 800.0), None);
        assert!(!tracker.fired);
    }

    #[test]
    fn absolute_position_stays_valid_in_half_open_region_bounds() {
        let mut tracker = tracker(PointerEdge::Right);
        assert_eq!(tracker.apply(30.0, 0.0), Some(PointerEdge::Right));
        let (x, y) = tracker.absolute();
        assert!((0.0..=W - 1.0).contains(&x));
        assert!((0.0..=H - 1.0).contains(&y));
    }

    #[test]
    fn pointer_can_cross_into_the_adjacent_monitor() {
        let mut tracker =
            AbsoluteTracker::new(PointerEdge::Left, 1, dual_regions(), 1.0, 1.0).unwrap();
        // anchored at the left edge of the first monitor
        assert_eq!(tracker.absolute(), (24.0, 540.0));
        assert_eq!(tracker.apply(2000.0, 0.0), None);
        let (x, _) = tracker.absolute();
        assert!(x > 1920.0, "cursor must reach the second monitor, got {x}");
        assert!(x <= 3839.0);
    }

    #[test]
    fn pointer_can_cross_into_a_stacked_monitor() {
        let regions = vec![(0.0, 0.0, 1920.0, 1080.0), (0.0, 1080.0, 1920.0, 2160.0)];
        let mut tracker = AbsoluteTracker::new(PointerEdge::Top, 1, regions, 1.0, 1.0).unwrap();
        assert_eq!(tracker.apply(0.0, 1500.0), None);
        let (_, y) = tracker.absolute();
        assert!(y > 1080.0, "cursor must reach the lower monitor, got {y}");
    }

    #[test]
    fn overrun_does_not_swallow_the_reverse_movement() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(-30.0, 0.0), Some(PointerEdge::Left));
        // cursor snapped to the edge; moving back inside moves it immediately
        let _ = tracker.apply(500.0, 0.0);
        let (x, _) = tracker.absolute();
        assert!((x - 500.0).abs() < 1.0, "expected x≈500, got {x}");
    }

    #[test]
    fn implausible_single_delta_is_dropped() {
        let mut tracker = tracker(PointerEdge::Left);
        let before = tracker.absolute();
        assert_eq!(tracker.apply(-1.0e12, 0.0), None);
        assert!(!tracker.fired);
        assert_eq!(tracker.absolute(), before);
    }

    #[test]
    fn screen_relative_mapping_is_one_logical_pixel_per_count() {
        // independent of the monitor scale: 1000 counts move 1000 logical px
        let mut tracker = AbsoluteTracker::new(
            PointerEdge::Left,
            1,
            vec![(0.0, 0.0, 1536.0, 960.0)],
            1.0,
            1.0,
        )
        .expect("region");
        let (x0, _) = tracker.absolute();
        assert_eq!(tracker.apply(1000.0, 0.0), None);
        let (x1, _) = tracker.absolute();
        assert!(
            (x1 - x0 - 1000.0).abs() < 2.0,
            "expected ~1000 px, got {}",
            x1 - x0
        );
    }

    #[test]
    fn hole_between_monitors_does_not_fire_a_false_handoff() {
        // L-shaped layout: A(0,0,1920x1080), B(1920,0,1920x1080),
        // C(3840,1080,1920x1080); there is an empty lower-left quadrant.
        let regions = vec![
            (0.0, 0.0, 1920.0, 1080.0),
            (1920.0, 0.0, 3840.0, 1080.0),
            (3840.0, 1080.0, 5760.0, 2160.0),
        ];
        let mut tracker =
            AbsoluteTracker::new(PointerEdge::Bottom, 1, regions, 1.0, 1.0).expect("regions");
        // anchor is at the bottom edge of C
        assert_eq!(tracker.absolute(), (4800.0, 2135.0));
        // push left into the empty quadrant: the real cursor is snapped to a
        // monitor edge, so this must NOT hand off
        assert_eq!(tracker.apply(-3000.0, 0.0), None);
        assert!(!tracker.fired);
        // more vertical push while still in the hole: still no hand-off
        assert_eq!(tracker.apply(0.0, 600.0), None);
        assert!(!tracker.fired);
        // reach C again, then genuinely push past its bottom edge
        let _ = tracker.apply(2500.0, 0.0);
        let _ = tracker.apply(0.0, 500.0);
        assert_eq!(tracker.apply(0.0, 700.0), Some(PointerEdge::Bottom));
    }

    #[test]
    fn entry_region_is_the_monitor_touching_that_edge() {
        let regions = vec![
            Region {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                mapping_id: None,
            },
            Region {
                x: 1920,
                y: 0,
                width: 1280,
                height: 720,
                scale: 1.0,
                mapping_id: None,
            },
        ];
        let boxes = region_boxes(&regions);
        assert_eq!(boxes.len(), 2);
        assert_eq!(
            entry_region_bounds(&boxes, PointerEdge::Left),
            Some((0.0, 0.0, 1920.0, 1080.0))
        );
        assert_eq!(
            entry_region_bounds(&boxes, PointerEdge::Right),
            Some((1920.0, 0.0, 3200.0, 720.0))
        );
        assert_eq!(entry_region_bounds(&[], PointerEdge::Left), None);
    }

    #[test]
    fn flat_profile_ignores_velocity() {
        let settings = PointerSettings {
            speed: 0.0,
            adaptive: false,
        };
        assert!((settings.acceleration(0.01) - 1.0).abs() < 1e-9);
        assert!((settings.acceleration(10.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn adaptive_profile_keeps_slow_motion_linear_and_boosts_fast_motion() {
        let settings = PointerSettings {
            speed: 0.0,
            adaptive: true,
        };
        let slow = settings.acceleration(0.05);
        assert!(
            (slow - 1.0).abs() < 1e-9,
            "slow motion must stay 1:1, got {slow}"
        );
        let medium = settings.acceleration(1.0);
        let fast = settings.acceleration(5.0);
        assert!(medium > 1.0, "medium motion must be accelerated");
        assert!(fast > medium, "faster motion must accelerate more");
        assert!(fast <= 2.2 + 1e-9, "boost must stay bounded");
    }

    #[test]
    fn speed_setting_scales_the_acceleration() {
        let default = PointerSettings {
            speed: 0.0,
            adaptive: true,
        };
        let faster = PointerSettings {
            speed: 0.5,
            adaptive: true,
        };
        let slower = PointerSettings {
            speed: -0.5,
            adaptive: true,
        };
        assert!(faster.acceleration(1.0) > default.acceleration(1.0));
        assert!(slower.acceleration(1.0) < default.acceleration(1.0));
        assert!(slower.acceleration(1.0) > 0.0);
    }

    #[test]
    fn acceleration_is_monotonic() {
        let settings = PointerSettings {
            speed: 0.0,
            adaptive: true,
        };
        let mut previous = 0.0;
        for step in 0..200 {
            let velocity = step as f64 * 0.05;
            let current = settings.acceleration(velocity);
            assert!(current >= previous - 1e-9, "not monotonic at {velocity}");
            previous = current;
        }
    }

    #[test]
    fn sensitivity_scales_the_screen_relative_mapping() {
        // 200% sensitivity doubles the logical movement, at any monitor scale
        let mut sensitive = AbsoluteTracker::new(
            PointerEdge::Left,
            1,
            vec![(0.0, 0.0, 1536.0, 960.0)],
            1.0,
            2.0,
        )
        .expect("region");
        let (x0, _) = sensitive.absolute();
        let _ = sensitive.apply(400.0, 0.0);
        let (x1, _) = sensitive.absolute();
        assert!((x1 - x0 - 800.0).abs() < 4.0);
    }

    #[test]
    fn tiny_region_anchor_stays_inside_the_region() {
        let tracker =
            AbsoluteTracker::new(PointerEdge::Left, 1, vec![(0.0, 0.0, 20.0, 20.0)], 1.0, 1.0)
                .expect("region");
        let (x, y) = tracker.absolute();
        assert!((0.0..=19.0).contains(&x), "x={x} outside the region");
        assert!((0.0..=19.0).contains(&y), "y={y} outside the region");
    }

    #[test]
    fn timestamp_units_are_inferred() {
        // milliseconds: 8 ms between events
        assert_eq!(dt_from_raw(Some(100), 108), Some(8.0));
        // microseconds (libei): 8000 us between events = 8 ms
        assert_eq!(dt_from_raw(Some(1_000_000), 1_008_000), Some(8.0));
        // a 1000 Hz report (1000 us) is one millisecond
        assert_eq!(dt_from_raw(Some(5_000), 6_000), Some(1.0));
        // a real pause keeps its duration instead of a fake 8 ms interval
        assert_eq!(dt_from_raw(Some(0), 50), Some(50.0));
        assert_eq!(dt_from_raw(Some(0), 200), Some(100.0));
        // missing or duplicate timestamps do not produce a velocity
        assert_eq!(dt_from_raw(None, 42), None);
        assert_eq!(dt_from_raw(Some(42), 42), None);
    }

    #[test]
    fn live_update_keeps_the_environment_override() {
        let base = 1.0;
        let sensitivity = 1.0;
        let env = 2.0;
        let armed = effective_delta_scale(base, sensitivity, env);
        // even a no-op live update must keep the temporary override
        let updated = effective_delta_scale(base, 1.0, env);
        assert!((armed - 2.0).abs() < 1e-9);
        assert!((updated - armed).abs() < 1e-9);
        // a real sensitivity change scales on top of the override
        let faster = effective_delta_scale(base, 1.5, env);
        assert!((faster - 3.0).abs() < 1e-9);
    }

    #[test]
    fn accelerated_fast_delta_is_not_rejected() {
        let mut tracker = tracker(PointerEdge::Left);
        // the raw delta limit is 4096; the tracker must accept the boosted
        // value (up to 3x) instead of treating it as corrupted
        assert_eq!(tracker.apply(-8800.0, 0.0), Some(PointerEdge::Left));
    }

    #[test]
    fn fired_latch_rearms_after_returning_inside() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(-30.0, 0.0), Some(PointerEdge::Left));
        assert_eq!(tracker.apply(-8.0, 0.0), None);
        let _ = tracker.apply(200.0, 0.0);
        assert_eq!(tracker.apply(-1200.0, 0.0), Some(PointerEdge::Left));
    }
}
