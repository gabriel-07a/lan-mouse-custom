//! OS-level tuning that keeps input latency low and steady on Wi-Fi.
//!
//! Everything here is best effort: a failure is logged and the service keeps
//! running with the OS defaults. Only Windows needs explicit tuning; on Linux
//! the DSCP mark set in `connect::configure_low_latency_udp` already selects
//! the WMM voice queue and timers are precise by default.
//!
//! What was applied is recorded so diagnostics can show it (`doctor`, the
//! Settings page): "the Wi-Fi tuning did not help" and "the Wi-Fi tuning was
//! never applied" look the same from the network side.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
pub(crate) use windows_impl::{init_process, prioritize_socket, set_link_active};

/// Process-wide setup, called once when the service starts.
#[cfg(not(windows))]
pub(crate) fn init_process() {}

/// Tell the OS whether a peer link is up, so radio tuning is only applied
/// while Lan Mouse is actually in use.
#[cfg(not(windows))]
pub(crate) fn set_link_active(_active: bool) {}

/// Ask the OS to prioritise the datagrams of a connected UDP socket.
#[cfg(not(windows))]
pub(crate) fn prioritize_socket(_socket: &tokio::net::UdpSocket) {}

/// State of the Wi-Fi low-latency mode.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum WifiState {
    /// No peer link yet, so nothing was attempted.
    #[default]
    Idle,
    /// Applied on this many adapters.
    Active(usize),
    /// Reverted because no peer link is up any more.
    Reverted,
    /// The machine has no usable WLAN service or adapter.
    Unavailable(String),
    /// Every adapter refused the settings.
    Refused(String),
}

/// What the tuning did so far.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TuningStatus {
    /// Result of the 1 ms timer request; `None` before it ran.
    timer: Option<Result<(), String>>,
    /// Result of the last QoS flow attach; `None` before the first socket.
    qos: Option<Result<(), String>>,
    wifi: WifiState,
}

static STATUS: Mutex<TuningStatus> = Mutex::new(TuningStatus {
    timer: None,
    qos: None,
    wifi: WifiState::Idle,
});

/// Set when [`STATUS`] changed since the last [`take_changed`].
static CHANGED: AtomicBool = AtomicBool::new(false);

#[cfg_attr(not(windows), allow(dead_code))]
fn update(apply: impl FnOnce(&mut TuningStatus)) {
    let mut status = STATUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = status.clone();
    apply(&mut status);
    if *status != before {
        CHANGED.store(true, Ordering::SeqCst);
    }
}

/// Whether the tuning status changed since the last call (the service then
/// re-sends the diagnostics to the frontends).
pub(crate) fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::SeqCst)
}

