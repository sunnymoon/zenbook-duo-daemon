# Zenbook Duo GNOME Shell Extension

A GNOME Shell extension that surfaces the detachable keyboard battery state
from the `zenbook-duo-daemon` directly in the GNOME UI.

## What it adds

| Location | What you see |
|---|---|
| **Top-bar status area** (right side) | Keyboard icon + battery `%` (+⚡ when charging). Battery severity thresholds are highlighted: `<25%` warning, `<10%` severe, `<5%` critical. |
| **Quick Settings panel** (the dropdown) | A compact "Duo Kbd" tile with short subtitle (`Pogo`, `USB`, `BT`, `Detached`), health row (`Root` / `Session`), keyboard link row, and keyboard battery row with iconography. |
| **Quick Settings actions** | Toggle second display desired state from the tile itself, **Microphone muted** switch (sets default-source mute; the keyboard LED follows the observed backend state), **Keyboard backlight** selector (Off/Low/Medium/High), direct primary panel actions (`eDP-1` / `eDP-2`), and a **Match panels** stylus-mapping switch. Second-display toggle is disabled while pogo-attached. |
| **Daemon/session status** | In the tile menu, shows `Root: up/unreachable` and session link status (`linked`, `quiet`, `not registered`, or `daemon update pending`), plus owner/id and last-seen age when available. |

> The match-panels switch is shown only when the daemon exposes the tablet mapping mode field; otherwise the submenu stays disabled as "daemon pending".

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
`SessionOwner`, `SessionId`, `SessionLastSeenUsec`, `SessionQuiet`,
`MicMuteLed`, `KeyboardBacklightLevel`.

Methods called (from Quick Settings actions): `SetSecondaryDisplayDesired(bool)`,
`SetDesiredPrimary(string)`, `SetTabletMappingMode(string)`, `SetMicMute(bool)`,
`SetKeyboardBacklightLevel(byte)`.

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
