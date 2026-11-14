# DiShus — plan, milestones, decisions

## Milestones

| | Milestone | Scope |
|---|---|---|
| a | Local prototype | Portal/PipeWire capture → GStreamer H.264 (NVENC → VA → x264 → openh264) → frames; uinput keyboard + mouse; a desktop test client that pipes video into `ffplay` |
| b | iroh remote | QUIC via iroh 1.4, dial by EndpointId, hole-punching, configurable relays, no listening TCP port |
| c | Trust | Pairing window + one-time token, allowlist gate in `after_handshake`, expiry/renew, local IPC, `dishus` CLI, lockdown (`kill`) |
| d | Android | Kotlin + Compose app, Rust core via UniFFI, MediaCodec decode, 60/40 screen/keyboard UI, encrypted key store |
| e | Ship | systemd hardening, AUR PKGBUILD, CI (`cargo-audit`, `cargo-deny`), README, manual test checklist |
| f | Windows | Desktop Duplication + Media Foundation H.264, SendInput, per-user install (by yasinlol). Released in 0.1.3 |
| g | Windows test VM | Unattended Windows 11 VM under user-mode libvirt, `scripts/vm-*.sh`, `docs/WINDOWS-TESTING.md` |

## Status (2026-11-13)

Released: 0.1.0 to 0.1.4 (Linux, Android; Windows since 0.1.3). 0.1.4 turns Caps/Num/Scroll
Lock back off when a phone session ends.
0.31 (2026-11-14): PC sound on the phone (Linux PCs), steadier stream on weak links, phone
screen height as the default resolution. Versions from now on: 0.31, 0.69, 1.31, 1.69, ...

Tested on Windows in the VM with 0.1.4: install, firewall prompt, pairing, keyboard, mouse,
Caps Lock undo, auto-start, `status`, `doctor`, and a standard (non-admin) user keeping the same
PC id after a restart. The phone's part was played by the `desktop_client` example. Not tested:
a real phone against Windows, video.

Known broken: video on Windows PCs whose graphics adapter has no video processor (the VM;
probably also PCs without a GPU driver). Windows' software renderer can't create a device with
`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`, and the pipeline needs the video processor for BGRA to
NV12. Measured in the VM: without that flag, Desktop Duplication delivers frames.

## Stream quality (released in 0.31)

Local commits after 0.1.4: per-second stats on the phone (`DishusVideo` log) and the PC (DEBUG);
adaptive fps waits 5 s, then 10, 20, up to 30 s before going back to 60 after each failure;
frames behind a keyframe get twice the in-flight limit (fewer keyframe storms); the bitrate also
backs off on QUIC packet loss (over 2 %) and an RTT rise (over 2x normal + 40 ms); the phone's
default resolution is its screen height ("Phone"), "Native" one tap away. Reconnect unchanged
(a Wi-Fi to mobile data switch measured about 1 s).

Mobile data, direct path, same video and settings (Native, Adaptive, Medium), RTT ~44 ms:

| | before (3 min) | after (60 s) |
|---|---|---|
| 30/60 fps flips per minute | 5.8 | 4.0 |
| time at 60 fps | 34 % | 48 % |
| frames dropped on the PC per minute | 27 | 22 |
| keyframes per minute | 5 | 4 |
| worst delay growth | 750 ms | 336 ms |
| longest freeze (gap between stats) | 1.8 s | 1.2 s |
| median bitrate | 1239 kbit/s | 1853 kbit/s |

One short run on a live mobile network, so this is noisy. Still bad: about 22 dropped frames
and 4 keyframes a minute, and fps still changes 4 times a minute.

## Sound (released in 0.31; Linux PC + Android)

PC audio to the phone, off until the phone turns it on (then remembered). Protocol version is
the ALPN: `dishus/session/2` adds audio, released builds keep `session/1` and still work both
ways (e2e test; the new client against the released 0.1.3 daemon fell back to session/1).
Linux: a separate worker (`--capture-worker audio`) records the default output's monitor via
PipeWire, Opus 48 kHz stereo 20 ms, 64 kbit/s, one QUIC datagram per packet; it restarts the
capture when PipeWire drops it (seen twice in the first test, "target not found"). Under
congestion Opus drops to 24 kbit/s; with video at its minimum, or more than ~100 ms queued,
audio packets are dropped. Phone: MediaCodec Opus, low-latency AudioTrack, buffer trimmed to
~60-80 ms (hard cap 120), speaker button, volume slider, silent during calls.