/// One-line description of the tuning in effect.
pub(crate) fn summary() -> String {
    if cfg!(windows) {
        let status = STATUS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        describe(&status)
    } else {
        "voice priority (DSCP EF); Wi-Fi tuning is Windows-only".to_string()
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn describe(status: &TuningStatus) -> String {
    let timer = match &status.timer {
        None => "timers: not set".to_string(),
        Some(Ok(())) => "1 ms timers".to_string(),
        Some(Err(e)) => format!("default timers ({e})"),
    };
    let qos = match &status.qos {
        None => "QoS: no outgoing link yet".to_string(),
        Some(Ok(())) => "QoS voice priority".to_string(),
        Some(Err(e)) => format!("QoS unavailable ({e})"),
    };
    let wifi = match &status.wifi {
        WifiState::Idle => "Wi-Fi low-latency: waiting for a link".to_string(),
        WifiState::Active(adapters) => format!("Wi-Fi low-latency on {adapters} adapter(s)"),
        WifiState::Reverted => "Wi-Fi low-latency: off (no link)".to_string(),
        WifiState::Unavailable(reason) => format!("no Wi-Fi tuning ({reason})"),
        WifiState::Refused(reason) => format!("Wi-Fi low-latency refused ({reason})"),
    };
    format!("{timer} · {qos} · {wifi}")
}

#[cfg(windows)]
mod windows_impl {
    use super::{WifiState, update};
    use std::os::windows::io::AsRawSocket;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use tokio::net::UdpSocket;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Media::timeBeginPeriod;
    use windows::Win32::NetworkManagement::QoS::{
        QOS_NON_ADAPTIVE_FLOW, QOS_VERSION, QOSAddSocketToFlow, QOSCreateHandle,
        QOSTrafficTypeVoice,
    };
    use windows::Win32::NetworkManagement::WiFi::{
        WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory,
        WlanOpenHandle, WlanSetInterface, wlan_intf_opcode_background_scan_enabled,
        wlan_intf_opcode_media_streaming_mode,
    };
    use windows::Win32::Networking::WinSock::SOCKET;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
    };

    /// A failed Wi-Fi tuning attempt is not retried sooner than this. The
    /// service reports link state changes about once per second; retrying the
    /// WLAN service that often would only repeat the same failure.
    const WIFI_RETRY_INTERVAL: Duration = Duration::from_secs(60);

    /// Raw handle value; `HANDLE` is not `Send`, the value is.
    struct RawHandle(isize);

    /// Process-wide setup, called once when the service starts.
    ///
    /// Windows wakes sleeping threads on a ~15.6 ms tick by default, so the
    /// 2 ms motion flush timer (and every other short timer of the runtime)
    /// would fire up to a whole tick late. Request 1 ms timers and, on
    /// Windows 11, opt out of the rule that ignores that request for
    /// processes without a visible window (the service has none).
    pub(crate) fn init_process() {
        static DONE: AtomicBool = AtomicBool::new(false);
        if DONE.swap(true, Ordering::SeqCst) {
            return;
        }
        // TIMERR_NOERROR is 0.
        let result = unsafe { timeBeginPeriod(1) };
        if result != 0 {
            log::warn!("could not request 1 ms timer resolution (error {result})");
            update(|status| status.timer = Some(Err(format!("error {result}"))));
            return;
        }
        update(|status| status.timer = Some(Ok(())));
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            // Control bit set, state bit clear: always honour the request.
            StateMask: 0,
        };
        let applied = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw const state).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        match applied {
            Ok(()) => log::debug!("1 ms timer resolution requested"),
            // Older Windows 10 builds do not know this control bit; there the
            // timeBeginPeriod request above is honoured unconditionally.
            Err(e) => log::debug!("timer resolution throttling opt-out not available: {e}"),
        }
    }

    /// Open WLAN client handle while low-latency mode is applied, and the
    /// time of the last failed attempt.
    struct Wlan {
        handle: Option<RawHandle>,
        failed_at: Option<Instant>,
    }

    static WLAN: Mutex<Wlan> = Mutex::new(Wlan {
        handle: None,
        failed_at: None,
    });

    /// Apply or revert the Wi-Fi low-latency mode.
    ///
    /// While connected, Windows periodically leaves the channel to scan for
    /// other networks, which stalls traffic for 100 ms or more. Media
    /// streaming mode and disabling the background scan avoid those stalls.
    /// Both settings belong to the WLAN client handle: closing it (or the
    /// process exiting, even by a crash) restores the adapter defaults.
    pub(crate) fn set_link_active(active: bool) {
        let mut wlan = WLAN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !active {
            wlan.failed_at = None;
            if let Some(handle) = wlan.handle.take() {
                unsafe {
                    WlanCloseHandle(HANDLE(handle.0 as *mut core::ffi::c_void), None);
                }
                log::debug!("Wi-Fi low-latency mode reverted");
                update(|status| status.wifi = WifiState::Reverted);
            }
            return;
        }
        if wlan.handle.is_some()
            || wlan
                .failed_at
                .is_some_and(|at| at.elapsed() < WIFI_RETRY_INTERVAL)
        {
            return;
        }
        match open_low_latency_wlan() {
            Ok((handle, tuned)) => {
                wlan.handle = Some(handle);
                wlan.failed_at = None;
                update(|status| status.wifi = WifiState::Active(tuned));
            }
            Err(state) => {
                wlan.failed_at = Some(Instant::now());
                update(|status| status.wifi = state);
            }
        }
    }

    fn open_low_latency_wlan() -> Result<(RawHandle, usize), WifiState> {
        let mut negotiated = 0u32;
        let mut handle = HANDLE::default();
        // Client version 2: Windows Vista and later.
        let opened = unsafe { WlanOpenHandle(2, None, &mut negotiated, &mut handle) };
        if opened != 0 {
            // No WLAN service (desktop without Wi-Fi, server edition).
            log::debug!("WLAN service not available (error {opened}); no Wi-Fi tuning");
            return Err(WifiState::Unavailable(format!(
                "WLAN service error {opened}"
            )));
        }
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        let enumerated = unsafe { WlanEnumInterfaces(handle, None, &mut list) };
        if enumerated != 0 || list.is_null() {
            log::debug!("could not enumerate WLAN interfaces (error {enumerated})");
            unsafe {
                WlanCloseHandle(handle, None);
            }
            return Err(WifiState::Unavailable(format!(
                "adapter list error {enumerated}"
            )));
        }
        let count = unsafe { (*list).dwNumberOfItems } as usize;
        let first = unsafe {
            (&raw const (*list).InterfaceInfo)
                .cast::<windows::Win32::NetworkManagement::WiFi::WLAN_INTERFACE_INFO>()
        };
        let mut tuned = 0usize;
        let mut refusal = None;
        for index in 0..count {
            let guid = unsafe { (*first.add(index)).InterfaceGuid };
            // BOOL values, 4 bytes each.
            let enabled: i32 = 1;
            let disabled: i32 = 0;
            let streaming = unsafe {
                WlanSetInterface(
                    handle,
                    &guid,
                    wlan_intf_opcode_media_streaming_mode,
                    size_of::<i32>() as u32,
                    (&raw const enabled).cast(),
                    None,
                )
            };
            let scan = unsafe {
                WlanSetInterface(
                    handle,
                    &guid,
                    wlan_intf_opcode_background_scan_enabled,
                    size_of::<i32>() as u32,
                    (&raw const disabled).cast(),
                    None,
                )
            };
            if streaming == 0 || scan == 0 {
                tuned += 1;
                if streaming != 0 || scan != 0 {
                    log::info!(
                        "Wi-Fi low-latency mode partially applied on interface {index} \
                         (streaming error {streaming}, scan error {scan})"
                    );
                }
            } else {
                log::warn!(
                    "Wi-Fi low-latency mode refused by interface {index} \
                     (streaming error {streaming}, scan error {scan})"
                );
                refusal = Some(format!("streaming error {streaming}, scan error {scan}"));
            }
        }
        unsafe {
            WlanFreeMemory(list.cast());
        }
        if tuned == 0 {
            unsafe {
                WlanCloseHandle(handle, None);
            }
            return Err(match refusal {
                Some(reason) => WifiState::Refused(reason),
                None => WifiState::Unavailable("no Wi-Fi adapter".to_string()),
            });
        }
        log::info!("Wi-Fi low-latency mode active on {tuned} interface(s)");
        Ok((RawHandle(handle.0 as isize), tuned))
    }

    /// QoS handle shared by every flow; it lives as long as the process
    /// because closing it would remove the flows of the live sockets.
    static QOS: Mutex<Option<RawHandle>> = Mutex::new(None);

    /// Ask Windows to mark the datagrams of a connected UDP socket as voice
    /// traffic (DSCP 56, WMM voice queue on Wi-Fi).
    ///
    /// `setsockopt(IP_TOS)` is ignored by Windows; the QoS2 (qWAVE) flow is
    /// the supported way for an unprivileged process. The flow is removed by
    /// the system when the socket is closed.
    pub(crate) fn prioritize_socket(socket: &UdpSocket) {
        let mut qos = QOS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if qos.is_none() {
            let version = QOS_VERSION {
                MajorVersion: 1,
                MinorVersion: 0,
            };
            let mut handle = HANDLE::default();
            if !unsafe { QOSCreateHandle(&version, &mut handle) }.as_bool() {
                let error = std::io::Error::last_os_error();
                log::debug!("QoS subsystem not available: {error}");
                update(|status| status.qos = Some(Err(error.to_string())));
                return;
            }
            *qos = Some(RawHandle(handle.0 as isize));
        }
        let Some(handle) = qos.as_ref() else {
            return;
        };
        let mut flow_id = 0u32;
        let added = unsafe {
            QOSAddSocketToFlow(
                HANDLE(handle.0 as *mut core::ffi::c_void),
                SOCKET(socket.as_raw_socket() as usize),
                // connected socket: the destination is already known
                None,
                QOSTrafficTypeVoice,
                Some(QOS_NON_ADAPTIVE_FLOW),
                &mut flow_id,
            )
        };
        if added.as_bool() {
            log::debug!("QoS voice flow {flow_id} attached to the outgoing socket");
            update(|status| status.qos = Some(Ok(())));
        } else {
            let error = std::io::Error::last_os_error();
            log::debug!("could not attach a QoS flow: {error}");
            update(|status| status.qos = Some(Err(error.to_string())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_names_what_was_applied_and_what_failed() {
        assert_eq!(
            describe(&TuningStatus::default()),
            "timers: not set · QoS: no outgoing link yet · Wi-Fi low-latency: waiting for a link"
        );
        let applied = TuningStatus {
            timer: Some(Ok(())),
            qos: Some(Ok(())),
            wifi: WifiState::Active(1),
        };
        assert_eq!(
            describe(&applied),
            "1 ms timers · QoS voice priority · Wi-Fi low-latency on 1 adapter(s)"
        );
        let failed = TuningStatus {
            timer: Some(Err("error 97".to_string())),
            qos: Some(Err("access denied".to_string())),
            wifi: WifiState::Refused("streaming error 5, scan error 5".to_string()),
        };
        assert_eq!(
            describe(&failed),
            "default timers (error 97) · QoS unavailable (access denied) · \
             Wi-Fi low-latency refused (streaming error 5, scan error 5)"
        );
        let desktop = TuningStatus {
            wifi: WifiState::Unavailable("no Wi-Fi adapter".to_string()),
            ..applied
        };
        assert!(describe(&desktop).ends_with("no Wi-Fi tuning (no Wi-Fi adapter)"));
    }

    #[test]
    fn only_real_changes_are_reported() {
        // Serialised with the other users of the global status by the lock;
        // start from a known state.
        update(|status| *status = TuningStatus::default());
        let _ = take_changed();
        update(|status| status.wifi = WifiState::Idle);
        assert!(
            !take_changed(),
            "an unchanged status must not re-send diagnostics"
        );
        update(|status| status.wifi = WifiState::Active(2));
        assert!(take_changed());
        assert!(!take_changed(), "the change is consumed");
        update(|status| *status = TuningStatus::default());
        let _ = take_changed();
    }
}
