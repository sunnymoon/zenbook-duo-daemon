/**
 * Zenbook Duo Keyboard Battery — GNOME Shell Extension
 *
 * Reads keyboard battery state from the zenbook-duo-daemon over D-Bus
 * (system bus, service "asus.zenbook.duo") and exposes it as:
 *
 *   • A panel icon + percentage label in the top-bar status area.
 *   • A Quick Settings card in the drop-down panel.
 *
 * D-Bus interface (system bus):
 *   Service   : asus.zenbook.duo
 *   Path      : /asus/zenbook/duo/State
 *   Interface : asus.zenbook.duo.State
 *
 * Properties consumed:
 *   KeyboardUsbConnected
 *   KeyboardPogoDocked
 *   BluetoothConnected
 *   KeyboardBatteryPresent
 *   KeyboardBatteryPercentage
 *   KeyboardBatteryCharging
 *   KeyboardBatteryFull
 *   DesiredPrimary
 *   DesiredSecondaryEnabled
 *   DesiredDisplayAttachment
 *   DesiredDisplayLayout
 *   DisplayBrightness
 *   DisplayApplyPaused
 *   TabletMappingEnabled
 *   TabletMappingMode
 *   TabletMappingApplyNonce
 *   SessionRegistered
 *   SessionOwner
 *   SessionId
 *   SessionLastSeenUsec
 *   MicMuteLed
 *   KeyboardBacklightLevel
 *
 * Methods used by UI actions:
 *   ToggleMicMuteLed
 *   SetMicMuteLed(bool)
 *   SetKeyboardBacklightLevel(y)
 *   SetSecondaryDisplayDesired(bool)
 *   SetDesiredPrimary(string)
 *   SetTabletMappingEnabled(bool)
 *   SetTabletMappingMode(string)
 *   ToggleTabletMappingMode()
 *   ApplyTabletMapping()
 */

import GObject from 'gi://GObject';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import St from 'gi://St';
import Clutter from 'gi://Clutter';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {
    SystemIndicator,
    QuickMenuToggle,
} from 'resource:///org/gnome/shell/ui/quickSettings.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

// ── D-Bus constants ────────────────────────────────────────────────────────
const DBUS_SERVICE   = 'asus.zenbook.duo';
const DBUS_PATH      = '/asus/zenbook/duo/State';
const DBUS_INTERFACE = 'asus.zenbook.duo.State';

// Minimal introspection XML — only the properties we need.
const DBUS_XML = `
<node>
  <interface name="asus.zenbook.duo.State">
    <method name="ToggleMicMuteLed"/>
    <method name="SetMicMuteLed">
      <arg name="enabled" type="b" direction="in"/>
    </method>
    <method name="SetKeyboardBacklightLevel">
      <arg name="level" type="y" direction="in"/>
    </method>
    <method name="SetSecondaryDisplayDesired">
      <arg name="enabled" type="b" direction="in"/>
    </method>
    <method name="SetDesiredPrimary">
      <arg name="primary" type="s" direction="in"/>
    </method>
    <method name="SetTabletMappingEnabled">
      <arg name="enabled" type="b" direction="in"/>
    </method>
    <method name="SetTabletMappingMode">
      <arg name="mode" type="s" direction="in"/>
    </method>
    <method name="ToggleTabletMappingMode"/>
    <method name="ApplyTabletMapping"/>
    <property name="KeyboardUsbConnected" type="b" access="read"/>
    <property name="KeyboardPogoDocked" type="b" access="read"/>
    <property name="BluetoothConnected" type="b" access="read"/>
    <property name="KeyboardBatteryPresent" type="b" access="read"/>
    <property name="KeyboardBatteryPercentage" type="y" access="read"/>
    <property name="KeyboardBatteryCharging" type="b" access="read"/>
    <property name="KeyboardBatteryFull" type="b" access="read"/>
    <property name="DesiredPrimary" type="s" access="read"/>
    <property name="DesiredSecondaryEnabled" type="b" access="read"/>
    <property name="MicMuteLed" type="b" access="read"/>
    <property name="KeyboardBacklightLevel" type="y" access="read"/>
    <property name="DesiredDisplayAttachment" type="s" access="read"/>
    <property name="DesiredDisplayLayout" type="s" access="read"/>
    <property name="DisplayBrightness" type="u" access="read"/>
    <property name="DisplayApplyPaused" type="b" access="read"/>
    <property name="TabletMappingEnabled" type="b" access="read"/>
    <property name="TabletMappingMode" type="s" access="read"/>
    <property name="TabletMappingApplyNonce" type="u" access="read"/>
    <property name="SessionRegistered" type="b" access="read"/>
    <property name="SessionOwner" type="s" access="read"/>
    <property name="SessionId" type="t" access="read"/>
    <property name="SessionLastSeenUsec" type="t" access="read"/>
  </interface>
</node>`;

