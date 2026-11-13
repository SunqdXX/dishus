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

Tested on Windows in the VM with 0.1.4: install, firewall prompt, pairing, keyboard, mouse,
Caps Lock undo, auto-start, `status`, `doctor`, and a standard (non-admin) user keeping the same
PC id after a restart. The phone's part was played by the `desktop_client` example. Not tested:
a real phone against Windows, video.

Known broken: video on Windows PCs whose graphics adapter has no video processor (the VM;
probably also PCs without a GPU driver). Windows' software renderer can't create a device with
`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`, and the pipeline needs the video processor for BGRA to
NV12. Measured in the VM: without that flag, Desktop Duplication delivers frames.

## Stream quality (2026-11-14, not released yet)

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

## Sound (2026-11-14, Linux PC + Android, not released yet)

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

Not done: Windows (WASAPI loopback + libopus, needs CI and a real Windows PC); one 4 s
stretch at 38 packets/s on Wi-Fi was not explained; following a changed default output is
untested.

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
5. Controlling the login screen, to type the password from the phone. Windows: a system service
   that captures the Winlogon desktop (would also cover UAC prompts). Linux: a root helper for
   the greeter, which the portal can't capture. Big and security-sensitive. The boot menu
   itself can't be done in software.
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
