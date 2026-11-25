// Hides the GNOME cursor while Lan Mouse's capture is active.
//
// The service keeps a heartbeat file in the user's runtime directory while a
// peer is being controlled. Freshness is checked here as well, so a crashed
// service cannot leave the cursor hidden forever.
//
// Reacting is event driven: a directory monitor sees the service's marker
// files change and the cursor tracker reports pointer movement, so hiding on
// a hand-off and showing on the return happen at once. The poll below only
// expires a stale heartbeat and backs the events up.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

const HEARTBEAT_MS = 3_000;
const EXTENSION_VERSION = '9';
const POLL_MS = 50;
// Ignore pointer movement right after a hand-off: the peer may still deliver
// a few injected motion events while it releases its capture. Requiring two
// consecutive polls beyond this distance also filters remote jitter.
const PARK_SETTLE_MS = 250;
const PARK_MOVE_PX = 12;
const PARK_CONFIRM_POLLS = 2;
// Content of the park marker when this machine's own capture just ended
// (its user moved back from the peer). No peer injects motion here, so the
// cursor is shown on the first poll that sees real movement: with the
// settle/confirm delays above a fast hand appeared only mid-screen.
const PARK_KIND_RETURN = 'return';
// After an own return any real movement shows the cursor: the hand is still
// moving back, and no peer injects motion that could be mistaken for it.
const PARK_MOVE_PX_RETURN = 0;
const MARKERS = ['cursor-hide', 'cursor-park'];