const BACKLIGHT_LABELS = ['Off', 'Low', 'Medium', 'High'];

// ── Battery icon helpers ───────────────────────────────────────────────────

/**
 * Returns a Adwaita/symbolic icon name that matches the battery level and
 * charging state, following the same naming convention GNOME uses for the
 * built-in battery indicator.
 *
 * @param {number}  pct       0-100
 * @param {boolean} charging  side-USB charge in progress
 * @param {boolean} full      battery is full / at threshold
 */
function batteryIconName(pct, charging, full) {
    if (full || pct >= 95)
        return charging ? 'battery-level-100-charging-symbolic' : 'battery-full-symbolic';

    // Round to nearest 10 for the levelled icons (10, 20, … 90)
    const level = Math.max(0, Math.min(100, Math.round(pct / 10) * 10));

    if (charging)
        return `battery-level-${level}-charging-symbolic`;

    if (pct <= 10)
        return 'battery-empty-symbolic';
    if (pct <= 20)
        return 'battery-caution-symbolic';

    return `battery-level-${level}-symbolic`;
}

/**
 * Human-readable status string shown inside the Quick Settings card.
 */
function batteryStatusText(pct, charging, full) {
    if (full)
        return `Full (${pct}%)`;
    if (charging)
        return `Charging — ${pct}%`;
    return `${pct}%`;
}

function connectionStateText(state) {
    if (state.keyboardPogoDocked)
        return 'Attached (pogo)';
    if (state.keyboardUsbConnected)
        return 'USB connected';
    if (state.bluetoothConnected)
        return 'Bluetooth mode';
    return 'Detached';
}

function variantValue(variant, fallback) {
    return variant ? variant.deepUnpack() : fallback;
}

function numberFromVariantValue(variant) {
    if (!variant)
        return null;
    const value = variant.deepUnpack();
    if (typeof value === 'bigint')
        return Number(value);
    return value;
}

function displayAttachmentText(value) {
    switch (value) {
    case 'builtin_only':
        return 'Built-in only';
    case 'external_only':
        return 'External only';
    case 'all_connected':
        return 'All connected';
    default:
        return value ?? 'unknown';
    }
}

function displayLayoutText(value) {
    switch (value) {
    case 'mirror':
        return 'Mirror';
    case 'joined':
        return 'Joined';
    default:
        return value ?? 'unknown';
    }
}

function tabletMappingModeText(value) {
    switch (value) {
    case 'one_to_one':
        return 'Match panels';
    case 'all_to_primary':
        return 'All to primary';
    default:
        return value ?? 'unknown';
    }
}

// ── Quick Settings card ────────────────────────────────────────────────────

