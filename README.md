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
  - handles Bluetooth keyboard vendor GATT control (see **ASUS Bluetooth keyboard: why multiple `/dev/input/event*` readers** below)
  - exposes authoritative state on the system D-Bus (including operator commands formerly sent via a FIFO)
  - subscribes to **logind** `PrepareForSleep` for suspend/resume (no separate sleep-hook units)
  - manages keyboard backlight, mic-mute LED, and secondary-display sysfs fallback
  - rate-limits display configuration applies from the session (blast-radius); if applies are paused after a burst, run `zenbook-duo-daemon resume-display-applies` from your **GNOME graphical session** (or `sudo zenbook-duo-daemon resume-display-applies` as root); both are authorized via **Polkit** when policy is installed
- **Session daemon** (`zenbook-duo-session.service`)
  - runs inside the **GNOME** graphical user session (systemd user unit; `ExecCondition` requires `XDG_CURRENT_DESKTOP` to contain `GNOME`)
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

## ASUS Bluetooth keyboard: why multiple `/dev/input/event*` readers (not a code smell)

On the Zenbook Duo, **one** physical Bluetooth keyboard still shows up as **several** `/dev/input/eventN` devices in `/dev/input/`. That is normal for this stack: the kernel (and ASUS firmware) split responsibilities across nodes.

**What each kind of node does**

- **Ordinary keys** (letters, numbers, modifiers for normal typing) are delivered as usual **key** events on the node the kernel chose for the “main” keyboard stream.
- **ASUS vendor hotkeys** (Fn+F4 brightness cycle, Fn+F5/F6 display brightness, Fn+F8 swap primaries, Fn+F9 mic mute, Fn+F11 emoji, Fn+F12 MyASUS, the key right of F12 for the bottom panel, etc.) are **not** always injected as normal `EV_KEY` scancodes on that same node. They are exposed to userspace as **`EV_ABS` / `ABS_MISC`** with vendor-specific integer values (mirroring the USB HID vendor path; see `keyboard_usb` and `parse_keyboard_event` in `src/keyboard_bt.rs`).

**Why the daemon opens more than one `event*` for the same MAC**

The kernel may route those **`ABS_MISC`** events through **one** of several sibling `event*` nodes—or **both** in quick succession—depending on connect order, BlueZ churn, and internal routing. If we only listened to **one** node, field experience showed **intermittent “dead Fn row”** behaviour: the hotkey simply never reached the daemon because it arrived on the **other** sibling.

So `zenbook-duo-daemon` deliberately:

1. **Filters** to the ASUS Duo Bluetooth device name and requires **`ABS_MISC`** on the node (other nodes are ignored).
2. **Groups** all qualifying paths by **`uniq`** (normalized MAC key) in `BtVendorHotkeyReaders`.
3. Runs **GATT / vendor HID restore once per Bluetooth connect session** for that MAC (`hid_restore_done`), not once per evdev path.
4. **Suppresses duplicate actions** when both siblings emit the **same** non-zero `ABS_MISC` value within a short window (`suppress_twin_abs_misc_pulse`).
5. **Removes** each path from the map on **`ENODEV`** disconnect so the state machine matches reality.

**Why this is not a “duplicate reader bug”**

We are not reading the **same** key stream twice out of carelessness: we are reading **different evdev fds** that the kernel uses as **parallel delivery channels** for the **same physical keyboard**, and we coordinate them. Collapsing to a single reader without new kernel/firmware guarantees would **risk regressions** (vendor keys missing after reconnect).

