# ASUS Zenbook Duo Daemon

Daemon and session companion for ASUS Zenbook Duo laptops on Linux.

It handles the detachable keyboard in both USB-attached and Bluetooth-detached modes, manages the secondary display policy, and integrates with GNOME for display/orientation behavior.

AI Generated Wiki: [![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/PegasisForever/zenbook-duo-daemon)

## Supported hardware

- ✅ Zenbook Duo 2025 (`UX8406CA`)
- ✅ Zenbook Duo 2024 (`UX8406MA`)

## Tested distributions

- ✅ Fedora 42
- ✅ Ubuntu 25.10
- ⚠️ NixOS: see this [fork](https://github.com/0Tick/zenbook-duo-daemon/tree/copilot/convert-to-nix-flake)
- ⚠️ Other distributions may work but are not regularly tested

## Current architecture

The project currently installs multiple components:

- **Root daemon** (`zenbook-duo-daemon.service`)
  - handles USB keyboard access
  - handles Bluetooth keyboard vendor GATT control
  - exposes authoritative state on the system D-Bus (including operator commands formerly sent via a FIFO)
  - subscribes to **logind** `PrepareForSleep` for suspend/resume (no separate sleep-hook units)
  - manages keyboard backlight, mic-mute LED, and secondary-display sysfs fallback
  - rate-limits display configuration applies from the session (blast-radius); if applies are paused after a burst, run `sudo zenbook-duo-daemon resume-display-applies`
- **Session daemon** (`zenbook-duo-session.service`)
  - runs inside the graphical user session
  - applies display/orientation changes through GNOME/Mutter
  - monitors GNOME ambient light setting
  - acknowledges root requests over D-Bus

## What currently works

- ✅ USB-attached keyboard handling
- ✅ Bluetooth-detached keyboard handling
- ✅ Fn-lock state restore in USB and Bluetooth mode
- ✅ Keyboard backlight control in USB and Bluetooth mode
- ✅ Microphone-mute LED control in USB and Bluetooth mode
- ✅ Secondary display policy based on keyboard attachment
- ✅ Secondary display brightness mirroring from the primary display
- ✅ Display orientation integration through the session daemon
- ✅ Native libinput disable-while-typing (DWT) in **USB mode**
- ✅ Native libinput disable-while-typing (DWT) in **Bluetooth mode**
- ✅ Configurable remapping of the proprietary special keys exposed by the Zenbook keyboard
- ✅ Persistence of keyboard/display/ambient-related daemon state
- ✅ Display mode preservation across Super+P / GNOME Control Center changes (mirror ↔ joined)
- ✅ Keyboard battery warnings (tiered notifications at 20% / 10% / 5% with themed icons)

## Current known limitations

- ⚠️ The project currently targets **GNOME** as the supported desktop environment for session-side display behavior
- ⚠️ Display recovery/startup edge cases are improved but still an active area of refinement
- ⚠️ **External monitors** are out of scope: layout and verification logic focus on **eDP-1** and **eDP-2**. Adding HDMI/DP/etc. may result in those outputs being omitted from an `ApplyMonitorsConfig` rebuild or in verification failing until the external is disconnected.
- ⚠️ The daemon only remaps the **Zenbook-specific special keys** that require vendor handling; standard keys that already arrive as normal evdev keys are not remapped by the daemon

## Dual display / Mutter apply ordering (important for Duo internals)

The Zenbook Duo’s two built-in panels are both **eDP** connectors (`eDP-1`, `eDP-2`). GNOME Shell drives them through Mutter’s **`org.gnome.Mutter.DisplayConfig`** API (`apply_monitors_config`), the same path tools like **`gdctl`** use.

### 1. Logical monitor order (“primary last”)

Experiments on real hardware showed that the **order of logical monitors in the apply payload matters**: the monitor that should end up **primary** should be the **last** entry in the list passed to `apply_monitors_config`. The daemon’s `build_duo_lms` in `src/session/display.rs` implements this by emitting **non-primary first, primary second** (including a swap when the effective primary is `eDP-1`, because the geometric construction naturally listed `eDP-1` first for the stacked layout).

Putting **primary first** can leave Mutter’s **logical monitor graph** inconsistent with what was requested: `gdctl` / D-Bus may show **duplicate connector names** on two logical rows or **two `primary: true` flags** even though the written config looked correct.

### 2. Two-phase apply when going from **eDP-1 only** to **eDP-2 primary** (dual)

A **single** atomic apply that both **enables the second panel** and **moves the primary to eDP-2** (from a state where only `eDP-1` had a logical monitor) was repeatedly broken on test hardware, while the **same final layout** worked if done in **two steps**:

1. **Phase 1:** dual layout with **`eDP-1` still primary** and **`eDP-2` enabled** (non-primary).
2. **Pause:** re-read Mutter state (new configuration **serial**), wait **~300 ms** (`EDPTWO_PRIMARY_PHASE1_STABILITY_MS` in `display.rs`) so KMS can settle after the first atomic commit.
3. **Phase 2:** dual layout with **`eDP-2` primary** (again: non-primary logical first, primary last).

The session daemon implements this in `apply_desired_display_state` when `requires_phase1_edp2_primary_from_edp1_solo` is true (see `src/session/display.rs`). Each phase uses a separate **root display-apply guard** attempt (`register_display_apply_attempt`), matching the cost of two compositor applies.

### 3. Why not rely on one apply? (journal / plausibility)

On Fedora 44 + **mutter 50.x**, logs on affected machines have shown:

- `drmModeAtomicCommit: Invalid argument`, **page flip** failures, and **KMS update** errors from **gnome-shell** during aggressive output reconfiguration.
- Prior **gnome-shell** crashes in **`meta_monitor_mode_foreach_crtc()`** (Mutter monitor / CRTC path).

Those failures correlate with **too much changing in one atomic modeset**, which matches the bad “one swipe” `gdctl` behavior. The **two-phase** path is a **workaround** for compositor + kernel fragility; keep it unless upstream fixes and hardware re-testing prove a single apply is safe.

**Do not remove or merge the two-phase path** without re-running manual **`gdctl`** experiments and checking `journalctl --user` during the transition.

## Keyboard function support

| Keyboard Function               | Wired Mode | Bluetooth Mode | Default Mapping              | Remappable via config |
| ------------------------------- | ---------- | -------------- | ---------------------------- | --------------------- |
| Mute Key                        | ✅         | ✅             | Native standard key          | ❌                    |
| Volume Down Key                 | ✅         | ✅             | Native standard key          | ❌                    |
| Volume Up Key                   | ✅         | ✅             | Native standard key          | ❌                    |
| Keyboard Backlight Key          | ✅         | ✅             | Toggle keyboard backlight    | ✅                    |
| Keyboard Backlight Control      | ✅         | ✅             | Device state restore/control | ✅                    |
| Brightness Down Key             | ✅         | ✅             | `KEY_BRIGHTNESSDOWN`         | ✅                    |
| Brightness Up Key               | ✅         | ✅             | `KEY_BRIGHTNESSUP`           | ✅                    |
| Swap Up/Down Display Key        | ✅         | ✅             | Swap primary display         | ✅                    |
| Microphone Mute Key             | ✅         | ✅             | `KEY_MICMUTE`                | ✅                    |
| Microphone Mute LED Control     | ✅         | ✅             | Device state restore/control | ✅                    |
| Emoji Picker Key                | ✅         | ✅             | `KEY_LEFTCTRL + KEY_DOT`     | ✅                    |
| MyASUS Key                      | ✅         | ✅             | No-op by default             | ✅                    |
| Toggle Secondary Display Key    | ✅         | ✅             | Toggle secondary display     | ✅                    |
| Fn + Function Keys              | ✅         | ✅             | Controlled by fn-lock state  | ❌                    |

## libinput quirks

Native DWT now depends on a small set of Zenbook-specific libinput quirks:

- USB keyboard marked as `AttrKeyboardIntegration=internal`
- Bluetooth keyboard marked as `AttrKeyboardIntegration=internal`
- Bluetooth touchpad marked as `AttrTPKComboLayout=below`

These quirks live in the repository as:

- `local-overrides.quirks`

The install script **merges only the missing Zenbook sections** into:

- `/etc/libinput/local-overrides.quirks`

It does **not** replace unrelated user quirks already present in that file.

## Installation

### Install from a local checkout

```bash
cargo build --release
sudo ./install local-install target/release/zenbook-duo-daemon
```

### Install from the latest release

```bash
curl -fsSL https://raw.githubusercontent.com/PegasisForever/zenbook-duo-daemon/refs/heads/master/install | sudo bash -s install
```

### Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/PegasisForever/zenbook-duo-daemon/refs/heads/master/install | sudo bash -s uninstall
```

### Useful status commands

```bash
systemctl status zenbook-duo-daemon.service
systemctl --user status zenbook-duo-session.service
```

### Debugging display applies

- Compare Mutter’s view with what you expect: `gdctl show` (same API family the session daemon uses).
- Session logs: `journalctl --user -u zenbook-duo-session.service --since "15 min ago"`.
- Pre-apply “current vs desired” layout diffs are logged at **debug**; set `RUST_LOG=debug` on `zenbook-duo-session.service` (e.g. `systemctl --user edit zenbook-duo-session.service`) when chasing ordering or verification issues.
- If the root daemon has **paused** further display applies after too many attempts in a short window: `sudo zenbook-duo-daemon resume-display-applies`, then investigate the burst in journals before it happens again.

## What the install script does

The install script currently:

1. installs the daemon binary into `/opt/zenbook-duo-daemon`
2. installs the root and user session systemd units
3. installs the D-Bus policy file into `/etc/dbus-1/system.d/zenbook-duo-daemon-dbus.conf`
4. ensures the **`zenbook-duo`** system group exists and adds active `loginctl` users plus the user who ran **`sudo ./install`** (`SUDO_USER`) when present
5. removes any **legacy** control FIFO (`/tmp/zenbook-duo-daemon.pipe`) and pre/post-sleep units from older installs
6. reloads D-Bus if possible
7. merges the Zenbook libinput quirks into `/etc/libinput/local-overrides.quirks`
8. runs config migration if needed
9. enables the root service and the user session service
10. restarts the session service for active logged-in users when possible

### D-Bus policy note

The D-Bus policy is installed as its **own file**:

- `/etc/dbus-1/system.d/zenbook-duo-daemon-dbus.conf`

It is **copied into place**, not XML-merged into another file. That is the correct model for D-Bus policy deployment here.

Only **`root`** (the daemon) and members of the **`zenbook-duo`** group may send to or receive from this service on the system bus (`install` creates the group, adds active `loginctl` users, and adds **`SUDO_USER`** when you run `sudo ./install`, but **a logout/login is required** so the session picks up the new group). Unprivileged callers without that membership will get access errors instead of being able to replace the registered session connection.

`register_session` additionally rejects callers whose D-Bus credentials report a **UID below 1000** (system accounts). Operator-style methods reject UIDs **1–999** (non-root system accounts). `resume_display_applies` accepts **root UID only** (intended for `sudo zenbook-duo-daemon resume-display-applies`).

Tighter models (Polkit rules per method, seat/session matching for registration, or splitting state by user) are possible follow-ups if multi-user shared machines need stronger isolation than “members of `zenbook-duo` trust each other on this host.”

## Configuration

The config file lives at:

- `/etc/zenbook-duo-daemon/config.toml`

You can configure:

- fn-lock default
- idle timeout
- special-key remappings
- keyboard USB VID:PID if needed
- backlight and secondary-display sysfs paths

## Operator CLI (D-Bus)

The **root** daemon must be running. Commands call **D-Bus** on `asus.zenbook.duo`; you must be in **`zenbook-duo`** or **root** (same as the bus policy). See `zenbook-duo-daemon --help` and `control --help`.

Examples:

```bash
zenbook-duo-daemon control mic-mute-led-toggle
zenbook-duo-daemon control mic-mute-led true
zenbook-duo-daemon control keyboard-backlight-toggle
zenbook-duo-daemon control keyboard-backlight-set medium
zenbook-duo-daemon control secondary-display-toggle
zenbook-duo-daemon control secondary-display false
sudo zenbook-duo-daemon resume-display-applies
```

## Suspend / resume

Suspend and resume are handled inside the root daemon via **logind**’s `PrepareForSleep` signal on the system bus (no `echo … >` FIFO and no `pre-sleep` / `post-sleep` systemd services). After resume the daemon refreshes idle/LED state and rescans USB keyboard attachment, matching the old post-sleep hook behavior.

## Development

### Prerequisites

```bash
sudo apt install build-essential libevdev-dev libdbus-1-dev pkg-config autoconf
```

### Build

```bash
cargo build
```

### Local install for testing

```bash
cargo build
sudo ./install local-install target/debug/zenbook-duo-daemon
```

After install, ensure your user is in the **`zenbook-duo`** group (the script adds active loginctl users) and **log out and back in** so the session daemon can access the system D‑Bus service. See **D-Bus policy note** above.