const ZenbookKeyboardBatteryToggle = GObject.registerClass(
class ZenbookKeyboardBatteryToggle extends QuickMenuToggle {
    _init(extension) {
        super._init({
            title: 'Zenbook Keyboard',
            iconName: 'input-keyboard-symbolic',
            toggleMode: true,
        });
        this._extension = extension;
        this.menu.setHeader('input-keyboard-symbolic', 'Zenbook Duo');

        this._stateItem = new PopupMenu.PopupMenuItem('Keyboard: Detached', {
            reactive: false,
            can_focus: false,
        });
        this._batteryItem = new PopupMenu.PopupMenuItem('Battery: Unknown', {
            reactive: false,
            can_focus: false,
        });
        this._daemonStatusItem = new PopupMenu.PopupMenuItem('Root: unreachable', {
            reactive: false,
            can_focus: false,
        });
        this._daemonDetailItem = new PopupMenu.PopupMenuItem('Session: unknown', {
            reactive: false,
            can_focus: false,
        });
        this._availabilityItem = new PopupMenu.PopupMenuItem('Second display: available', {
            reactive: false,
            can_focus: false,
        });
        this._displayMenu = new PopupMenu.PopupSubMenuMenuItem('Display policy');
        this._displayPrimaryItem = new PopupMenu.PopupMenuItem('Primary: eDP-1', {
            reactive: false,
            can_focus: false,
        });
        this._displayAttachmentItem = new PopupMenu.PopupMenuItem('Attachment: builtin_only', {
            reactive: false,
            can_focus: false,
        });
        this._displayLayoutItem = new PopupMenu.PopupMenuItem('Layout: joined', {
            reactive: false,
            can_focus: false,
        });
        this._displayBrightnessItem = new PopupMenu.PopupMenuItem('Brightness: unknown', {
            reactive: false,
            can_focus: false,
        });
        this._displayPausedItem = new PopupMenu.PopupMenuItem('Apply paused: no', {
            reactive: false,
            can_focus: false,
        });
        this._primaryMenu = new PopupMenu.PopupSubMenuMenuItem('Primary panel');
        this._primaryMenuItems = new Map();
        for (const primary of ['eDP-1', 'eDP-2']) {
            const item = new PopupMenu.PopupMenuItem(primary);
            item.connect('activate', () => {
                this._extension.callMethod(
                    'SetDesiredPrimary',
                    GLib.Variant.new('(s)', [primary]),
                );
            });
            this._primaryMenu.menu.addMenuItem(item);
            this._primaryMenuItems.set(primary, item);
        }
        this._tabletMenu = new PopupMenu.PopupSubMenuMenuItem('Stylus / tablet mapping');
        this._tabletStatusItem = new PopupMenu.PopupMenuItem('Enabled: unknown', {
            reactive: false,
            can_focus: false,
        });
        this._tabletModeItem = new PopupMenu.PopupMenuItem('Mode: unknown', {
            reactive: false,
            can_focus: false,
        });
        this._tabletNonceItem = new PopupMenu.PopupMenuItem('Last apply nonce: unknown', {
            reactive: false,
            can_focus: false,
        });
        this._tabletRefreshItem = new PopupMenu.PopupMenuItem('Refresh pens now');
        this._tabletRefreshItem.connect('activate', () => {
            this._extension.callMethod('ApplyTabletMapping', GLib.Variant.new('()', []));
        });
        this._suppressTabletToggle = false;
        this._tabletEnableItem = new PopupMenu.PopupSwitchMenuItem('Stylus panel mapping', false);
        this._tabletEnableItem.connect('toggled', (_item, state) => {
            if (this._suppressTabletToggle)
                return;
            this._extension.callMethod(
                'SetTabletMappingEnabled',
                GLib.Variant.new('(b)', [state]),
            );
        });
        this._tabletModeMenu = new PopupMenu.PopupSubMenuMenuItem('Mapping mode');
        this._tabletModeItems = new Map();
        for (const mode of ['one_to_one', 'all_to_primary']) {
            const item = new PopupMenu.PopupMenuItem(tabletMappingModeText(mode));
            item.connect('activate', () => {
                this._extension.callMethod(
                    'SetTabletMappingMode',
                    GLib.Variant.new('(s)', [mode]),
                );
            });
            this._tabletModeMenu.menu.addMenuItem(item);
            this._tabletModeItems.set(mode, item);
        }
        this._tabletMenu.menu.addMenuItem(this._tabletStatusItem);
        this._tabletMenu.menu.addMenuItem(this._tabletModeItem);
        this._tabletMenu.menu.addMenuItem(this._tabletNonceItem);
        this._tabletMenu.menu.addMenuItem(this._tabletEnableItem);
        this._tabletMenu.menu.addMenuItem(this._tabletModeMenu);
        this._tabletMenu.menu.addMenuItem(this._tabletRefreshItem);
        this._suppressMicToggle = false;
        this._micItem = new PopupMenu.PopupSwitchMenuItem('Microphone muted (LED)', false);
        this._micItem.connect('toggled', (_item, state) => {
            if (this._suppressMicToggle)
                return;
            this._extension.callMethod(
                'SetMicMuteLed',
                GLib.Variant.new('(b)', [state]),
            );
        });
        this._backlightMenu = new PopupMenu.PopupSubMenuMenuItem('Keyboard backlight');
        this._backlightMenuItems = new Map();
        for (let level = 0; level <= 3; level++) {
            const item = new PopupMenu.PopupMenuItem(BACKLIGHT_LABELS[level]);
            item.connect('activate', () => {
                this._extension.callMethod(
                    'SetKeyboardBacklightLevel',
                    GLib.Variant.new('(y)', [level]),
                );
            });
            this._backlightMenu.menu.addMenuItem(item);
            this._backlightMenuItems.set(level, item);
        }

        this.menu.addMenuItem(this._stateItem);
        this.menu.addMenuItem(this._batteryItem);
        this.menu.addMenuItem(this._daemonStatusItem);
        this.menu.addMenuItem(this._daemonDetailItem);
        this.menu.addMenuItem(this._availabilityItem);
        this.menu.addMenuItem(this._displayMenu);
        this._displayMenu.menu.addMenuItem(this._displayPrimaryItem);
        this._displayMenu.menu.addMenuItem(this._displayAttachmentItem);
        this._displayMenu.menu.addMenuItem(this._displayLayoutItem);
        this._displayMenu.menu.addMenuItem(this._displayBrightnessItem);
        this._displayMenu.menu.addMenuItem(this._displayPausedItem);
        this._displayMenu.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._displayMenu.menu.addMenuItem(this._primaryMenu);
        this.menu.addMenuItem(this._tabletMenu);
        this.menu.addMenuItem(this._backlightMenu);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this.menu.addMenuItem(this._micItem);

        this.connect('clicked', () => {
            if (!this._extension.state)
                return;
            if (this._extension.state.keyboardPogoDocked)
                return;

            const target = !this.checked;
            this._extension.callMethod(
                'SetSecondaryDisplayDesired',
                GLib.Variant.new('(b)', [target]),
            );
        });
    }

    /**
     * Update the card contents from the latest daemon state.
     *
     * @param {boolean} present
     * @param {number}  pct       0-100
     * @param {boolean} charging
     * @param {boolean} full
     */
    update(state) {
        const connectedText = connectionStateText(state);
        const batteryText = state.keyboardBatteryPresent
            ? batteryStatusText(
                state.keyboardBatteryPercentage,
                state.keyboardBatteryCharging,
                state.keyboardBatteryFull,
            )
            : 'Unknown';
        const primaryText = `Primary: ${state.desiredPrimary}`;
        const attachmentText = `Attachment: ${displayAttachmentText(state.desiredDisplayAttachment)}`;
        const layoutText = `Layout: ${displayLayoutText(state.desiredDisplayLayout)}`;
        const brightnessText = `Brightness: ${
            state.displayBrightness === null ? 'unknown' : state.displayBrightness
        }`;
        const pausedText = `Apply paused: ${state.displayApplyPaused ? 'yes' : 'no'}`;
        const tabletEnabledText = `Enabled: ${
            state.tabletMappingEnabled === null
                ? 'unknown'
                : (state.tabletMappingEnabled ? 'yes' : 'no')
        }`;
        const tabletModeText = `Mode: ${tabletMappingModeText(state.tabletMappingMode)}`;
        const tabletNonceText = `Last apply nonce: ${
            state.tabletMappingApplyNonce === null ? 'unknown' : state.tabletMappingApplyNonce
        }`;
        const staleSession = state.sessionRegistered === true
            && state.sessionLastSeenUsec !== null
            && state.sessionLastSeenUsec > 0
            && (state.nowUsec - state.sessionLastSeenUsec) > 30_000_000;
        const tabletKnown = state.tabletMappingEnabled !== null
            && state.tabletMappingMode !== null
            && state.tabletMappingApplyNonce !== null;

        this.subtitle = `${connectedText} · ${batteryText}`;
        this._stateItem.label.text = `Keyboard: ${connectedText}`;
        this._batteryItem.label.text = `Battery: ${batteryText}`;
        if (!state.rootReachable) {
            this._daemonStatusItem.label.text = 'Root: unreachable';
            this._daemonDetailItem.label.text = 'Session: unknown';
        } else if (state.sessionRegistered === null) {
            this._daemonStatusItem.label.text = 'Root: up · Session: daemon update pending';
            this._daemonDetailItem.label.text = 'Session details unavailable';
        } else if (state.sessionRegistered) {
            this._daemonStatusItem.label.text = staleSession
                ? 'Root: up · Session: linked (session quiet)'
                : 'Root: up · Session: linked';
            const ownerText = state.sessionOwner || 'unknown owner';
            const idText = state.sessionId === null ? 'unknown id' : `id ${state.sessionId}`;
            this._daemonDetailItem.label.text = `${ownerText} · ${idText}`;
        } else {
            this._daemonStatusItem.label.text = 'Root: up · Session: not registered';
            this._daemonDetailItem.label.text = 'No active session daemon registration';
        }
        this._displayPrimaryItem.label.text = primaryText;
        this._displayAttachmentItem.label.text = attachmentText;
        this._displayLayoutItem.label.text = layoutText;
        this._displayBrightnessItem.label.text = brightnessText;
        this._displayPausedItem.label.text = pausedText;
        this._tabletStatusItem.label.text = tabletEnabledText;
        this._tabletModeItem.label.text = tabletModeText;
        this._tabletNonceItem.label.text = tabletNonceText;
        this._tabletMenu.label.text = tabletKnown
            ? 'Stylus / tablet mapping'
            : 'Stylus / tablet mapping (daemon pending)';
        this._tabletMenu.setSensitive(tabletKnown);

        if (state.keyboardBatteryPresent) {
            this.iconName = batteryIconName(
                state.keyboardBatteryPercentage,
                state.keyboardBatteryCharging,
                state.keyboardBatteryFull,
            );
        } else {
            this.iconName = 'input-keyboard-symbolic';
        }

        this.checked = state.desiredSecondaryEnabled;
        if (state.keyboardPogoDocked) {
            this.setSensitive(false);
            this._availabilityItem.label.text = 'Second display: unavailable while attached';
        } else {
            this.setSensitive(true);
            this._availabilityItem.label.text = 'Second display: available';
        }

        const hasLink = state.keyboardUsbConnected || state.keyboardPogoDocked || state.bluetoothConnected;
        const backlightKnown = state.keyboardBacklightLevel !== null;
        this._backlightMenu.setSensitive(hasLink && backlightKnown);
        this._backlightMenu.label.text = backlightKnown
            ? `Keyboard backlight (${BACKLIGHT_LABELS[state.keyboardBacklightLevel]})`
            : 'Keyboard backlight (daemon update pending)';
        for (const [level, item] of this._backlightMenuItems.entries()) {
            item.setOrnament(
                backlightKnown && level === state.keyboardBacklightLevel
                    ? PopupMenu.Ornament.DOT
                    : PopupMenu.Ornament.NONE,
            );
        }
        for (const [primary, item] of this._primaryMenuItems.entries()) {
            item.setOrnament(
                primary === state.desiredPrimary
                    ? PopupMenu.Ornament.DOT
                    : PopupMenu.Ornament.NONE,
            );
        }
        for (const [mode, item] of this._tabletModeItems.entries()) {
            item.setOrnament(
                mode === state.tabletMappingMode
                    ? PopupMenu.Ornament.DOT
                    : PopupMenu.Ornament.NONE,
            );
        }

        if (state.micMuteLed === null) {
            this._micItem.setSensitive(false);
        } else {
            this._micItem.setSensitive(true);
            this._suppressMicToggle = true;
            this._micItem.setToggleState(state.micMuteLed);
            this._suppressMicToggle = false;
        }

        if (state.tabletMappingEnabled === null) {
            this._tabletEnableItem.setSensitive(false);
        } else {
            this._tabletEnableItem.setSensitive(true);
            this._suppressTabletToggle = true;
            this._tabletEnableItem.setToggleState(state.tabletMappingEnabled);
            this._suppressTabletToggle = false;
        }

        this._tabletModeMenu.setSensitive(tabletKnown && state.tabletMappingEnabled);
        this._tabletRefreshItem.setSensitive(tabletKnown && state.tabletMappingEnabled);
    }
});

