# DiShus

Control your Linux PC from your Android phone. The phone shows the PC screen live and works as its
keyboard and touchpad, at home or from anywhere else, straight from phone to PC.

No account, no sign-up, no server of ours in the middle, no port forwarding.

<p align="center">
  <img src="docs/media/remote.gif" alt="Typing on the phone's DiShus keyboard: the command appears and runs in the PC's terminal, the phone shows the PC screen live, and swiping the touchpad moves the PC's pointer" width="100%">
  <br>
  <sub>Left: the PC. Right: the phone showing the PC screen, typing and moving the mouse.</sub>
</p>

The phone screen is split in two. The top part shows your PC and doubles as a touchpad. The bottom
part is a full PC keyboard, with Ctrl, Alt, Super, Esc, F1 to F12 and the arrow keys.

Free and open source, MIT or Apache-2.0.

## Where it's been tested

| | Works on |
|---|---|
| Desktops | Hyprland, KDE Plasma, X11 |
| Graphics | NVIDIA, Intel graphics, and PCs with no graphics card (the CPU encodes the video) |
| Phones | Samsung Galaxy phones, including the Galaxy Z Fold7 (Android 16) |
| Networks | home Wi-Fi, mobile data, other people's Wi-Fi; reconnects by itself when the network drops |

Not tested yet: GNOME, AMD graphics, phones from other brands. They use the same code paths and
should work. If you try one, open an issue and tell us how it went.

You need an Android phone with Android 9 or newer, and a Linux PC with a desktop running.

## Install

### 1. Download

From the [latest release](https://github.com/SunqdXX/dishus/releases/latest), get both files:

- `dishus-<version>-linux-x86_64.tar.gz` for the PC
- `DiShus-<version>.apk` for the phone

### 2. PC

Open a terminal where you downloaded it and run this (it uses the real file name, version
included):

```sh
tar xf dishus-*-linux-x86_64.tar.gz
cd dishus-*-linux-x86_64
./install.sh
```

The installer puts everything in your home folder and starts DiShus in the background. Before that
it checks your PC: it tries encoding video with every option available (NVIDIA, Intel or AMD
graphics, the CPU) and tells you what works.

If something is missing, it prints one install command for your distro and graphics card. Run it,
then run `./install.sh` again. On Arch with Intel graphics, for example:

```sh
sudo pacman -S --needed gst-plugin-va intel-media-driver gst-plugin-pipewire gst-plugins-ugly
```

You can run the same check any time with `dishus doctor`.

The installer may also ask you to run three `sudo` commands for keyboard and mouse access. That's a
one-time step: Linux only lets programs create a virtual keyboard after you allow it.

### 3. Phone

Open the APK on the phone to install it. Android will ask you to allow installing apps from your
browser or file manager. Start DiShus and pick an app password; it protects your pairings if
someone gets hold of your phone.

### 4. Pair

On the PC:

```sh
dishus pair
```

First your desktop asks which screen to share. Pick your whole screen, not a single window, and tick
"remember" (on Hyprland: "Allow a restore token"). Then a QR code appears. In the app, tap
**Scan QR**, or copy the `dishus://pair?…` link and use **Paste link**. The code works once and
expires after 60 seconds.

<img src="docs/media/pairing.gif" alt="Pairing: dishus pair shows a QR code and link on the PC; the phone pastes the link and the PC appears in its list" width="100%">

That's it. Tap **Connect** in the app.

A pairing lasts 6 months. Renew it from the app's settings while connected, or on the PC with
`dishus renew <phone>`.

## Using it

The top of the screen works like a laptop touchpad:

- move one finger to move the mouse, tap to click
- long-press or tap with two fingers to right-click
- double-tap and hold to drag
- two fingers to scroll, pinch to zoom the picture on the phone

On the keyboard, tap a modifier (Ctrl, Alt, Shift, Super) to apply it to the next key, or
long-press it to keep it held until you tap it again. The keys follow your PC's keyboard layout;
the labels on the phone are US layout.

The gear icon opens quick settings: frame rate, picture quality, resolution, which monitor to show,
renewing the pairing, and whether you're connected directly or through a relay.

If you lose your phone or something looks wrong, run `dishus kill` on the PC. It cuts every
connection at once and refuses new ones until you run `dishus unlock`.

## Using it after the PC reboots

DiShus starts when you log in to your desktop. If you want to reach the PC after a reboot while
you're away, turn on automatic login in your login manager (SDDM: `[Autologin]`, greetd:
`initial_session`). Keep a lock screen if other people can reach the PC; you'll see it on the
phone and can type your password from there.

