// Hides the GNOME cursor while Lan Mouse's capture is active.
//
// The service keeps a heartbeat file in the user's runtime directory while a
// peer is being controlled. Freshness is checked here as well, so a crashed
// service cannot leave the cursor hidden forever.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

const HEARTBEAT_MS = 3_000;
const EXTENSION_VERSION = '2';
const POLL_MS = 100;

export default class LanMouseCursorExtension extends Extension {
    enable() {
        // Clutter.Stage has no global cursor API; Mutter's CursorTracker can
        // inhibit cursor visibility for the compositor.
        try {
            this._cursorTracker = global.backend.get_cursor_tracker();
        } catch (_error) {
            this._cursorTracker = null;
        }
        this._inhibited = false;

        const runtime = `${GLib.get_user_runtime_dir()}/lan-mouse`;
        this._stateFile = Gio.File.new_for_path(`${runtime}/cursor-hide`);
        this._loadedFile = Gio.File.new_for_path(
            `${runtime}/cursor-extension-loaded`
        );
        // Lets the app distinguish "enabled in GSettings" from "loaded by the
        // running Shell"; without a logout/login the file does not exist yet.
        try {
            GLib.mkdir_with_parents(runtime, 0o700);
            GLib.file_set_contents(this._loadedFile.get_path(), EXTENSION_VERSION);
        } catch (_error) {}

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
        if (this._loadedFile) {
            try {
                this._loadedFile.delete(null);
            } catch (_error) {}
            this._loadedFile = null;
        }
    }

    _setInhibited(inhibit) {
        if (!this._cursorTracker) {
            return;
        }
        try {
            if (inhibit && !this._inhibited) {
                this._cursorTracker.inhibit_cursor_visibility();
                this._inhibited = true;
            } else if (!inhibit && this._inhibited) {
                this._cursorTracker.uninhibit_cursor_visibility();
                this._inhibited = false;
            }
        } catch (error) {
            console.error(`Lan Mouse cursor extension: ${error}`);
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
        this._setInhibited(hide);
    }
}
