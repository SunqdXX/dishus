# DiShus

Use your Android phone as the screen, keyboard and mouse of your PC. Works on **Windows** and
**Linux**, at home or from another city, straight from your phone to your PC.

No account, no sign-up, no server of ours in the middle, no port forwarding. Free and open source.

<p align="center">
  <img src="docs/media/remote.gif" alt="Typing on the phone's DiShus keyboard: the command appears and runs in the PC's terminal, the phone shows the PC screen live, and swiping the touchpad moves the PC's pointer" width="100%">
  <br>
  <sub>Left: the PC. Right: the phone showing the PC screen, typing and moving the mouse.</sub>
</p>

The top part of the phone shows your PC and works as a touchpad. The bottom part is a full PC
keyboard, with Ctrl, Alt, Super, Esc, F1 to F12 and the arrow keys.

## Download

Everything is on the [latest release](https://github.com/SunqdXX/dishus/releases/latest) page.

| You have | Download |
|---|---|
| a Windows PC | `DiShus-<version>-windows-x86_64.exe` |
| a Linux PC | `dishus-<version>-linux-x86_64.tar.gz` |
| an Android phone (Android 9 or newer) | `DiShus-<version>.apk` |

You need one PC file and the phone app. iPhone isn't supported, see the [FAQ](#faq).

## Where it's been tested

| | |
|---|---|
| Windows | works, tested by a friend on his own PC |
| Linux | Hyprland, KDE Plasma, X11 |
| Graphics | NVIDIA, Intel graphics, and PCs with no graphics card (the CPU encodes the video) |
| Phones | Samsung Galaxy phones, including the Galaxy Z Fold7 (Android 16) |
| Networks | home Wi-Fi, mobile data, other people's Wi-Fi; reconnects by itself when the network drops |

Not tested yet: GNOME, AMD graphics, phones from other brands. They use the same code and should
work. If you try one, open an issue and tell us how it went.

## Setup on Windows

1. Double-click `DiShus-<version>-windows-x86_64.exe`. Nothing else needs to be installed, and it
   doesn't ask for administrator rights.
2. The exe isn't signed (that costs money every year), so Windows says "Windows protected your
   PC". Click **More info**, then **Run anyway**. Some antivirus programs may complain for the same
   reason.
3. The first time, the Windows firewall asks whether DiShus may use the network. Click **Allow**.
4. It installs DiShus for your user, starts it, and shows a QR code. Keep it open and go to
   [the phone](#setup-on-the-phone).

From then on DiShus runs in the background and starts by itself when you sign in. To pair another
phone later, open the exe again or run `dishus pair`.

## Setup on Linux

Open a terminal in the folder you downloaded to:

```sh
tar xf dishus-*-linux-x86_64.tar.gz
cd dishus-*-linux-x86_64
./install.sh
```

The installer puts everything in your home folder and starts DiShus in the background. First it
checks your PC: it tries encoding video with every option there is (NVIDIA, Intel or AMD graphics,
the CPU) and tells you what works. If something is missing, it prints one install command for your
distro and graphics card. Run it, then `./install.sh` again. On Arch with Intel graphics, for
example:

```sh
sudo pacman -S --needed gst-plugin-va intel-media-driver gst-plugin-pipewire gst-plugins-ugly
```

It may also print three `sudo` commands for keyboard and mouse access. Run them once (more in
[Keyboard and mouse access](#keyboard-and-mouse-access)).

Then pair:

```sh
dishus pair
```

Your desktop first asks which screen to share. Pick the **whole screen**, not a single window, and
tick "remember" (on Hyprland: "Allow a restore token"). Then a QR code appears.

The prebuilt Linux files need a recent system (glibc 2.39 or newer: Arch, Ubuntu 24.04+,
Debian 13+, Fedora 40+). On anything older, [build it yourself](#building-from-source).

## Setup on the phone

1. Open the APK on the phone. Android asks you to allow installing apps from your browser or file
   manager; allow it.
2. Start DiShus and choose an app password. It locks the app and encrypts your pairings. There's
   no way to recover it, so don't forget it.
3. Tap **Scan QR** and point the camera at the QR code on the PC. (Or copy the `dishus://pair?…`
   link the PC shows and use **Paste link**.) The code works once and only for 60 seconds.
4. Tap your PC, then **Connect**.

<img src="docs/media/pairing.gif" alt="Pairing: dishus pair shows a QR code and link on the PC; the phone pastes the link and the PC appears in its list" width="100%">

A pairing lasts 6 months. Renew it from the app's settings while connected, or on the PC with
`dishus renew <phone>`.

## Using it

The top of the screen works like a laptop touchpad:

- move one finger to move the mouse, tap to click
- long-press or tap with two fingers to right-click
- double-tap and hold to drag
- two fingers to scroll, pinch to zoom the picture on the phone

On the keyboard, tap Ctrl, Alt, Shift or Super to apply it to the next key, or long-press it to
keep it held until you tap it again. The keys follow your PC's keyboard layout; the labels on the
phone are US layout.

The gear icon opens quick settings: frame rate, picture quality, resolution, which monitor to show,
renewing the pairing, and whether you're connected directly or through a relay.

Lost your phone, or something looks wrong? Run `dishus kill` on the PC. It cuts every connection
at once and refuses new ones until you run `dishus unlock`. To remove one phone for good:
`dishus revoke <phone>`.

## Reaching your PC after it restarts

DiShus starts when you log in. To reach the PC after a restart while you're away, turn on
automatic login:

- **Windows:** run `netplwiz` and untick "Users must enter a user name and password".
- **Linux:** in your login manager (SDDM: `[Autologin]`, greetd: `initial_session`).

On Linux, keep a lock screen if other people can get to the PC: you'll see it on the phone and can
type your password from there. On Windows that doesn't work, because Windows doesn't let any app
see or use its lock screen. Auto-login straight to the desktop is the way there.

## FAQ

**Does it work from another city?**
Yes. Your phone connects straight to your PC over the internet, on Wi-Fi or mobile data. The delay
is about the ping between the two places, usually 20 to 80 ms inside one country. If a direct
connection isn't possible, the data goes through a relay, which adds a bit of delay.

**Does my screen go through your servers?**
No. We don't run any servers. Relays (iroh's public ones by default, or your own) only forward
encrypted data they can't read.

**Is it free?**
Yes. No ads, no account, no paid version.

**iPhone?**
Not for now. Apple only lets people install apps through the App Store or TestFlight, and both need
a developer account that costs $99 a year. A web version that runs in Safari might come later.

**Mac?**
Not yet. Nobody has built it.

**Can I pair one phone with several PCs?**
Yes, they all show up in the app. You control one at a time.

**Is it safe?**
Only phones you paired can connect, each one is identified by its own key, and everything is
encrypted end to end. The app is locked with your password. Power, sleep and wake keys from the
phone are refused. Details: [docs/SECURITY.md](docs/SECURITY.md).

## When something goes wrong

Start with `dishus doctor` on the PC. It checks everything DiShus needs and tells you what's
missing.

**Windows says "Windows protected your PC" or the antivirus blocks it**
That's because the exe isn't signed. Click **More info**, then **Run anyway**, or allow it in the
antivirus.

**`tar: Cannot open: No such file or directory`** (Linux)
The file name has the version in it. Run `ls ~/Downloads | grep dishus` and use the exact name.

**I shared the wrong window or screen and ticked "remember"** (Linux)
Delete the saved choice, then pick again:
```sh
rm -f ~/.local/state/dishus/portal-restore-token*
dishus setup-capture
```

**The phone sees the screen but can't type or click**
On Linux, keyboard and mouse access isn't set up yet, see the next section. On Windows, the window
you're trying to use runs as administrator, and Windows doesn't let normal apps type into those.

**Anything else**
`dishus status` shows what DiShus is doing. Logs are in `journalctl --user -u dishus -e` on Linux
and in `%LOCALAPPDATA%\dishus\logs` on Windows. Open an issue with both and we'll take a look.

### Keyboard and mouse access

DiShus types and clicks through a virtual keyboard and mouse (`/dev/uinput`), which most distros
only let root create. Run these once, from the unpacked release folder:

```sh
sudo install -Dm644 70-dishus-uinput.rules /etc/udev/rules.d/70-dishus-uinput.rules
echo uinput | sudo tee /etc/modules-load.d/uinput.conf
sudo udevadm control --reload && sudo modprobe uinput && sudo udevadm trigger
systemctl --user restart dishus
```

From a source checkout the file is `packaging/udev/70-dishus-uinput.rules`. The rule gives access
to whoever is logged in at the PC itself. It isn't installed automatically because on a PC shared
by several people, it also lets one user type into another user's session. Without it, DiShus
still works, view-only.

### Hyprland: no screen-share popup on the PC

When the phone connects, Hyprland sometimes shows its "pick a screen" popup on the PC, and nobody
is there to click it. DiShus comes with a small helper that answers that popup for DiShus only
(whole screen, remembered). Every other app still gets your normal picker.

`install.sh` already copies the helper to `~/.local/bin`. To turn it on, add this to
`~/.config/hypr/xdph.conf` (with your own home path):

```
screencopy {
    allow_token_by_default = true
    custom_picker_binary = /home/YOU/.local/bin/dishus-share-picker
}
```

Then run `systemctl --user restart xdg-desktop-portal-hyprland`. If you already had a custom
picker, write its path on the first line of `~/.config/dishus/share-picker-fallback` and the helper
keeps using it for other apps. It only answers while DiShus is waiting on a phone, for up to 20
seconds, and logs every answer to `journalctl -t dishus-share-picker`. `dishus pair` and
`dishus setup-capture` always show you the real popup.

## Uninstall

**Windows:** run `dishus uninstall`.

**Linux:** from the folder you installed from, run `./install.sh --uninstall`. If you ran the
keyboard/mouse `sudo` commands, also run
`sudo rm -f /etc/udev/rules.d/70-dishus-uinput.rules /etc/modules-load.d/uinput.conf`.

Your pairings are kept in case you install it again. To remove everything, also delete
`%APPDATA%\dishus` and `%LOCALAPPDATA%\dishus` on Windows, or `~/.config/dishus` and
`~/.local/state/dishus` on Linux.

## Commands

```
dishus status          what's running, which encoder, who's connected
dishus pair            pair a new phone (add --skip-capture-setup to skip the screen popup)
dishus list            paired phones, when they expire, when they were last seen
dishus renew <phone>   extend a pairing (--days 1 to 180, default 180)
dishus revoke <phone>  remove a phone and disconnect it right away
dishus kill            cut everything and refuse all connections...
dishus unlock          ...until you run this (survives restarts)
dishus setup-capture   (Linux) choose the shared screen again
dishus doctor          check that this PC can stream, and what to install if not
dishus uninstall       (Windows) remove DiShus, keep the pairings
```

## Settings

DiShus works without any configuration. To change something, create `config.toml` in
`~/.config/dishus/` on Linux or `%APPDATA%\dishus\` on Windows. Anything you leave out keeps its
default:

```toml
pc_name = "my-desktop"        # name shown on the phone (default: the computer's name)
relays = []                   # empty = iroh's public relays, or your own: ["https://relay.example.org/"]
disable_relays = false        # true = only direct connections (same network or open port)
expiry_days = 180             # how long a pairing lasts, 1 to 180
pairing_ttl_secs = 60         # how long a pairing code stays valid, 10 to 60
udp_port = 0                  # 0 = random; set a fixed port if you want to forward it

[video]
encoder = "auto"              # Linux: auto, nvenc, vaapi, vaapi-lp, x264, openh264
                              # Windows: auto, nvenc, qsv, amf, hardware, software
max_bitrate_kbps = 20000
show_cursor = true
```

Restart DiShus after editing: on Linux `systemctl --user restart dishus`, on Windows open the
installer exe again (or sign out and back in).

## How it works

The phone and the PC talk over [iroh](https://iroh.computer), an encrypted peer-to-peer
connection (QUIC). Each side has its own key pair, and the PC only accepts phones that were paired
with it. When the two can reach each other directly they do, even across different networks; when
they can't, a relay passes along the encrypted data. The PC doesn't open any port to the internet.

When a phone connects, the PC starts a separate capture process that grabs the screen, compresses
it to H.264 video and streams it. It exits when you disconnect, so its memory is freed.

- **Linux:** the screen comes from your desktop's screen-share portal (PipeWire) or X11. Video is
  encoded with the best option available: NVIDIA NVENC, then Intel/AMD VA-API, then x264 or
  openh264 on the CPU. Typing and clicks go through a virtual keyboard and mouse (`/dev/uinput`).
- **Windows:** only what ships with Windows. Desktop Duplication grabs the screen, Media Foundation
  encodes it (on the graphics card when there is one), SendInput types and clicks. The local
  control channel is a named pipe only your Windows account can open, and the key files are
  readable by your account only.

The stream adapts as it goes: it starts at 30 fps, goes up to 60 fps on a good connection, and
lowers quality on a bad one. Each video frame travels on its own, so a lost frame is skipped
instead of freezing the picture. If the connection goes quiet for 5 seconds, every held key and
mouse button is released.

On the phone, your pairings and the phone's identity are encrypted with your app password
(Argon2id) and an Android Keystore key. Screenshots of the app are blocked unless you allow them in
settings, and backups are off. The app disconnects when you leave it and locks after a while in
the background.

Design notes: [docs/PLAN.md](docs/PLAN.md). Security model: [docs/SECURITY.md](docs/SECURITY.md).

## Building from source

**Linux** (Rust 1.89 or newer, GStreamer 1.24+ development files):

```sh
cargo build --release -p dishus-daemon -p dishus-cli
install -Dm755 -t ~/.local/bin/ target/release/dishus-daemon target/release/dishus
mkdir -p ~/.config/systemd/user
sed 's|^ExecStart=.*|ExecStart=%h/.local/bin/dishus-daemon|' packaging/systemd/dishus.service \
  > ~/.config/systemd/user/dishus.service
systemctl --user daemon-reload && systemctl --user enable --now dishus
```

Then set up [keyboard and mouse](#keyboard-and-mouse-access). On Arch you can instead build the
package in `packaging/aur/PKGBUILD` with `makepkg -si`. At runtime the PC needs GStreamer with its
PipeWire plugin, `h264parse` and at least one H.264 encoder, plus your desktop's screen-share
portal; `dishus doctor` checks all of it.

**Windows** (Rust with the MSVC toolchain, Visual Studio Build Tools with "Desktop development with
C++"): `packaging\windows\build.ps1` builds the one-file installer into `dist\`. The release exe
is built the same way by GitHub Actions.

**Phone app** (Android SDK 35, NDK 27, `cargo-ndk`, Rust targets `aarch64-linux-android` and
`x86_64-linux-android`):

```sh
./android/build-rust.sh
cd android && gradle assembleDebug
```

`packaging/make-release.sh` builds the Linux tarball and the signed APK into `dist/`. It needs
`DISHUS_KEYSTORE_PROPERTIES` pointing to the release key's properties file, which is never in the
repository.

### What's where

| Folder | What's in it |
|---|---|
| `crates/dishus-daemon` | the PC program: connections, pairing, video, keyboard and mouse |
| `crates/dishus-cli` | the `dishus` command, and the Windows installer |
| `crates/dishus-core` | the phone's Rust core (encrypted storage, connection), used by the app |
| `crates/dishus-proto` | the messages both sides exchange, with size limits and checks |
| `crates/dishus-win` | Windows helpers: private files, the control pipe, the registry |
| `android/` | the phone app (Kotlin, Jetpack Compose) |
| `packaging/` | Linux installer, systemd unit, udev rule, Hyprland helper, AUR package, Windows build script |
| `docs/` | design notes, security model, test checklist |

### Tests

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check && cargo audit
```

Every push runs these on Linux and Windows, plus the Android build. There's
also a soak suite that runs every scenario (pairing, renew, revoke, kill, expiry, restarts) ten
times in a row against a real PC. `cargo run -p dishus-core --example desktop_client -- --help`
gives you a stand-in for the phone on your PC, handy for debugging.

## Known limits

- One phone connected at a time. A new connection replaces the old one.
- The delay shown in the app is the network round trip, not the full delay from the PC screen to
  your phone.
- The keyboard labels are US layout; the keys follow your PC's layout.
- On Windows, the lock screen, the Ctrl+Alt+Del screen and UAC "allow this app" prompts can't be
  seen or used from the phone (Windows doesn't allow it). They show up frozen until they're gone.
- PC memory: about 36 MB idle and 115 MB while streaming (Linux with NVIDIA).

## Thanks

Windows support was built by [yasinlol](https://github.com/yasinlol).

## Licence

MIT or Apache-2.0, your choice. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