// ── Panel indicator (top-bar icons) ───────────────────────────────────────

const ZenbookKeyboardBatteryIndicator = GObject.registerClass(
class ZenbookKeyboardBatteryIndicator extends SystemIndicator {
    _init(extension) {
        super._init();

        // Icon in the top bar (same row as system battery, clock, etc.)
        this._panelIcon = this._addIndicator();
        this._panelIcon.icon_name = 'input-keyboard-symbolic';
        this._panelIcon.visible = false;

        // Percentage label next to the icon
        this._panelLabel = new St.Label({
            style_class: 'zenbook-kbd-battery-label',
            text: '',
            y_align: Clutter.ActorAlign.CENTER,
        });
        this.add_child(this._panelLabel);
        this._panelLabel.visible = false;

        // Quick Settings card
        this._toggle = new ZenbookKeyboardBatteryToggle(extension);
        this.quickSettingsItems.push(this._toggle);
    }

    /**
     * Refresh all visual elements from daemon state.
     */
    update(state) {
        const hasLink = state.keyboardUsbConnected || state.keyboardPogoDocked || state.bluetoothConnected;
        const show = hasLink || state.keyboardBatteryPresent;
        this._panelIcon.visible = show;
        this._panelLabel.visible = show;

        if (state.keyboardBatteryPresent) {
            this._panelIcon.icon_name = batteryIconName(
                state.keyboardBatteryPercentage,
                state.keyboardBatteryCharging,
                state.keyboardBatteryFull,
            );
            const suffix = state.keyboardBatteryCharging ? '⚡' : '';
            this._panelLabel.text = `${state.keyboardBatteryPercentage}%${suffix}`;
        } else {
            this._panelIcon.icon_name = 'input-keyboard-symbolic';
            this._panelLabel.text = hasLink ? '—' : '';
        }

        this._toggle.update(state);
    }

    destroy() {
        this._toggle.destroy();
        super.destroy();
    }
});