### Hyprland: no screen-share popup on the PC

When the phone connects, Hyprland sometimes shows its "pick a screen" popup on the PC, and nobody
is there to click it. DiShus comes with a small helper that answers that popup for DiShus only
(whole screen, remembered). Every other app still gets your normal picker.

`install.sh` already copies the helper to `~/.local/bin`. To turn it on, add this to
`~/.config/hypr/xdph.conf` (use your own home path):

```
screencopy {
    allow_token_by_default = true
    custom_picker_binary = /home/YOU/.local/bin/dishus-share-picker
}
```

Then run `systemctl --user restart xdg-desktop-portal-hyprland`. If you already had a custom
picker, write its path on the first line of `~/.config/dishus/share-picker-fallback` and the
helper will keep using it for other apps.

The helper only answers while DiShus is waiting on a phone's request, for up to 20 seconds. When
you run `dishus pair` or `dishus setup-capture` at the PC, you always get the real popup. Every
answer it gives is logged: `journalctl -t dishus-share-picker`.

## When something goes wrong

**`tar: Cannot open: No such file or directory`**
The file name has the version in it. Run `ls ~/Downloads | grep dishus` and use the exact name.

**No picture on the phone**
Run `dishus doctor` on the PC. It tells you what's missing and the command to fix it.

**I shared the wrong window or screen and ticked "remember"**
Delete the saved choice, then pick again:
```sh
rm -f ~/.local/state/dishus/portal-restore-token*
dishus setup-capture
```

**The phone sees the screen but can't type or click**
Keyboard and mouse access isn't set up yet. See the next section.

### Keyboard and mouse access

DiShus types and clicks through a virtual keyboard and mouse (`/dev/uinput`), which most distros
only let root create. Run these once:

```sh
sudo install -Dm644 70-dishus-uinput.rules /etc/udev/rules.d/70-dishus-uinput.rules
echo uinput | sudo tee /etc/modules-load.d/uinput.conf
sudo udevadm control --reload && sudo modprobe uinput && sudo udevadm trigger
systemctl --user restart dishus
```

Run the first line from the unpacked release folder. From a source checkout the file is
`packaging/udev/70-dishus-uinput.rules`. The rule gives access to whoever is logged in at the PC
itself. It isn't installed automatically because on a PC shared by several people, it also means
being able to type into another user's session. Without it, DiShus still works, view-only.

**Anything else**
`dishus status` shows what the PC side is doing, and `journalctl --user -u dishus -e` shows its
log. Open an issue with both and we'll take a look.

## Uninstall

From the folder you installed from:

```sh
./install.sh --uninstall
```

Your pairings stay in `~/.config/dishus` and `~/.local/state/dishus` in case you reinstall. Delete
those two folders to remove everything. If you ran the keyboard/mouse `sudo` commands, also run
`sudo rm -f /etc/udev/rules.d/70-dishus-uinput.rules /etc/modules-load.d/uinput.conf`.

## Commands

```
dishus status          what's running, which encoder, who's connected
dishus pair            pair a new phone (add --skip-capture-setup to skip the screen popup)
dishus list            paired phones, when they expire, when they were last seen
dishus renew <phone>   extend a pairing (--days 1 to 180, default 180)
dishus revoke <phone>  remove a phone and disconnect it right away
dishus kill            cut everything and refuse all connections...
dishus unlock          ...until you run this (survives restarts)
dishus setup-capture   choose the shared screen again
dishus doctor          check that this PC can stream, and what to install if not
```

## Settings

DiShus works without any configuration. To change something, create
`~/.config/dishus/config.toml`; any setting you leave out keeps its default:

```toml
pc_name = "my-desktop"        # name shown on the phone (default: the hostname)
relays = []                   # empty = iroh's public relays, or your own: ["https://relay.example.org/"]
disable_relays = false        # true = only direct connections (same network or open port)
expiry_days = 180             # how long a pairing lasts, 1 to 180
pairing_ttl_secs = 60         # how long a pairing code stays valid, 10 to 60
udp_port = 0                  # 0 = random; set a fixed port if you want to forward it

[video]
encoder = "auto"              # auto, nvenc, vaapi, vaapi-lp, x264 or openh264
max_bitrate_kbps = 20000
show_cursor = true
```

Restart after editing: `systemctl --user restart dishus`.

## How it works

