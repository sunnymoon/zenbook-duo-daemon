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
 *   SetMicMute(bool)
 *   SetKeyboardBacklightLevel(y)
 *   SetSecondaryDisplayDesired(bool)
 *   SetDesiredPrimary(string)
 *   SetTabletMappingMode(string)
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
    <method name="ToggleMicMute"/>
    <method name="SetMicMute">
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
    <method name="SetTabletMappingMode">
      <arg name="mode" type="s" direction="in"/>
    </method>
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
    <property name="SessionQuiet" type="b" access="read"/>
  </interface>
</node>`;

const BACKLIGHT_LABELS = ['Off', 'Low', 'Medium', 'High'];

function batterySeverity(present, pct) {
    if (!present)
        return 'unknown';
    if (pct < 5)
        return 'critical';
    if (pct < 10)
        return 'severe';
    if (pct < 25)
        return 'warning';
    return 'normal';
}

function updateSeverityStyle(actor, prefix, severity) {
    const classes = [`${prefix}-warning`, `${prefix}-severe`, `${prefix}-critical`];
    for (const klass of classes)
        actor.remove_style_class_name(klass);
    switch (severity) {
    case 'warning':
        actor.add_style_class_name(`${prefix}-warning`);
        break;
    case 'severe':
        actor.add_style_class_name(`${prefix}-severe`);
        break;
    case 'critical':
        actor.add_style_class_name(`${prefix}-critical`);
        break;
    default:
        break;
    }
}

function updateHealthStyle(actor, status) {
    const classes = ['zenbook-status-on', 'zenbook-status-off', 'zenbook-status-warn'];
    for (const klass of classes)
        actor.remove_style_class_name(klass);
    switch (status) {
    case 'on':
        actor.add_style_class_name('zenbook-status-on');
        break;
    case 'off':
        actor.add_style_class_name('zenbook-status-off');
        break;
    case 'warn':
        actor.add_style_class_name('zenbook-status-warn');
        break;
    default:
        break;
    }
}

function connectionStateShortText(state) {
    if (state.keyboardPogoDocked)
        return 'Pogo';
    if (state.keyboardUsbConnected)
        return 'USB';
    if (state.bluetoothConnected)
        return 'BT';
    return 'Detached';
}

function connectionStateText(state) {
    if (state.keyboardPogoDocked)
        return 'Attached via pogo';
    if (state.keyboardUsbConnected)
        return 'USB connected';
    if (state.bluetoothConnected)
        return 'Bluetooth connected';
    return 'Detached';
}

function formatSessionAge(ageSeconds) {
    if (ageSeconds < 60)
        return `${ageSeconds}s`;

    const minutes = Math.floor(ageSeconds / 60);
    const seconds = ageSeconds % 60;
    return seconds === 0 ? `${minutes}m` : `${minutes}m ${seconds}s`;
}

function variantValue(variant, fallback) {
    return variant ? variant.deepUnpack() : fallback;
}

function numberFromVariantValue(variant) {
    if (!variant)
        return null;
    const value = variant.deepUnpack();
    // Keep bigints as-is to avoid precision loss; return other types as-is
    return value;
}

// ── Quick Settings card ────────────────────────────────────────────────────

const ZenbookKeyboardBatteryToggle = GObject.registerClass(
class ZenbookKeyboardBatteryToggle extends QuickMenuToggle {
    _init(extension) {
        super._init({
            title: 'Duo Kbd',
            iconName: 'input-keyboard-symbolic',
            toggleMode: true,
        });
        this._extension = extension;
        this.menu.setHeader('input-keyboard-symbolic', 'Zenbook Duo Kbd');

        this._healthItem = new PopupMenu.PopupImageMenuItem('Root unavailable', 'dialog-error-symbolic', {
            reactive: false,
            can_focus: false,
        });
        this._linkItem = new PopupMenu.PopupImageMenuItem('Keyboard detached', 'input-keyboard-symbolic', {
            reactive: false,
            can_focus: false,
        });
        this._batteryItem = new PopupMenu.PopupImageMenuItem('Battery unknown', 'input-keyboard-symbolic', {
            reactive: false,
            can_focus: false,
        });
        this._availabilityItem = new PopupMenu.PopupImageMenuItem('Second display available', 'video-display-symbolic', {
            reactive: false,
            can_focus: false,
        });
        this._primaryTopItem = new PopupMenu.PopupMenuItem('Use top panel as primary');
        this._primaryTopItem.connect('activate', () => {
            this._extension.callMethod('SetDesiredPrimary', GLib.Variant.new('(s)', ['eDP-1']));
        });
        this._primaryBottomItem = new PopupMenu.PopupMenuItem('Use bottom panel as primary');
        this._primaryBottomItem.connect('activate', () => {
            this._extension.callMethod('SetDesiredPrimary', GLib.Variant.new('(s)', ['eDP-2']));
        });
        this._tabletMenu = new PopupMenu.PopupSubMenuMenuItem('Stylus / tablet mapping');
        this._suppressMatchPanelsToggle = false;
        this._matchPanelsItem = new PopupMenu.PopupSwitchMenuItem('Match panels', false);
        this._matchPanelsItem.connect('toggled', (_item, state) => {
            if (this._suppressMatchPanelsToggle)
                return;
            this._extension.callMethod(
                'SetTabletMappingMode',
                GLib.Variant.new('(s)', [state ? 'one_to_one' : 'all_to_primary']),
            );
        });
        this._tabletMenu.menu.addMenuItem(this._matchPanelsItem);
        this._suppressMicToggle = false;
        this._micItem = new PopupMenu.PopupSwitchMenuItem('Microphone muted', false);
        this._micItem.connect('toggled', (_item, state) => {
            if (this._suppressMicToggle)
                return;
            this._extension.callMethod(
                'SetMicMute',
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

        this.menu.addMenuItem(this._healthItem);
        this.menu.addMenuItem(this._linkItem);
        this.menu.addMenuItem(this._batteryItem);
        this.menu.addMenuItem(this._availabilityItem);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this.menu.addMenuItem(this._primaryTopItem);
        this.menu.addMenuItem(this._primaryBottomItem);
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
        const connectedShort = connectionStateShortText(state);
        const batteryText = state.keyboardBatteryPresent
            ? `${state.keyboardBatteryPercentage}%${state.keyboardBatteryCharging ? ' ⚡' : ''}`
            : 'Unknown';
        const severity = batterySeverity(state.keyboardBatteryPresent, state.keyboardBatteryPercentage);
        // The daemon provides sessionQuiet to indicate stale sessions; no need to calculate age locally
        const staleSession = state.sessionQuiet === true;
        const tabletKnown = state.tabletMappingMode !== null;

        this.subtitle = `${connectedShort} · ${batteryText}`;
        this._linkItem.label.text = `Keyboard ${connectedText}`;
        this._batteryItem.label.text = `Keyboard battery ${batteryText}`;
        updateSeverityStyle(this._batteryItem.label, 'zenbook-kbd-battery', severity);
        if (!state.rootReachable) {
            this._healthItem.label.text = 'Root off · Session unknown';
            updateHealthStyle(this._healthItem.label, 'off');
        } else if (state.sessionRegistered === null) {
            this._healthItem.label.text = 'Root on · Session pending';
            updateHealthStyle(this._healthItem.label, 'warn');
        } else if (state.sessionRegistered) {
            const ageText = sessionAgeSeconds === null ? '' : ` · ${formatSessionAge(sessionAgeSeconds)} ago`;
            this._healthItem.label.text = staleSession
                ? `Root on · Session quiet${ageText}`
                : `Root on · Session linked${ageText}`;
            updateHealthStyle(this._healthItem.label, staleSession ? 'warn' : 'on');
        } else {
            this._healthItem.label.text = 'Root on · Session off';
            updateHealthStyle(this._healthItem.label, 'off');
        }
        this._tabletMenu.label.text = tabletKnown ? 'Stylus / tablet mapping' : 'Stylus / tablet mapping (pending)';
        this._tabletMenu.setSensitive(tabletKnown);
        this._primaryTopItem.setOrnament(
            state.desiredPrimary === 'eDP-1' ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NONE,
        );
        this._primaryBottomItem.setOrnament(
            state.desiredPrimary === 'eDP-2' ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NONE,
        );

        this.iconName = 'input-keyboard-symbolic';

        this.checked = state.desiredSecondaryEnabled;
        if (state.keyboardPogoDocked) {
            this.sensitive = false;
            this._availabilityItem.label.text = 'Second display unavailable while attached';
        } else {
            this.sensitive = true;
            this._availabilityItem.label.text = 'Second display available';
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
        if (state.micMuteLed === null) {
            this._micItem.setSensitive(false);
        } else {
            this._micItem.setSensitive(true);
            this._suppressMicToggle = true;
            this._micItem.setToggleState(state.micMuteLed);
            this._suppressMicToggle = false;
        }

        this._matchPanelsItem.setSensitive(tabletKnown);
        if (tabletKnown) {
            this._suppressMatchPanelsToggle = true;
            this._matchPanelsItem.setToggleState(state.tabletMappingMode === 'one_to_one');
            this._suppressMatchPanelsToggle = false;
        }
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
        const severity = batterySeverity(state.keyboardBatteryPresent, state.keyboardBatteryPercentage);
        this._panelIcon.visible = show;
        this._panelLabel.visible = show;

        if (state.keyboardBatteryPresent) {
            this._panelIcon.icon_name = 'input-keyboard-symbolic';
            const suffix = state.keyboardBatteryCharging ? '⚡' : '';
            this._panelLabel.text = `${state.keyboardBatteryPercentage}%${suffix}`;
        } else {
            this._panelIcon.icon_name = 'input-keyboard-symbolic';
            this._panelLabel.text = hasLink ? '—' : '';
        }
        updateSeverityStyle(this._panelLabel, 'zenbook-kbd-battery', severity);
        updateSeverityStyle(this._panelIcon, 'zenbook-kbd-battery', severity);

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
            sessionQuiet: null,
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

        if (this._healthTimerId) {
            GLib.Source.remove(this._healthTimerId);
            this._healthTimerId = 0;
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

                if (!this._healthTimerId) {
                    this._healthTimerId = GLib.timeout_add_seconds(
                        GLib.PRIORITY_DEFAULT,
                        10,
                        () => {
                            this._refresh();
                            return GLib.SOURCE_CONTINUE;
                        },
                    );
                }

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

        if (this._healthTimerId) {
            GLib.Source.remove(this._healthTimerId);
            this._healthTimerId = 0;
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
        // Guard against calls during or after disable()
        if (!this._indicator || !this._proxy)
            return;
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
        const sessionQuietVar = get('SessionQuiet');
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
            sessionQuiet: sessionQuietVar ? sessionQuietVar.get_boolean() : null,
            micMuteLed: micMuteVar ? micMuteVar.get_boolean() : null,
            keyboardBacklightLevel: backlightLevelVar ? backlightLevelVar.get_byte() : null,
        };

        this._indicator.update(this.state);
    }
}
