//! Caps Lock / Num Lock kept in sync between connected machines.
//!
//! Each machine watches its own lock state and tells its peers when it
//! changes ([`ProtoEvent::LockState`](lan_mouse_proto::ProtoEvent)); a machine
//! that receives a different state toggles its own lock by injecting the lock
//! key through its input-emulation backend. Injection is what makes the LED of
//! the local keyboard follow, and it works the same on every backend (libei,
//! wlroots, X11, the RemoteDesktop portal, Windows).
//!
//! Only *reading* the state is OS specific:
//!
//! - Linux: the keyboard LEDs in `/sys/class/leds/*::{capslock,numlock}`.
//!   The compositor or X server drives those LEDs from its keyboard state, so
//!   this works on GNOME, KDE, wlroots compositors and X11 alike, without
//!   privileges.
//! - Windows: the toggle bit of `GetKeyState`.
//!
//! The decision logic is [`LockSync`], a pure state machine.

use lan_mouse_proto::lock_bits;
use std::time::{Duration, Instant};

/// Linux key codes of the lock keys (input-event codes).
pub(crate) const KEY_CAPSLOCK: u32 = 58;
pub(crate) const KEY_NUMLOCK: u32 = 69;

/// How often the local lock state is read.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long after injecting a lock key a matching local change is treated as
/// the echo of that injection instead of a user action.
const APPLY_ECHO_WINDOW: Duration = Duration::from_secs(2);

/// How long after injecting a lock key the capture must not forward lock keys
/// to a peer. Some capture backends see injected keys while they capture
/// (Windows forwards injected keys; a compositor may route EIS input through
/// its capture), and forwarding our own toggle would flip the peer back.
pub(crate) const INJECTED_KEY_SUPPRESS: Duration = Duration::from_millis(500);

/// Lock state of one machine. Only the bits in `known` are meaningful.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Locks {
    pub known: u8,
    pub on: u8,
}

impl Locks {
    fn masked(self, mask: u8) -> Self {
        let known = self.known & mask;
        Self {
            known,
            on: self.on & known,
        }
    }
}

/// Keys whose locks differ between `current` and `target`, restricted to the
/// locks both sides know.
fn toggles(current: Locks, target: Locks) -> Vec<u32> {
    let comparable = current.known & target.known;
    let differ = (current.on ^ target.on) & comparable;
    let mut keys = Vec::new();
    if differ & lock_bits::CAPS != 0 {
        keys.push(KEY_CAPSLOCK);
    }
    if differ & lock_bits::NUM != 0 {
        keys.push(KEY_NUMLOCK);
    }
    keys
}

/// Decides when to tell peers about a lock change and when to toggle a local
/// lock.
///
/// Loop prevention: a machine only broadcasts changes it did not cause. After
/// applying a remote state it expects to observe exactly that state; seeing it
/// (within [`APPLY_ECHO_WINDOW`]) is not re-broadcast, so the exchange stops
/// after one round-trip.
#[derive(Debug)]
pub(crate) struct LockSync {
    /// Locks taking part in the sync (configuration).
    mask: u8,
    /// Last observed local state.
    local: Option<Locks>,
    /// State we toggled towards and when, until it is observed.
    applied: Option<(Locks, Instant)>,
}

impl LockSync {
    pub(crate) fn new(mask: u8) -> Self {
        Self {
            mask,
            local: None,
            applied: None,
        }
    }

    /// A fresh local reading. Returns the state to broadcast when the user (or
    /// another program) changed a lock here.
    pub(crate) fn observe_local(&mut self, observed: Locks, now: Instant) -> Option<Locks> {
        let observed = observed.masked(self.mask);
        let previous = self.local.replace(observed);
        if previous == Some(observed) {
            return None;
        }
        if let Some((target, at)) = self.applied {
            if now.saturating_duration_since(at) < APPLY_ECHO_WINDOW
                && toggles(observed, target).is_empty()
            {
                // The change we just injected: the peer already has it.
                self.applied = None;
                return None;
            }
            if now.saturating_duration_since(at) >= APPLY_ECHO_WINDOW {
                self.applied = None;
            }
        }
        // The first reading only establishes the baseline: nothing changed.
        previous.map(|_| observed)
    }

    /// The peer reported its state. Returns the lock keys to inject so this
    /// machine matches it.
    pub(crate) fn on_remote(&mut self, remote: Locks, now: Instant) -> Vec<u32> {
        let remote = remote.masked(self.mask);
        let Some(local) = self.local else {
            return Vec::new();
        };
        // While a toggle is in flight the local reading is stale: compare
        // against the state we are already moving to, so a repeated message
        // does not toggle twice.
        let current = match self.applied {
            Some((target, at)) if now.saturating_duration_since(at) < APPLY_ECHO_WINDOW => target,
            _ => local,
        };
        let keys = toggles(current, remote);
        if !keys.is_empty() {
            let comparable = current.known & remote.known;
            let target = Locks {
                known: current.known,
                on: (current.on & !comparable) | (remote.on & comparable),
            };
            self.applied = Some((target, now));
        }
        keys
    }