Measured on the owner's phone: Wi-Fi 30 s, 0 lost, 0 dropped, mute/volume work, sound
116-126 ms behind the picture (log only, no correction; the owner found it fine by ear).
"Mobile data" 60 s with sound on: 2854 packets, 2 lost, 6 dropped, 7 underruns, buffer
median 68 ms; video fps median 31, 17 PC drops, 3 keyframes, longest gap 2.9 s (a reconnect
during the run). The app RTT median was 5 ms, so most of that run was probably on Wi-Fi:
it does not prove video is unaffected on a real mobile link.

Not done: Windows sound (on the branch windows-sound, waiting for a real Windows PC);
following a changed default output on Linux is untested.

### Sound check and fix (2026-11-14)

Owner's phone, music playing, 30 s each (phone counters):

| | packets/s | lost | underruns | buffer |
|---|---|---|---|---|
| DiShus alone | 50 | 0 | 0 | 84 ms |
| + PC screen recorder | 50 | 0 | 1 | 76 ms |
| + scrcpy recording the phone with its sound | 30 | 0 | 31 | 36 ms |

A second scrcpy run was clean, so scrcpy is not the cause by itself. The owner heard glitches
after reopening the app, after a long run, and after toggling the sound; toggling again fixed
it. In every bad stretch the phone got 27-39 packets/s, numbered without gaps, and the delay
did not grow: the PC handed over too few packets before numbering them (a phone or network
problem would show gaps or growing delay). Not reproduced on demand: 2 app reopens and 6
toggles by adb were clean. Fix: the PC counts packets per second from its capture;
3 seconds in a row with 1-39 (0 = idle output) restart the capture, at most once per 20 s, and
log a warning with the timestamp jumps. Packet numbers carry on. e2e test with a capture slowed
to 30/s (fails without the fix). Built for the owner to install by hand; not proven in real use yet.

## File send, phone to PC (branch file-send, local, 2026-11-14)

Built on `audio-watchdog` (main plus the sound restart fix), nothing from `windows-sound`.
Not pushed, not released.

- Protocol: new ALPN `dishus/session/3` (session/2 plus files). The phone offers 3, 2 and 1; old
  PCs keep working without the button. Feature checks compare versions (>= 2 for sound), so
  sound works on session/3 too. After Welcome the PC sends `FilesInfo` (on/off, max size).
  One QUIC stream per file: offer (name, size), the PC accepts or refuses with a reason, the
  bytes, then the PC answers with the name it really used.
- PC rules: off by default (`[files] enabled = false`), 2048 MB per file, refuses when the disk
  would have under 1 GB left, one file at a time. Only `Downloads/DiShus` (`XDG_DOWNLOAD_DIR`
  or user-dirs.dirs on Linux, the Downloads known folder on Windows). Names keep only the last
  path part, lose control, invisible (RTL override) and Windows-forbidden characters and
  leading or trailing dots; Windows device names get a `_` on every OS; max 200 bytes. Data
  goes to a new `.part` (O_EXCL, never executable) and is moved with a no-replace rename, so a
  taken name becomes `name (1).ext` and nothing is overwritten, even a file created during the
  transfer. Exactly the announced size or nothing; cancel, stalls (30 s), revoke, kill or the
  session ending delete the `.part`. Never opened. Windows adds the internet zone mark. Log
  line, desktop notification, and a `files` line in `dishus status`.
- App: paperclip button (session/3 only), "file sending is off on this PC" with how to turn it
  on, a one-time warning, Android's picker (several files), progress with speed and Cancel,
  a summary with the saved names. The open picker gets at most 60 s before it counts as leaving the app
  (it used to end the session and could lock the app).
- Proof: 12 unit tests (names, numbering, limits, no overwrite, leftover `.part` kept, not
  executable), 11 e2e tests (off, bytes and no overwrite, hostile names incl. `../../x`,
  `..\..\y`, `CON.txt`, cancel halfway, wrong sizes, busy, revoke mid-transfer, nothing on
  session/2, sound on session/3, phone core send and cancel). Each was seen failing first.
  A real daemon (temp dirs) with `desktop_client send-file`: 3.5 MB saved byte for byte,
  `big (1).bin`, `../../escape.txt` -> `escape.txt` in the folder, `CON.txt` -> `_CON.txt`,
  too big refused, cancel left nothing. Release APK builds with the same signing key.
- Not tested: a real phone (picker, grace, progress bar, cancel), anything on Windows (the
  Windows code is not even compiled yet: no Windows target here; CI would compile it on a
  push), a real slow network. Files come out 0600 in a 0700 folder on Linux (the daemon's
  private umask), which is safe but stricter than a browser download.

## Login-screen mode, plan only (2026-11-14, not started; waiting for the owner's OK)

Goal: after boot, at the login screen, type the password from an already paired phone.