export default class LanMouseCursorExtension extends Extension {
    enable() {
        // Resolve the tracker lazily: extensions are enabled early during
        // Shell startup, and `global.backend` may not be ready at this exact
        // moment. A one-shot lookup here could silently leave cursor hiding
        // disabled for the whole session.
        this._cursorTracker = null;
        this._trackerError = '';
        this._inhibited = false;
        // GNOME 50: the plain visibility inhibitor is undone by the next
        // pointer event (the cursor renderer is re-attached and shown again).
        // The Shell's own screen shield/magnifier also keep the seat's
        // "unfocus inhibited" refcount up while hiding; without it the
        // cursor sprite is detached on hide and re-shown on the first motion.
        this._seat = null;
        this._seatUnfocusInhibited = false;
        this._seatError = '';
        this._lastStatus = '';

        const runtime = `${GLib.get_user_runtime_dir()}/lan-mouse`;
        this._runtimeDir = runtime;
        this._stateFile = Gio.File.new_for_path(`${runtime}/cursor-hide`);
        // Written by the service when a remote peer hands control back. The
        // cursor is usually parked at the edge at that point; hide it there,
        // but restore it as soon as the local pointer moves.
        this._parkFile = Gio.File.new_for_path(`${runtime}/cursor-park`);
        this._parkOrigin = null;
        this._parkSeenAt = 0;
        this._parkAwayPolls = 0;
        this._parkSettleMs = PARK_SETTLE_MS;
        this._parkConfirmPolls = PARK_CONFIRM_POLLS;
        this._parkMovePx = PARK_MOVE_PX;
        this._positionId = 0;
        this._loadedFile = Gio.File.new_for_path(
            `${runtime}/cursor-extension-loaded`
        );
        // Lets the app distinguish "enabled in GSettings" from "loaded by the
        // running Shell"; without a logout/login the file does not exist yet.
        try {
            GLib.mkdir_with_parents(runtime, 0o700);
            GLib.file_set_contents(this._loadedFile.get_path(), EXTENSION_VERSION);
        } catch (_error) {}
        this._writeStatus('loaded');

        // Marker changes arrive at once instead of on the next poll tick.
        try {
            this._monitor = Gio.File.new_for_path(runtime).monitor_directory(
                Gio.FileMonitorFlags.NONE,
                null
            );
            this._monitor.set_rate_limit(0);
            this._monitor.connect('changed', (_monitor, file) => {
                if (MARKERS.includes(file.get_basename()))
                    this._sync();
            });
        } catch (_error) {
            this._monitor = null;
        }

        this._pollId = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT,
            POLL_MS,
            () => {
                this._sync();
                return GLib.SOURCE_CONTINUE;
            }
        );
        this._sync();
    }

    disable() {
        if (this._pollId) {
            GLib.source_remove(this._pollId);
            this._pollId = 0;
        }
        if (this._monitor) {
            this._monitor.cancel();
            this._monitor = null;
        }
        if (this._positionId && this._cursorTracker) {
            this._cursorTracker.disconnect(this._positionId);
        }
        this._positionId = 0;
        this._stateFile = null;
        this._setInhibited(false);
        this._uninhibitSeat();
        this._cursorTracker = null;
        this._seat = null;
        this._runtimeDir = null;
        if (this._loadedFile) {
            try {
                this._loadedFile.delete(null);
            } catch (_error) {}
            this._loadedFile = null;
        }
    }

    _resolveTracker() {
        if (this._cursorTracker) {
            return true;
        }
        try {
            if (global.backend?.get_cursor_tracker) {
                this._cursorTracker = global.backend.get_cursor_tracker();
            } else if (global.display?.get_cursor_tracker) {
                this._cursorTracker = global.display.get_cursor_tracker();
            }
        } catch (error) {
            this._trackerError = String(error);
        }
        if (!this._cursorTracker && !this._trackerError) {
            this._trackerError = 'Mutter CursorTracker unavailable';
        }
        if (this._cursorTracker && !this._positionId) {
            // Pointer movement while parked shows the cursor at once.
            try {
                this._positionId = this._cursorTracker.connect(
                    'position-invalidated',
                    () => {
                        if (this._parkOrigin)
                            this._sync();
                    }
                );
            } catch (_error) {
                this._positionId = 0;
            }
        }
        return this._cursorTracker !== null;
    }

    _writeStatus(state) {
        if (!this._runtimeDir) {
            return;
        }
        const status = `tracker=${this._cursorTracker ? 1 : 0} inhibited=${
            this._inhibited ? 1 : 0
        } seat=${this._seatUnfocusInhibited ? 1 : 0} state=${state} error=${
            this._trackerError || this._seatError
        }`;
        if (status === this._lastStatus) {
            return;
        }
        this._lastStatus = status;
        try {
            GLib.file_set_contents(
                `${this._runtimeDir}/cursor-extension-status`,
                status
            );
        } catch (_error) {}
        if (!this._cursorTracker) {
            console.warn(`Lan Mouse cursor extension: ${status}`);
        }
    }

    _getSeat() {
        if (this._seat)
            return this._seat;
        try {
            this._seat = global.stage.context.get_backend().get_default_seat();
        } catch (error) {
            this._seatError = String(error);
        }
        return this._seat;
    }

    _uninhibitSeat() {
        if (!this._seatUnfocusInhibited)
            return;
        const seat = this._getSeat();
        if (seat) {
            try {
                seat.uninhibit_unfocus();
            } catch (error) {
                this._seatError = String(error);
            }
        }
        this._seatUnfocusInhibited = false;
    }

    _setInhibited(inhibit) {
        if (inhibit && !this._resolveTracker()) {
            this._writeStatus('no-tracker');
            return;
        }
        if (!this._cursorTracker) {
            return;
        }
        try {
            if (inhibit && !this._inhibited) {
                // Keep the pointer focus-surface attached while the cursor is
                // hidden. On GNOME 50 hiding detaches the cursor renderer; the
                // next pointer event re-attaches it and shows the cursor again
                // (the Shell's screen shield/magnifier hold the same seat
                // refcount for exactly this reason).
                const seat = this._getSeat();
                if (seat) {
                    seat.inhibit_unfocus();
                    this._seatUnfocusInhibited = true;
                }
                this._cursorTracker.inhibit_cursor_visibility();
                this._inhibited = true;
                this._writeStatus('inhibited');
            } else if (!inhibit && this._inhibited) {
                this._cursorTracker.uninhibit_cursor_visibility();
                this._inhibited = false;
                this._uninhibitSeat();
                this._writeStatus('visible');
            }
        } catch (error) {
            this._trackerError = String(error);
            this._cursorTracker = null;
            this._inhibited = false;
            this._uninhibitSeat();
            console.error(`Lan Mouse cursor extension: ${error}`);
            this._writeStatus('error');
        }
    }

    _readParkKind() {
        try {
            const [ok, bytes] = GLib.file_get_contents(this._parkFile.get_path());
            if (ok)
                return new TextDecoder().decode(bytes).trim();
        } catch (_error) {}
        return '';
    }

    _sync() {
        let hide = false;
        try {
            const info = this._stateFile.query_info(
                'time::modified',
                Gio.FileQueryInfoFlags.NONE,
                null
            );
            const modified = info.get_attribute_uint64('time::modified');
            const now = GLib.get_real_time() / 1_000_000;
            hide = now - modified < HEARTBEAT_MS / 1000;
        } catch (_error) {
            hide = false;
        }

        // A cursor left parked at the edge after returning control is hidden
        // until the local pointer actually moves. This keeps the desktop
        // usable: any physical mouse movement restores the pointer.
        try {
            if (this._parkFile.query_exists(null)) {
                const [x, y] = global.get_pointer();
                if (!this._parkOrigin) {
                    this._parkOrigin = [x, y];
                    this._parkSeenAt = Date.now();
                    this._parkAwayPolls = 0;
                    const ownReturn = this._readParkKind() === PARK_KIND_RETURN;
                    this._parkSettleMs = ownReturn ? 0 : PARK_SETTLE_MS;
                    this._parkConfirmPolls = ownReturn ? 1 : PARK_CONFIRM_POLLS;
                    this._parkMovePx = ownReturn ? PARK_MOVE_PX_RETURN : PARK_MOVE_PX;
                    // Hide on the first observation: waiting for the next
                    // poll adds a visible delay to the return hand-off.
                    hide = true;
                } else if (Date.now() - this._parkSeenAt < this._parkSettleMs) {
                    hide = true;
                } else {
                    const dx = x - this._parkOrigin[0];
                    const dy = y - this._parkOrigin[1];
                    if (Math.hypot(dx, dy) > this._parkMovePx) {
                        this._parkAwayPolls += 1;
                        if (this._parkAwayPolls >= this._parkConfirmPolls) {
                            try {
                                this._parkFile.delete(null);
                            } catch (_error) {}
                            this._parkOrigin = null;
                            this._parkAwayPolls = 0;
                        } else {
                            hide = true;
                        }
                    } else {
                        this._parkAwayPolls = 0;
                        hide = true;
                    }
                }
            } else {
                this._parkOrigin = null;
                this._parkAwayPolls = 0;
            }
        } catch (_error) {
            this._parkOrigin = null;
        }

        this._setInhibited(hide);
    }
}