    /// State to send to a peer the pointer just moved to: the machine whose
    /// keyboard the user is typing on wins the hand-off.
    pub(crate) fn for_handoff(&self) -> Option<Locks> {
        self.local.filter(|locks| locks.known != 0)
    }
}

/// Read the local lock state, or `None` when this OS/machine cannot report it.
pub(crate) fn read_local() -> Option<Locks> {
    imp::read()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Locks;
    use lan_mouse_proto::lock_bits;
    use std::path::{Path, PathBuf};

    /// Test hook: read LEDs from another directory (e.g. two isolated
    /// instances on one machine).
    const LEDS_DIR_ENV: &str = "LAN_MOUSE_LEDS_DIR";

    pub(super) fn read() -> Option<Locks> {
        let dir = std::env::var_os(LEDS_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/sys/class/leds"));
        read_dir(&dir)
    }

    /// A lock is on when any keyboard reports its LED lit; it is known when at
    /// least one keyboard exposes that LED.
    pub(super) fn read_dir(dir: &Path) -> Option<Locks> {
        let mut locks = Locks::default();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let bit = if name.ends_with("::capslock") {
                lock_bits::CAPS
            } else if name.ends_with("::numlock") {
                lock_bits::NUM
            } else {
                continue;
            };
            let Ok(brightness) = std::fs::read_to_string(entry.path().join("brightness")) else {
                continue;
            };
            locks.known |= bit;
            if brightness
                .trim()
                .parse::<u32>()
                .is_ok_and(|value| value > 0)
            {
                locks.on |= bit;
            }
        }
        (locks.known != 0).then_some(locks)
    }
}