Facts on the owner's PC (read only): SDDM 0.21 with an X11 greeter (no `DisplayServer`
set, X11 is the default); no LUKS; network is one system-wide Ethernet profile (no user
permissions), up before login; no Wi-Fi hardware; `/dev/uinput` is `root:input 0660` plus a
logind ACL for whoever has the active session (at the greeter that is the sddm user, not
the owner); the screen lock is caelestia's own (ext-session-lock, PAM `passwd` with
pam_faillock); SDDM's PAM also uses pam_faillock (defaults: 3 failures, 10 minutes locked).

1. A second daemon, `dishus-daemon --boot`, as a system unit `dishus-boot.service`
   (`Before=display-manager.service`), user `dishus-boot` (sysusers.d, no shell, no home),
   state in `/var/lib/dishus-boot` (0700). Own iroh identity, so it never clashes with the
   owner's daemon. It is idle (no input device, no capture, endpoint closed) whenever a
   graphical session is active on seat0, and comes back on logout.
2. Input: `/dev/uinput` only. Not the `input` group (that also reads the real keyboard). Either
   systemd `OpenFile=/dev/uinput` (the unit passes an open fd, no group at all; needs a check
   that the evdev crate can use an fd) or one udev line giving the `dishus-boot` group an ACL
   on `/dev/uinput` only.
3. Video: the greeter is an X11 server. SDDM runs `Xsetup` as root when it starts; one line
   (`xhost +SI:localuser:dishus-boot`) lets only that user read the greeter's screen, with
   GStreamer's `ximagesrc` (already a dependency). NVENC if the NVIDIA nodes are usable by
   the user, else x264. Typing blind is the fallback, but it is risky: a password typed into
   the user name field ends up in the journal as a user name.
4. Root is needed only to install: the unit, the user, the uinput rule, the Xsetup line.
   At run time nothing runs as root. Sandbox: ProtectSystem=strict, ProtectHome=yes,
   NoNewPrivileges, empty CapabilityBoundingSet, DevicePolicy=closed with DeviceAllow for
   uinput and the GPU nodes, RestrictAddressFamilies (inet, unix, netlink),
   SystemCallFilter=@system-service, RestrictNamespaces, LockPersonality, PrivateTmp with
   the X socket bound in.
5. Allowlist: its own `/var/lib/dishus-boot/devices.toml`, only phones turned on for login mode
   with `sudo dishus boot allow <phone>` from a logged-in session. No pairing ALPN at all:
   pairing is impossible at boot. The phone learns the login-mode address from the normal
   daemon during a normal session. `dishus revoke`, `dishus kill` and expiry reach the boot
   daemon through its control socket (owner-only, can remove but never add). Login-mode
   pairings last 30 days, renewed only from inside a session.
6. Brute force: the phone's key only exists while the app is unlocked, and the app asks its
   password again before every login-mode connection. On the PC: pam_faillock already locks
   after 3 failures (checked: SDDM's PAM includes system-login, then system-auth). The boot
   daemon adds limits a phone can't dodge (counting Enter presses can be, by clicking the
   login button): each login-mode session lasts at most 3 minutes, at most 3 sessions per hour
   per phone, and after 5 sessions in a day with no login following, login mode is off for
   that phone until re-allowed from a session. Every use is logged and shown as a notification
   at the next login.
7. Same Wi-Fi: nothing changes from today. QUIC with keys, unknown phones are dropped after
   the handshake before any data, no TCP port, nothing shown without a paired key. Optional
   `boot.direct_only`.
8. Off by default: nothing is installed until `sudo dishus boot enable`; `sudo dishus boot
   disable` removes all of it.
9. Not possible: the LUKS prompt (it runs before the system and its network start), the UEFI
   boot menu.

Lock screen while logged in (the normal daemon, today): input should already work (the
virtual keyboard is a normal keyboard to Hyprland, and the logind ACL stays with the owner
while locked). Whether the portal picture shows the lock screen on Hyprland is not known; it
needs a 2-minute test with the owner (lock, connect, look). Note: this means a stolen,
unlocked, paired phone can already try passwords on the lock screen today; pam_faillock limits
that to 3 tries per 10 minutes.

Windows (later, maybe): a SYSTEM service that starts a helper in the console session on the
Winlogon desktop (DXGI duplication and SendInput work there only as SYSTEM), plus SendSAS for
Ctrl+Alt+Del. This is how RustDesk does it. It would also fix UAC prompts. It is a network
service running as SYSTEM: a big risk and a lot of code. Only worth it if friends really ask.

Unknowns, all untested: the Xsetup grant, passing a uinput fd to the service, NVENC as a
system user, telling the greeter apart from a user session in logind, and whether the lock
screen shows in the picture on Hyprland.

