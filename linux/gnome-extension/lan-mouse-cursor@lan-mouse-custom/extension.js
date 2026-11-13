// Hides the GNOME cursor while Lan Mouse's capture is active.
//
// The service keeps a heartbeat file in the user's runtime directory while a
// peer is being controlled. Freshness is checked here as well, so a crashed
// service cannot leave the cursor hidden forever.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

const HEARTBEAT_MS = 3_000;
const EXTENSION_VERSION = '6';
const POLL_MS = 50;
// Ignore pointer movement right after a hand-off: the peer may still deliver
// a few injected motion events while it releases its capture. Requiring two
// consecutive polls beyond this distance also filters remote jitter.
const PARK_SETTLE_MS = 250;
const PARK_MOVE_PX = 12;
const PARK_CONFIRM_POLLS = 2;

export default class LanMouseCursorExtension extends Extension {
    enable() {
        // Resolve the tracker lazily: extensions are enabled early during
        // Shell startup, and `global.backend` may not be ready at this exact
        // moment. A one-shot lookup here could silently leave cursor hiding
        // disabled for the whole session.
        this._cursorTracker = null;
        this._trackerError = '';
        this._inhibited = false;
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
        this._stateFile = null;
        this._setInhibited(false);
        this._cursorTracker = null;
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
        return this._cursorTracker !== null;
    }

    _writeStatus(state) {
        if (!this._runtimeDir) {
            return;
        }
        const status = `tracker=${this._cursorTracker ? 1 : 0} inhibited=${
            this._inhibited ? 1 : 0
        } state=${state} error=${this._trackerError}`;
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
                this._cursorTracker.inhibit_cursor_visibility();
                this._inhibited = true;
                this._writeStatus('inhibited');
            } else if (!inhibit && this._inhibited) {
                this._cursorTracker.uninhibit_cursor_visibility();
                this._inhibited = false;
                this._writeStatus('visible');
            }
        } catch (error) {
            this._trackerError = String(error);
            this._cursorTracker = null;
            this._inhibited = false;
            console.error(`Lan Mouse cursor extension: ${error}`);
            this._writeStatus('error');
        }
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
                    // Hide on the first observation: waiting for the next
                    // poll adds a visible delay to the return hand-off.
                    hide = true;
                } else if (Date.now() - this._parkSeenAt < PARK_SETTLE_MS) {
                    hide = true;
                } else {
                    const dx = x - this._parkOrigin[0];
                    const dy = y - this._parkOrigin[1];
                    if (Math.hypot(dx, dy) > PARK_MOVE_PX) {
                        this._parkAwayPolls += 1;
                        if (this._parkAwayPolls >= PARK_CONFIRM_POLLS) {
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