#[cfg(windows)]
mod imp {
    use super::Locks;
    use lan_mouse_proto::lock_bits;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL, VK_NUMLOCK};

    pub(super) fn read() -> Option<Locks> {
        // The low-order bit of GetKeyState is the toggle state of a lock key.
        let toggled = |key: u16| unsafe { GetKeyState(i32::from(key)) } & 1 != 0;
        let mut on = 0;
        if toggled(VK_CAPITAL.0) {
            on |= lock_bits::CAPS;
        }
        if toggled(VK_NUMLOCK.0) {
            on |= lock_bits::NUM;
        }
        Some(Locks {
            known: lock_bits::CAPS | lock_bits::NUM,
            on,
        })
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::Locks;

    pub(super) fn read() -> Option<Locks> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: u8 = lock_bits::CAPS | lock_bits::NUM;

    fn locks(on: u8) -> Locks {
        Locks { known: BOTH, on }
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn the_first_reading_is_a_baseline_and_changes_are_broadcast() {
        let base = Instant::now();
        let mut sync = LockSync::new(BOTH);
        assert_eq!(sync.observe_local(locks(0), base), None);
        assert_eq!(sync.observe_local(locks(0), at(base, 100)), None);
        assert_eq!(
            sync.observe_local(locks(lock_bits::CAPS), at(base, 200)),
            Some(locks(lock_bits::CAPS)),
            "the user pressed Caps Lock here"
        );
    }

    #[test]
    fn a_remote_change_toggles_the_differing_locks_once() {
        let base = Instant::now();
        let mut sync = LockSync::new(BOTH);
        sync.observe_local(locks(lock_bits::NUM), base);
        assert_eq!(
            sync.on_remote(locks(lock_bits::CAPS | lock_bits::NUM), at(base, 10)),
            vec![KEY_CAPSLOCK]
        );
        // The same message again (retransmission, hand-off) before the local
        // reading caught up must not toggle a second time.
        assert!(
            sync.on_remote(locks(lock_bits::CAPS | lock_bits::NUM), at(base, 20))
                .is_empty()
        );
        // Both locks differ: both keys, Caps first.
        let mut sync = LockSync::new(BOTH);
        sync.observe_local(locks(0), base);
        assert_eq!(
            sync.on_remote(locks(BOTH), base),
            vec![KEY_CAPSLOCK, KEY_NUMLOCK]
        );
    }

    /// The loop guard: the change we injected is observed locally and must
    /// not be sent back to the peer (which would make it toggle again).
    #[test]
    fn the_echo_of_an_applied_change_is_not_broadcast() {
        let base = Instant::now();
        let mut sync = LockSync::new(BOTH);
        sync.observe_local(locks(0), base);
        assert_eq!(
            sync.on_remote(locks(lock_bits::CAPS), at(base, 10)),
            vec![KEY_CAPSLOCK]
        );
        assert_eq!(
            sync.observe_local(locks(lock_bits::CAPS), at(base, 60)),
            None,
            "our own toggle must not bounce back"
        );
        // A later user change is broadcast again.
        assert_eq!(sync.observe_local(locks(0), at(base, 900)), Some(locks(0)));
    }

    #[test]
    fn a_user_change_during_the_echo_window_is_still_broadcast() {
        let base = Instant::now();
        let mut sync = LockSync::new(BOTH);
        sync.observe_local(locks(0), base);
        sync.on_remote(locks(lock_bits::CAPS), base);
        // The user toggled Num Lock right away: not the expected state.
        assert_eq!(
            sync.observe_local(locks(lock_bits::NUM), at(base, 50)),
            Some(locks(lock_bits::NUM))
        );
    }

    #[test]
    fn a_stale_injection_expectation_expires() {
        let base = Instant::now();
        let mut sync = LockSync::new(BOTH);
        sync.observe_local(locks(0), base);
        sync.on_remote(locks(lock_bits::CAPS), base);
        // The injection never took effect (emulation unavailable). Much later
        // the user switches Caps Lock on: that is a real change.
        assert_eq!(
            sync.observe_local(locks(lock_bits::CAPS), at(base, 5_000)),
            Some(locks(lock_bits::CAPS))
        );
        // And a remote state is compared with the real reading again.
        assert_eq!(
            sync.on_remote(locks(0), at(base, 5_100)),
            vec![KEY_CAPSLOCK]
        );
    }

    #[test]
    fn only_locks_both_sides_know_are_synced() {
        let base = Instant::now();
        let mut sync = LockSync::new(BOTH);
        // This machine has no Num Lock LED.
        sync.observe_local(
            Locks {
                known: lock_bits::CAPS,
                on: 0,
            },
            base,
        );
        assert!(
            sync.on_remote(locks(lock_bits::NUM), base).is_empty(),
            "an unknown local Num Lock must not be toggled blindly"
        );
        assert_eq!(
            sync.on_remote(locks(lock_bits::CAPS), base),
            vec![KEY_CAPSLOCK]
        );
    }

    #[test]
    fn a_disabled_lock_is_neither_sent_nor_applied() {
        let base = Instant::now();
        let mut sync = LockSync::new(lock_bits::CAPS);
        sync.observe_local(locks(0), base);
        assert_eq!(
            sync.observe_local(locks(lock_bits::NUM), at(base, 100)),
            None,
            "Num Lock is not synced"
        );
        assert!(
            sync.on_remote(locks(lock_bits::NUM), at(base, 200))
                .is_empty()
        );
        assert_eq!(
            sync.for_handoff(),
            Some(Locks {
                known: lock_bits::CAPS,
                on: 0
            })
        );
    }

    #[test]
    fn nothing_is_applied_before_the_local_state_is_known() {
        let mut sync = LockSync::new(BOTH);
        assert!(sync.on_remote(locks(BOTH), Instant::now()).is_empty());
        assert_eq!(sync.for_handoff(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_leds_are_read_from_sysfs() {
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-leds-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let led = |name: &str, value: &str| {
            let path = dir.join(name);
            std::fs::create_dir_all(&path).expect("led dir");
            std::fs::write(path.join("brightness"), value).expect("brightness");
        };
        assert_eq!(imp::read_dir(&dir), None, "missing directory");
        led("input2::capslock", "0\n");
        led("input11::capslock", "1\n");
        led("input2::numlock", "0\n");
        led("input2::scrolllock", "1\n");
        led("phy0-led", "1\n");
        assert_eq!(
            imp::read_dir(&dir),
            Some(Locks {
                known: BOTH,
                on: lock_bits::CAPS
            }),
            "any keyboard with the LED lit means the lock is on"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs on the Windows CI runner (interactive desktop session): toggling
    /// Caps Lock with SendInput must be visible to the reader, and the
    /// original state is restored.
    #[cfg(windows)]
    #[test]
    fn windows_reader_sees_a_caps_lock_toggle() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_CAPITAL,
        };
        fn tap_caps() {
            let key = |flags| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_CAPITAL,
                        dwFlags: flags,
                        ..Default::default()
                    },
                },
            };
            let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
            unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
            std::thread::sleep(Duration::from_millis(100));
        }
        let before = read_local().expect("windows always reports its locks");
        tap_caps();
        let after = read_local().expect("locks");
        tap_caps();
        assert_ne!(
            before.on & lock_bits::CAPS,
            after.on & lock_bits::CAPS,
            "the reader must see Caps Lock toggled by SendInput"
        );
        assert_eq!(read_local(), Some(before), "state restored");
    }
}
