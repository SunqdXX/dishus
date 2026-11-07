# DiShus — plan, milestones, decisions

## Milestones

| | Milestone | Scope |
|---|---|---|
| a | Local prototype | Portal/PipeWire capture → GStreamer H.264 (NVENC → VA → x264 → openh264) → frames; uinput keyboard + mouse; a desktop test client that pipes video into `ffplay` |
| b | iroh remote | QUIC via iroh 1.4, dial by EndpointId, hole-punching, configurable relays, no listening TCP port |
| c | Trust | Pairing window + one-time token, allowlist gate in `after_handshake`, expiry/renew, local IPC, `dishus` CLI, lockdown (`kill`) |
| d | Android | Kotlin + Compose app, Rust core via UniFFI, MediaCodec decode, 60/40 screen/keyboard UI, encrypted key store |
| e | Ship | systemd hardening, AUR PKGBUILD, CI (`cargo-audit`, `cargo-deny`), README, manual test checklist |

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
