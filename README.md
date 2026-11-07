# DiShus — Distans Show/Use

Use your Android phone as the screen, keyboard and mouse of your Linux PC, from anywhere,
peer-to-peer. No accounts, no cloud service of ours, no port forwarding.

- **PC side:** `dishus-daemon` (a small Rust systemd *user* service) plus the `dishus` CLI.
- **Phone side:** a Kotlin/Compose app. The top 60 % shows the PC screen and works as a touchpad.
  The bottom 40 % is a full PC keyboard.
- **Network:** [iroh](https://iroh.computer) QUIC. Both ends are identified by public keys. The PC
  opens no listening TCP port. Connections are hole-punched directly when possible, and otherwise
  go through a relay (by default iroh's public relays, configurable). A relay only carries
  end-to-end-encrypted QUIC.
- **Licence:** MIT OR Apache-2.0.

> **Status: alpha.** The PC side runs and was tested end-to-end on Hyprland + NVIDIA with a
> desktop test client. The Android app is written but **has not been compiled yet**.
> See [Status](#status--whats-verified) before relying on it.

## How it works

```
 phone (Compose UI)                                   PC (systemd --user)
 ┌──────────────────┐   iroh QUIC, ALPN dishus/session/1   ┌──────────────────────────────┐
 │ dishus-core (Rust│◄────────────────────────────────────►│ dishus-daemon                │
 │  via UniFFI)     │  bi-stream: control + input (reliable)│  allowlist gate (pre-parse)  │
 │ MediaCodec H.264 │  uni-stream per video frame (drop-old)│  uinput kbd + pointer        │
 └──────────────────┘                                       │  ├ capture worker (per session)
                                                            │  │  portal/PipeWire → GStreamer
                                                            │  │  NVENC → VA → x264 → openh264
                                                            └──┴───────────────────────────┘
                                                               ▲ unix socket (0600, uid-checked)
                                                               dishus CLI
```

- **Video.** Each encoded frame travels on its own QUIC uni-stream. If more than 4 frames are
  unacknowledged, or a frame misses its 1 s deadline, the PC drops or resets frames and forces a
  keyframe. The phone never decodes a corrupted half-frame. It also asks for a keyframe on any
  sequence gap.
- **Adaptive bitrate and frame rate.** The rate starts at 30 fps. It steps up to 60 fps once the
  link has stayed clean, backs off by 30 % on congestion, and stays inside the bitrate range set by
  the quality preset.
- **Capture and encode** run in a separate worker process that exists only during a session. On
  NVIDIA, NVENC costs ~300 MB of driver mappings, and that memory is gone as soon as you disconnect.
- **Input** goes through `/dev/uinput` as evdev *physical* keycodes. The PC's keyboard layout
  applies, and the on-screen keyboard is labelled US-QWERTY. Every held key and button is released
  on disconnect, on a 5 s silence, on revoke, and on kill. Power, sleep and wake keys are refused.
- **Pointer** works like a laptop touchpad (relative):
  - one finger moves, tap = left click
  - long-press or two-finger tap = right click
  - double-tap-and-hold = drag
  - two fingers = scroll
  - pinch = zoom the phone view

Design decisions and their reasons are in [docs/PLAN.md](docs/PLAN.md). The threat model is in
[docs/SECURITY.md](docs/SECURITY.md).

## Install (PC)

Arch: `packaging/aur/PKGBUILD` (`dishus-git`). Its `url`/`source` point at the upstream repo
(`github.com/dishus/dishus`, a placeholder until the project is published). To build from a local
checkout instead:

```sh
mkdir -p /tmp/dishus-pkg && cp packaging/aur/PKGBUILD /tmp/dishus-pkg/ && cd /tmp/dishus-pkg
sed -i "s|^source=.*|source=(\"dishus::git+file://$OLDPWD\")|" PKGBUILD
makepkg -si
```

Or build by hand (Rust ≥ 1.89, GStreamer 1.24+ dev files):

```sh
cargo build --release -p dishus-daemon -p dishus-cli
install -Dm755 target/release/dishus-daemon ~/.local/bin/dishus-daemon
install -Dm755 target/release/dishus        ~/.local/bin/dishus
install -Dm644 packaging/systemd/dishus.service ~/.config/systemd/user/dishus.service
# edit ExecStart= to ~/.local/bin/dishus-daemon if you installed there
```

Runtime packages (Arch names):
- **Required:**
  - `gstreamer`, `gst-plugins-base-libs`
  - `gst-plugins-bad-libs`: NVENC (`nvcodec`) and `h264parse`
  - `gst-plugin-pipewire`, `pipewire`, `xdg-desktop-portal`
  - the portal backend for your desktop (`xdg-desktop-portal-hyprland`, `-gnome`, `-kde`, …)
- **Optional:**
  - `gst-plugin-va`: VA-API on Intel/AMD
  - `gst-plugins-ugly`: x264 software encoder
  - `gst-plugins-bad`: openh264
  - `gst-plugins-good`: X11 capture

### 1. Let the daemon create input devices

The daemon needs write access to `/dev/uinput`. Check first:

```sh
getfacl /dev/uinput     # your user, or a group you are in, needs rw
```

If you lack access, install the shipped rule. It grants access to whoever is logged in at the
local seat. It is not installed automatically, because on a multi-user machine uinput access means
being able to type into anyone's session.

```sh
sudo install -Dm644 /usr/share/dishus/70-dishus-uinput.rules /etc/udev/rules.d/
echo uinput | sudo tee /etc/modules-load.d/uinput.conf
sudo udevadm control --reload && sudo modprobe uinput && sudo udevadm trigger
```

Without uinput the daemon still runs, as view-only. `dishus status` shows `input: none`.

### 2. Start the daemon

```sh
systemctl --user enable --now dishus.service
dishus status
```

The unit is not tied to `graphical-session.target`, because plain Hyprland, without uwsm, never
reaches it. For screen capture, the portal must see your compositor's environment. Most setups do
that already (Hyprland, for example, with
`exec-once = dbus-update-activation-environment --systemd --all`). The X11 fallback also needs
`DISPLAY` in the user manager: `systemctl --user import-environment DISPLAY`.

### 3. Pair the phone (at the PC)

```sh
dishus pair
```

1. **Screen-capture permission.** `dishus pair` first opens your desktop's screen-share dialog.
   Pick the screen(s), and on Hyprland tick **"Allow a restore token"**. The permission is stored
   as a restore token, so later remote sessions don't hit a dialog nobody is there to click. You
   can re-run this alone with `dishus setup-capture`.
   - xdph users can make the restore-token box default to on with
     `screencopy { allow_token_by_default = true }` in `~/.config/hypr/xdph.conf`.
2. **QR code.** It then shows a QR code and a `dishus://pair?…` link. The link is valid for 60 s
   and for one use. Scan it in the app (**Scan QR**), or paste the link.
3. **Confirmation.** The CLI prints the new device when pairing succeeds.

### Remote use after a reboot: auto-login is required

`dishus-daemon` is a *user* service. It runs only while you have a logged-in session, and screen
capture needs a running compositor. For "PC rebooted while I'm away" to work, configure **automatic
login into your graphical session**. Pair it with a lock screen (hyprlock etc.) if the PC is
physically accessible: you will see the lock screen remotely and can type your password from the
phone keyboard.

Configure auto-login in your display manager, for example SDDM `[Autologin]` or greetd
`initial_session`. DiShus does not change it for you.

## CLI

```
dishus status                  daemon state, PC id, relays, capture/input backends, sessions
dishus pair [--skip-capture-setup]
dishus list                    paired phones, expiry, last seen
dishus renew <name|id> [--days N]   extend (1–60 days, default 30)
dishus revoke <name|id>        remove a phone and cut its session immediately
dishus kill                    PANIC: cut every session and refuse everything until…
dishus unlock                  …you leave lockdown (lockdown survives restarts)
dishus setup-capture           re-grant screen-capture permission
```

`list` and `revoke` also work when the daemon is stopped; they edit the device file directly.
`kill` falls back to `systemctl --user stop dishus.service` if the daemon doesn't answer. The
daemon sends a desktop notification for pairings, connects, disconnects, revokes and kill.

## Configuration

`~/.config/dishus/config.toml` (created with defaults; unknown keys are rejected):

```toml
pc_name = "my-desktop"        # shown on the phone (default: hostname)
relays = []                   # [] = iroh's public relays; or ["https://relay.example.org/"]
disable_relays = false        # true = direct connections only (LAN / reachable hosts)
expiry_days = 30              # pairing lifetime, 1..=60
pairing_ttl_secs = 60         # pairing link lifetime, 10..=60
udp_port = 0                  # 0 = random; set to pin a port for a firewall/port-forward rule

[video]
encoder = "auto"              # auto | nvenc | vaapi | x264 | openh264
max_bitrate_kbps = 20000      # ceiling for the adaptive controller
show_cursor = true
```

Restart the daemon after editing (`systemctl --user restart dishus`). The phone has its own relay
list under **Settings**. To use a self-hosted relay
([iroh-relay](https://github.com/n0-computer/iroh)), set it on both sides.

Files the daemon keeps (all `0600` in `0700` directories; group- or world-readable files are
refused):

| Path | Content |
|---|---|
| `~/.config/dishus/secret.key` | PC identity (Ed25519) |
| `~/.config/dishus/devices.toml` | allowlist: phone public keys, names, expiry |
| `~/.local/state/dishus/` | portal restore token, lockdown flag |
| `$XDG_RUNTIME_DIR/dishus/control.sock` | CLI socket |

Logging: `DISHUS_LOG=debug` (a `tracing` filter). Secrets and tokens are never logged.

## Phone app

- **First start.** Choose an app password. It derives (Argon2id) the key that encrypts the phone's
  identity and its list of paired PCs. The encrypted blob is then wrapped again by an Android
  Keystore key (StrongBox when available). Fingerprint or face unlock is optional. There is no
  recovery: if you forget the password, wipe the app data and pair again.
- **Pairing.** Scan the QR from `dishus pair`.
- **Connecting.** Tap a PC. With one paired PC, or "connect to last PC" on, it connects
  immediately after unlock.
- **Screen.** Gear icon opens quick settings:
  - frame rate (Adaptive / 30 / 60), quality, resolution cap, monitor
  - renew the pairing for 30 or 60 days
  - connection path (direct or relay) and app-level round-trip time
  - relay and app settings, disconnect
- **Keyboard.** Modifier keys: tap = applies to the next key; long-press = locked until tapped
  again. Holding a normal key auto-repeats on the PC.
- **Privacy.** The app locks after the configured time in the background, and drops the session
  whenever it leaves the foreground. Screenshots and the recents thumbnail are blocked by default
  (`FLAG_SECURE`; *Settings → Allow screenshots* lifts it everywhere except the lock screen).
  Backups are disabled.

### Building the app

Needs the Android SDK (API 35), NDK 27, `cargo-ndk`, and Rust targets `aarch64-linux-android`
and `x86_64-linux-android`.

```sh
./android/build-rust.sh          # Rust core for arm64 + x86_64, then Kotlin bindings
cd android && gradle assembleDebug
```

## Repository layout

| Crate / dir | What |
|---|---|
| `crates/dishus-proto` | wire messages, framing with size caps, validation, pairing URI, keycode policy |
| `crates/dishus-daemon` | daemon: iroh endpoint, allowlist gate, pairing, sessions, ABR, uinput, IPC, capture worker |
| `crates/dishus-cli` | `dishus` command |
| `crates/dishus-core` | phone-side core: encrypted vault, iroh client, UniFFI API; `examples/desktop_client.rs` test client |
| `android/` | Compose app |
| `packaging/` | systemd unit, AUR PKGBUILD, udev rule |
| `docs/` | plan & decisions, security, manual test checklist |

Development checks (the same ones CI runs):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check && cargo audit
```

`cargo run -p dishus-core --example desktop_client -- --help` gives a desktop "phone": it can
pair, stream video to stdout (`… view | ffplay -f h264 -`), send keys and renew.

## Status — what's verified

**Verified** on Arch, Hyprland, RTX 5070 Ti, NVIDIA driver, using the desktop test client:

| | |
|---|---|
| ✅ | 52 automated tests: protocol and validation, pairing (wrong token, reuse, closed window), expiry and renew, allowlist rejection of unknown and expired peers, revoke kicks a live session, kill/lockdown, silent-phone timeout releases keys, invalid message ends session, vault crypto |
| ✅ | Daemon reaches iroh relays; pairing via CLI QR link; connect in ~30 ms on LAN |
| ✅ | NVENC H.264 1080p from the GStreamer test source over QUIC; first frame ~0.9 s; adaptive 30 → 60 fps; output decodes cleanly with ffmpeg |
| ✅ | uinput virtual keyboard and pointer appear in Hyprland (`hyprctl devices`) |
| ✅ | Runs under the hardened systemd unit |
| ✅ | `cargo clippy -D warnings`, `cargo deny check` clean |

**Not verified yet:**

| | |
|---|---|
| ⚠️ | **Android app never compiled or run.** No Android SDK/NDK on the dev machine. The Kotlin binding names were checked against bindings generated from the Rust core, nothing more. Expect compile fixes. |
| ⚠️ | **Real screen capture through the portal** (the dialog, restore token and PipeWire path) was not exercised. Testing it pops a dialog on the desktop. Only the GStreamer test source went through the full encode/transport path. |
| ⚠️ | Actual key and pointer *effects* on the desktop: devices are created, but no input was injected into the live session during development. |
| ⚠️ | VA-API, x264 and openh264 encoder paths (plugins not installed here), X11 fallback, GNOME/KDE/niri portals. |
| ⚠️ | The CI workflow itself has never run (no remote yet). `makepkg` of the PKGBUILD **was** verified from a local checkout: it builds, runs the tests, and packages. |

**Known gaps and limits:**

- **Memory.** Daemon idle ≈ 26 MB RSS. **During a session the NVENC capture worker is ≈ 335 MB
  RSS** (≈ 97 MB anonymous; the rest is NVIDIA driver mappings), which is over the ~150 MB goal.
  It exits when the session ends. VA and x264 workers should be far smaller (unmeasured).
- **No zero-copy.** PipeWire buffers are SHM, which costs one copy per frame to the encoder. A
  DMA-BUF → CUDA import is future work.
- **Displayed RTT is app-level.** The latency number is the control-stream ping, not
  glass-to-glass latency. Measure that with the checklist in [docs/TESTING.md](docs/TESTING.md).
- **Keyboard layout.** The on-screen keyboard is US-QWERTY-labelled; keys follow the PC's layout.
- **One session at a time.** A newer connection replaces the older one.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