// ── Main Extension class ───────────────────────────────────────────────────

export default class ZenbookDuoExtension extends Extension {
    enable() {
        this.state = null;
        this._indicator = new ZenbookKeyboardBatteryIndicator(this);

        // Insert the indicator into the Quick Settings status area.
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);

        this.state = {
            rootReachable: false,
            nowUsec: Date.now() * 1000,
            keyboardUsbConnected: false,
            keyboardPogoDocked: false,
            bluetoothConnected: false,
            keyboardBatteryPresent: false,
            keyboardBatteryPercentage: 0,
            keyboardBatteryCharging: false,
            keyboardBatteryFull: false,
            desiredPrimary: 'eDP-1',
            desiredSecondaryEnabled: false,
            desiredDisplayAttachment: 'builtin_only',
            desiredDisplayLayout: 'joined',
            displayBrightness: null,
            displayApplyPaused: false,
            tabletMappingEnabled: null,
            tabletMappingMode: null,
            tabletMappingApplyNonce: null,
            sessionRegistered: null,
            sessionOwner: null,
            sessionId: null,
            sessionLastSeenUsec: null,
            micMuteLed: null,
            keyboardBacklightLevel: null,
        };
        this._indicator.update(this.state);

        this._connectDbus();
    }

    disable() {
        this._disconnectDbus();

        if (this._indicator) {
            this._indicator.quickSettingsItems.forEach(i => i.destroy());
            this._indicator.destroy();
            this._indicator = null;
        }
    }

    // ── D-Bus ────────────────────────────────────────────────────────────

    _connectDbus() {
        const iface = Gio.DBusNodeInfo.new_for_xml(DBUS_XML)
            .interfaces[0]; // parsed array; first entry is our interface

        // Build a proxy using the minimal XML introspection above.
        this._proxy = new Gio.DBusProxy({
            g_connection:      Gio.DBus.system,
            g_name:            DBUS_SERVICE,
            g_object_path:     DBUS_PATH,
            g_interface_name:  DBUS_INTERFACE,
            g_interface_info:  iface,
            g_flags:           Gio.DBusProxyFlags.NONE,
        });

        this._proxy.init_async(
            GLib.PRIORITY_DEFAULT,
            null,
            (proxy, result) => {
                try {
                    proxy.init_finish(result);
                } catch (e) {
                    console.error(`[zenbook-duo] D-Bus init failed: ${e.message}`);
                    this.state = {
                        ...this.state,
                        rootReachable: false,
                        nowUsec: Date.now() * 1000,
                    };
                    this._indicator?.update(this.state);
                    return;
                }

                // Initial read
                this._refresh();

                // Watch for property changes pushed by the daemon.
                this._propChangeId = this._proxy.connect(
                    'g-properties-changed',
                    () => this._refresh(),
                );
                this._nameOwnerId = this._proxy.connect(
                    'notify::g-name-owner',
                    () => this._refresh(),
                );
            },
        );
    }

    _disconnectDbus() {
        if (this._proxy) {
            if (this._propChangeId) {
                this._proxy.disconnect(this._propChangeId);
                this._propChangeId = null;
            }
            if (this._nameOwnerId) {
                this._proxy.disconnect(this._nameOwnerId);
                this._nameOwnerId = null;
            }
            this._proxy = null;
        }
    }

    callMethod(methodName, parameters) {
        if (!this._proxy)
            return;

        this._proxy.call(
            methodName,
            parameters,
            Gio.DBusCallFlags.NONE,
            -1,
            null,
            (proxy, result) => {
                try {
                    proxy.call_finish(result);
                    this._refresh();
                } catch (e) {
                    console.error(`[zenbook-duo] D-Bus call ${methodName} failed: ${e.message}`);
                }
            },
        );
    }

    /**
     * Read all relevant properties from the cached proxy values and push
     * them to the indicator.
     */
    _refresh() {
        if (!this._proxy || !this._indicator)
            return;

        const get = (name) => this._proxy.get_cached_property(name);

        const usbVar = get('KeyboardUsbConnected');
        const pogoVar = get('KeyboardPogoDocked');
        const btVar = get('BluetoothConnected');
        const presentVar = get('KeyboardBatteryPresent');
        const pctVar = get('KeyboardBatteryPercentage');
        const chargingVar = get('KeyboardBatteryCharging');
        const fullVar = get('KeyboardBatteryFull');
        const desiredPrimaryVar = get('DesiredPrimary');
        const desiredSecondaryVar = get('DesiredSecondaryEnabled');
        const desiredAttachmentVar = get('DesiredDisplayAttachment');
        const desiredLayoutVar = get('DesiredDisplayLayout');
        const brightnessVar = get('DisplayBrightness');
        const displayPausedVar = get('DisplayApplyPaused');
        const tabletMappingEnabledVar = get('TabletMappingEnabled');
        const tabletMappingModeVar = get('TabletMappingMode');
        const tabletMappingApplyNonceVar = get('TabletMappingApplyNonce');
        const sessionRegisteredVar = get('SessionRegistered');
        const sessionOwnerVar = get('SessionOwner');
        const sessionIdVar = get('SessionId');
        const sessionLastSeenUsecVar = get('SessionLastSeenUsec');
        const micMuteVar = get('MicMuteLed');
        const backlightLevelVar = get('KeyboardBacklightLevel');
        const rootReachable = Boolean(this._proxy.get_name_owner());

        this.state = {
            rootReachable,
            nowUsec: Date.now() * 1000,
            keyboardUsbConnected: variantValue(usbVar, false),
            keyboardPogoDocked: variantValue(pogoVar, false),
            bluetoothConnected: variantValue(btVar, false),
            keyboardBatteryPresent: variantValue(presentVar, false),
            keyboardBatteryPercentage: variantValue(pctVar, 0),
            keyboardBatteryCharging: variantValue(chargingVar, false),
            keyboardBatteryFull: variantValue(fullVar, false),
            desiredPrimary: variantValue(desiredPrimaryVar, 'eDP-1'),
            desiredSecondaryEnabled: variantValue(desiredSecondaryVar, false),
            desiredDisplayAttachment: variantValue(desiredAttachmentVar, 'builtin_only'),
            desiredDisplayLayout: variantValue(desiredLayoutVar, 'joined'),
            displayBrightness: brightnessVar ? brightnessVar.deepUnpack() : null,
            displayApplyPaused: variantValue(displayPausedVar, false),
            tabletMappingEnabled: tabletMappingEnabledVar ? tabletMappingEnabledVar.get_boolean() : null,
            tabletMappingMode: tabletMappingModeVar ? tabletMappingModeVar.deepUnpack() : null,
            tabletMappingApplyNonce: tabletMappingApplyNonceVar
                ? tabletMappingApplyNonceVar.deepUnpack()
                : null,
            sessionRegistered: sessionRegisteredVar ? sessionRegisteredVar.get_boolean() : null,
            sessionOwner: sessionOwnerVar ? sessionOwnerVar.deepUnpack() : null,
            sessionId: numberFromVariantValue(sessionIdVar),
            sessionLastSeenUsec: numberFromVariantValue(sessionLastSeenUsecVar),
            micMuteLed: micMuteVar ? micMuteVar.get_boolean() : null,
            keyboardBacklightLevel: backlightLevelVar ? backlightLevelVar.get_byte() : null,
        };

        this._indicator.update(this.state);
    }
}