The phone and the PC talk over [iroh](https://iroh.computer), an encrypted peer-to-peer
connection (QUIC). Each side is identified by a key pair, and the PC only accepts phones that were
paired with it. When the two can reach each other directly, they do, even across different
networks. When they can't, the traffic goes through a relay, which only passes along encrypted
data it can't read. The PC opens no listening TCP port.

On the PC, `dishus-daemon` runs as a user service. When a phone connects, it starts a separate
capture process that grabs the screen (through the desktop's screen-share portal and PipeWire, or
X11), compresses it to H.264 with the best encoder available (NVIDIA NVENC, then Intel/AMD VA-API,
then x264 or openh264 on the CPU) and streams it. That process exits when you disconnect, so its
memory is freed. Typing and clicks from the phone become a virtual keyboard and mouse
(`/dev/uinput`). Power, sleep and wake keys are refused, and every held key is released if the
connection goes quiet for 5 seconds.

The stream adapts as it goes: it starts at 30 fps, moves up to 60 fps on a good connection, and
lowers quality on a bad one. Each video frame travels separately, so a lost frame is skipped
instead of stalling the picture.

On the phone, your pairings and the phone's identity are encrypted with your app password
(Argon2id) and an Android Keystore key. Screenshots of the app are blocked unless you allow them in
settings, and backups are off. The app disconnects when it leaves the foreground and locks after a
while in the background.

Design notes: [docs/PLAN.md](docs/PLAN.md). Security model: [docs/SECURITY.md](docs/SECURITY.md).

## Building from source

PC (Rust 1.89 or newer, GStreamer 1.24+ development files):

```sh
cargo build --release -p dishus-daemon -p dishus-cli
install -Dm755 -t ~/.local/bin/ target/release/dishus-daemon target/release/dishus
mkdir -p ~/.config/systemd/user
sed 's|^ExecStart=.*|ExecStart=%h/.local/bin/dishus-daemon|' packaging/systemd/dishus.service \
  > ~/.config/systemd/user/dishus.service
systemctl --user daemon-reload && systemctl --user enable --now dishus
```

Then set up [keyboard and mouse access](#keyboard-and-mouse-access).

On Arch you can instead build the package in `packaging/aur/PKGBUILD` with `makepkg -si`.

At runtime the PC needs GStreamer with its PipeWire plugin, `h264parse` and at least one H.264
encoder: NVIDIA (in `gst-plugins-bad-libs`, with the proprietary driver), Intel or AMD
(`gst-plugin-va`, plus `intel-media-driver` on Intel), or the CPU (`gst-plugins-ugly`). Plus the
screen-share portal for your desktop. `dishus doctor` checks all of it.

Phone app (Android SDK 35, NDK 27, `cargo-ndk`, Rust targets `aarch64-linux-android` and
`x86_64-linux-android`):

```sh
./android/build-rust.sh
cd android && gradle assembleDebug
```

`packaging/make-release.sh` builds the PC tarball and the signed APK into `dist/`. It needs
`DISHUS_KEYSTORE_PROPERTIES` pointing to the release key's properties file, which is never in the
repository.

### Repository layout

| Folder | What's in it |
|---|---|
| `crates/dishus-daemon` | the PC service: connections, pairing, video, keyboard and mouse |
| `crates/dishus-cli` | the `dishus` command |
| `crates/dishus-core` | the phone's Rust core (encrypted storage, connection), used by the app |
| `crates/dishus-proto` | the messages both sides exchange, with size limits and validation |
| `android/` | the phone app (Kotlin, Jetpack Compose) |
| `packaging/` | installer, systemd unit, udev rule, Hyprland helper, AUR package |
| `docs/` | design notes, security model, test checklist |

### Tests

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check && cargo audit
```

CI runs the same checks plus the Android build on every push. There are 73 automated tests, plus
a soak suite that runs every scenario (pairing, renew, revoke, kill, expiry, restarts) ten times in
a row against a real daemon. `cargo run -p dishus-core --example desktop_client -- --help` gives
you a desktop stand-in for the phone, handy for debugging.

## Known limits

- One phone connected at a time. A new connection replaces the old one.
- The latency shown in the app is the network round trip, not the full delay from the PC screen
  to your phone's screen.
- The keyboard labels are US layout; the keys themselves follow your PC's layout.
- Memory on the PC: about 36 MB idle, about 115 MB while streaming with NVIDIA.
- Each frame is copied once on its way to the encoder. Zero-copy capture is on the to-do list.

## Licence

MIT or Apache-2.0, your choice. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
