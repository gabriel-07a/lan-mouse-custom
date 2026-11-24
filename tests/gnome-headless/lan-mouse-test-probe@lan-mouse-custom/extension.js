// Test-only probe for scripts/gnome-headless.sh: every 20 ms it writes the
// pointer position, the primary monitor's geometry and the Ubuntu Dock state to
// $LAN_MOUSE_TEST_PROBE (JSON, replaced atomically), so a test outside the
// shell can tell what the user would have seen.
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const DOCK = 'file:///usr/share/gnome-shell/extensions/ubuntu-dock@ubuntu.com/docking.js';

export default class TestProbe extends Extension {
    enable() {
        this._path = GLib.getenv('LAN_MOUSE_TEST_PROBE');
        if (!this._path)
            return;
        this._dockShownEver = false;
        this._docking = null;
        import(DOCK).then(module => {
            this._docking = module;
        }).catch(() => {});
        // GNOME starts in the overview, where the dock is always shown; leave
        // it once, like a user going to the desktop.
        const leaveOverview = () => Main.overview.hide();
        if (Main.layoutManager._startingUp)
            this._startup = Main.layoutManager.connect('startup-complete', leaveOverview);
        else
            leaveOverview();
        this._timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 20, () => {
            this._write();
            return GLib.SOURCE_CONTINUE;
        });
    }

    disable() {
        if (this._timer)
            GLib.source_remove(this._timer);
        this._timer = 0;
        if (this._startup)
            Main.layoutManager.disconnect(this._startup);
        this._startup = 0;
    }

    _dockState() {
        // DockManager is created by ubuntu-dock; State: 0 hidden, 1 showing,
        // 2 shown, 3 hiding. -1: the dock is not loaded (yet).
        const dock = this._docking?.DockManager?.getDefault?.()?.mainDock;
        return dock ? dock._dockState : -1;
    }

    _dockSettings() {
        try {
            const settings = this._docking?.DockManager?.settings;
            return {
                dockFixed: settings?.dockFixed ?? null,
                autohide: settings?.autohide ?? null,
                intellihide: settings?.intellihide ?? null,
            };
        } catch {
            return {};
        }
    }

    _write() {
        const [x, y] = global.get_pointer();
        const monitor = Main.layoutManager.primaryMonitor;
        const dock = this._dockState();
        if (dock === 1 || dock === 2)
            this._dockShownEver = true;
        const state = {
            x, y,
            mx: monitor?.x ?? 0,
            my: monitor?.y ?? 0,
            width: monitor?.width ?? 0,
            height: monitor?.height ?? 0,
            dock,
            dockShownEver: this._dockShownEver,
            ...this._dockSettings(),
            overview: Main.overview.visible,
            startupComplete: !Main.layoutManager._startingUp && !Main.overview.visible,
        };
        try {
            const tmp = `${this._path}.tmp`;
            GLib.file_set_contents(tmp, JSON.stringify(state));
            Gio.File.new_for_path(tmp).move(
                Gio.File.new_for_path(this._path), Gio.FileCopyFlags.OVERWRITE, null, null);
        } catch (e) {
            logError(e, 'lan-mouse test probe');
        }
    }
}