Implementation: `src/keyboard_bt.rs` (`start_bt_keyboard_monitor_task`, `try_start_bt_keyboard_task`, `start_bt_keyboard_task`).

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
curl -fsSL https://raw.githubusercontent.com/sunnymoon/zenbook-duo-daemon/refs/heads/master/install | sudo bash -s install
```

### Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/sunnymoon/zenbook-duo-daemon/refs/heads/master/install | sudo bash -s uninstall
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
- If the root daemon has **paused** further display applies after too many attempts in a short window: run `zenbook-duo-daemon resume-display-applies` from the **active GNOME session** (or `sudo …` as root), then investigate the burst in journals before it happens again.

## What the install script does

The install script currently:

1. installs the daemon binary into `/opt/zenbook-duo-daemon` and updates **`/usr/local/bin/zenbook-duo-daemon`** as a **symlink** to that file (so a normal `PATH` matches the binary the root unit runs); on **uninstall**, removes that symlink only if it still resolves to the removed `/opt/...` binary
2. installs the root and user session systemd units
3. installs the D-Bus policy file into `/etc/dbus-1/system.d/zenbook-duo-daemon-dbus.conf`
4. installs the Polkit actions file into `/usr/share/polkit-1/actions/org.zenbook.duo.policy` (`install` downloads it from `GITHUB_RAW_BASE` in `install`, i.e. this repo’s default `master` raw tree; `local-install` copies `org.zenbook.duo.policy` from the directory that contains `install`)
5. reloads **polkit** when possible so new actions register immediately
6. removes any **legacy** control FIFO (`/tmp/zenbook-duo-daemon.pipe`) and pre/post-sleep units from older installs
7. on **uninstall**, removes the legacy **`zenbook-duo`** system group if it still exists (older installs only)
8. reloads D-Bus if possible
9. merges the Zenbook libinput quirks into `/etc/libinput/local-overrides.quirks`
10. runs config migration if needed
11. enables the root service and **globally** enables the user session service (`systemctl --global enable zenbook-duo-session`) so every user gets the unit; it only **starts** under GNOME because of `ExecCondition` in the unit
12. restarts the session service for active logged-in users when possible

### D-Bus and Polkit

**D-Bus** (`/etc/dbus-1/system.d/zenbook-duo-daemon-dbus.conf`) allows normal clients to connect to `asus.zenbook.duo` on the system bus. **Authorization is enforced in the daemon** with **Polkit** (`org.freedesktop.PolicyKit1.Authority.CheckAuthorization`) using a **unix-process** subject (caller PID + start time from `/proc`).

Installed actions (see `org.zenbook.duo.policy` in this repository):

| Action id | Used for |
|-----------|-----------|
| `org.zenbook.duo.register-session` | Session daemon registering with the root service |
| `org.zenbook.duo.operator` | Hardware operator D-Bus methods and the `control` CLI |
| `org.zenbook.duo.resume-display-applies` | Clearing the display-apply safety guard |

Defaults for all three: **`allow_any` no**, **`allow_inactive` no**, **`allow_active` yes** — i.e. **only processes in an active local session** (and **root**, via the distribution’s default Polkit rules) are implicitly allowed without a password prompt.

You need **polkit** installed and running. If `CheckAuthorization` fails at runtime, verify the policy file is present and reload polkit (`systemctl reload polkit` or reboot).

Site-specific tightening (e.g. restrict to a group, require `auth_self`, or per-seat rules) can be done with files under `/etc/polkit-1/rules.d/` without changing the daemon binary.

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

The **root** daemon must be running. Commands call **D-Bus** on `asus.zenbook.duo`. The calling process must pass **Polkit** for `org.zenbook.duo.operator` (typically: **active local graphical session** or **root**). See `zenbook-duo-daemon --help` and `control --help`.

Examples:

```bash
zenbook-duo-daemon control mic-mute-led-toggle
zenbook-duo-daemon control mic-mute-led true
zenbook-duo-daemon control keyboard-backlight-toggle
zenbook-duo-daemon control keyboard-backlight-set medium
zenbook-duo-daemon control secondary-display-toggle
zenbook-duo-daemon control secondary-display false
zenbook-duo-daemon resume-display-applies
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

After install, log in to a **GNOME** session on the machine: `zenbook-duo-session` is enabled globally and starts when `XDG_CURRENT_DESKTOP` contains `GNOME`. The session registers with the root daemon; **Polkit** grants `org.zenbook.duo.register-session` for active local sessions. **No `zenbook-duo` Unix group** is required anymore.

**Migrating from older installs** that used the `zenbook-duo` group: `sudo ./install uninstall` attempts `groupdel zenbook-duo`. If that fails because users are still members, remove them explicitly, for example `sudo gpasswd -d yourlogin zenbook-duo`, then remove the empty group if needed.

If you maintain a **different GitHub fork**, edit `GITHUB_REPO` and `GITHUB_RAW_BASE` at the top of `install` so `install` (curl) and the Polkit policy download match your repository.