## Next (none started; each needs the owner's go)

1. Later (the owner postponed it on 2026-11-14): Windows video without a video processor, a
   CPU BGRA to NV12 fallback for the software encoder. GPU path stays the default. About 150
   lines in `capture/win/`, the converter testable on Linux. Needs a test on a real Windows PC
   with a GPU afterwards (the owner or a friend), to show the GPU path is unchanged.
2. `dishus doctor` on Windows blames hybrid graphics for that failure, and the daemon logs
   "encoders: software" right after its capture check failed. Both messages are wrong.
3. A real phone against the Windows VM (needs the owner and a phone, about 5 minutes).
4. Turning the PC on from the phone (Wake-on-LAN). The owner's request was "the PC opener";
   that reading is not confirmed yet. Needs Wake-on-LAN enabled in the BIOS by the owner, and
   only works from the same network unless the router or another device can send the packet.
5. Controlling the login screen: planned above, waiting for the owner's OK.
6. iOS: still blocked by Apple's $99/year. Free sideloading expires every 7 days and needs a
   Mac or AltStore.
7. Parked: the setup-capture token-reuse bug, the zero-install plan.

## Decisions (made without asking; revisit freely)

1. **Video transport = one QUIC uni-stream per encoded frame** (the "drop-old-frames stream"
   option), not datagrams. QUIC fragments and retransmits inside a frame, so the receiver
   never sees a half frame. Congestion is handled on the *sender*: if more than N frames are
   un-acked, new frames are dropped until the next IDR, and an IDR is forced. Lost frames
   therefore never reach the decoder as corruption. The phone can still request a keyframe
   (decoder error, resume from background).
2. **Wire format = `postcard` (serde) inside u32 length-prefixed frames**, 64 KiB cap on the
   control stream, 8 MiB cap per video frame. Every decoded message runs `validate()`.
3. **Two ALPNs**: `dishus/session/1` and `dishus/pair/1`. An `EndpointHooks::after_handshake`
   gate rejects a session connection whose TLS-proven EndpointId is not on the allowlist (or is
   expired, or the daemon is in lockdown), and rejects every pairing connection while the
   pairing window is closed. No application bytes are read before the gate.
4. **Pointer = touchpad (relative) mode.** Absolute positioning through uinput gets classified as
   a tablet/touchscreen by libinput and is mapped differently per compositor and monitor
   layout. Relative mode works everywhere. Tap = click at the cursor, as on a laptop touchpad.
5. **`dishus kill` = cut all sessions + persistent lockdown** until `dishus unlock`. A panic
   button that the next reconnect undoes is not a panic button.
6. **Capture permission is obtained while the user is at the PC.** `dishus pair` (and
   `dishus setup-capture`) runs the portal once with `persist_mode = until revoked` and stores the
   restore token, so remote sessions don't hit a dialog nobody is there to click. All monitors
   are selected at once so the phone can switch monitors without another dialog.
7. **Capture only runs while a session is active.**
8. **Power/sleep/wake keycodes are rejected** by the daemon (KEY_POWER, KEY_SLEEP, KEY_WAKEUP,
   KEY_SUSPEND). Everything else in 1..=248 plus mouse buttons is allowed.
9. **One active session at a time.** A newer authenticated session replaces the older one (this
   makes reconnects after a network change seamless); both events are notified.
10. **Keycodes follow the PC's XKB layout.** The phone sends evdev keycodes; the keyboard UI is
    labelled US-QWERTY.
11. **GStreamer** over ffmpeg bindings: it already has the PipeWire source and NVENC/VA/x264
    encoders as plugins, and the Rust bindings are mature. The daemon links only `gstreamer`,
    `gstreamer-app`, `gstreamer-video`.
12. **Capture/encode runs in a per-session worker process** (`dishus-daemon --capture-worker`).
    Measured on RTX 5070 Ti: any process that loads NVENC carries ~300 MB RSS (NVIDIA userspace
    driver mappings; ~90 MB anonymous), and GStreamer keeps the CUDA context alive after the
    pipeline stops. Splitting it out keeps the network-facing daemon at ~26 MB, means nothing
    GPU-related is resident between sessions, and isolates media-stack crashes from the process
    that owns the input devices.
13. **No CUDA colour conversion unless downscaling.** NVENC takes BGRx from system memory directly;
    `cudaconvertscale` would pull in NVRTC (~100 MB). With `max_height` set, `cudascale` is used.
14. **Zero-copy DMA-BUF capture is not implemented yet.** PipeWire is forced to SHM buffers; one
    memcpy per frame. A DMA-BUF → CUDA import path is future work.
