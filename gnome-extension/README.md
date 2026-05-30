# Zenbook Duo GNOME Shell Extension

A GNOME Shell extension that surfaces the detachable keyboard battery state
from the `zenbook-duo-daemon` directly in the GNOME UI.

## What it adds

| Location | What you see |
|---|---|
| **Top-bar status area** (right side) | Keyboard icon + battery status. Shows `%` (+⚡ when charging) when a battery sample exists; otherwise shows a keyboard state marker. |
| **Quick Settings panel** (the dropdown) | A "Zenbook Keyboard" tile showing keyboard state (`Attached (pogo)`, `USB connected`, `Bluetooth mode`, `Detached`) and battery status, plus a nested display-policy submenu. |
| **Quick Settings actions** | Toggle second display desired state from the tile itself, synced **Mic mute LED** switch, **Keyboard backlight** selector (Off/Low/Medium/High), **Primary panel** selector (`eDP-1` / `eDP-2`), and **Stylus / tablet mapping** controls. Second-display toggle is disabled while pogo-attached. |
| **Daemon/session status** | In the tile menu, shows `Root: up/unreachable` and session link status (`linked`, `not registered`, or `daemon update pending`), plus owner/id when available. |

> The tablet mapping controls are shown only when the daemon exposes the tablet D-Bus fields; otherwise the submenu stays disabled as "daemon pending".

## D-Bus interface consumed

| Field | Value |
|---|---|
| Bus | **system** |
| Service | `asus.zenbook.duo` |
| Path | `/asus/zenbook/duo/State` |
| Interface | `asus.zenbook.duo.State` |

Properties read: `KeyboardUsbConnected`, `KeyboardPogoDocked`,
`BluetoothConnected`, `KeyboardBatteryPresent`, `KeyboardBatteryPercentage`,
`KeyboardBatteryCharging`, `KeyboardBatteryFull`, `DesiredPrimary`,
`DesiredSecondaryEnabled`, `DesiredDisplayAttachment`, `DesiredDisplayLayout`,
`DisplayBrightness`, `DisplayApplyPaused`, `TabletMappingEnabled`,
`TabletMappingMode`, `TabletMappingApplyNonce`, `SessionRegistered`,
`SessionOwner`, `SessionId`, `SessionLastSeenUsec`, `MicMuteLed`,
`KeyboardBacklightLevel`.

Methods called (from Quick Settings actions): `SetSecondaryDisplayDesired(bool)`,
`SetDesiredPrimary(string)`, `SetTabletMappingEnabled(bool)`,
`SetTabletMappingMode(string)`, `ToggleTabletMappingMode()`,
`ApplyTabletMapping()`, `SetMicMuteLed(bool)`,
`ToggleMicMuteLed()`, `SetKeyboardBacklightLevel(byte)`,
`ToggleKeyboardBacklight()`.

## Requirements

- GNOME Shell 45 or newer
- `zenbook-duo-daemon` running on the system bus

## Installation

```bash
cd gnome-extension
bash install.sh --enable
# If GNOME does not detect a freshly added UUID immediately, log out/in once.
```

## Debugging

```bash
# Stream GNOME Shell logs while testing
journalctl /usr/bin/gnome-shell -f | grep -i zenbook

# Inspect live D-Bus property values
busctl --system get-property asus.zenbook.duo \
       /asus/zenbook/duo/State \
       asus.zenbook.duo.State \
       KeyboardBatteryPercentage
```

## Extension structure

```
gnome-extension/
├── install.sh
├── README.md
└── zenbook-duo@zenbook-duo-daemon/
    ├── metadata.json    — UUID, name, supported GNOME versions
    ├── extension.js     — Main extension code (ES module, GNOME 45+ API)
    └── stylesheet.css   — Minimal CSS for the panel label
```
