# Manual test checklist — Hyprland + NVIDIA + a real Android phone

The automated tests (`cargo test --workspace`) cover protocol, pairing, expiry, allowlist rejection,
revoke, kill and key release against a fake capture/input backend. This checklist covers what they
can't: the real portal, the real GPU encoder, real input, a real phone on a real network.

Tick each box. Write the measured numbers next to them. If anything fails, note the
`journalctl --user -u dishus -b` output (run with `DISHUS_LOG=debug` for detail).

## 0. Preparation

- [ ] Daemon installed, `systemctl --user enable --now dishus` → `systemctl --user status dishus`
      is active.
- [ ] `dishus status` shows:
  - [ ] the PC id and at least one relay URL
  - [ ] `input` = uinput
  - [ ] `capture` naming the NVENC encoder (the probe runs in the background ~1–2 s after
        start)
- [ ] `hyprctl devices` lists `dishus-virtual-keyboard` and `dishus-virtual-pointer`.
- [ ] App installed on the phone (debug APK from `gradle assembleDebug`).
- [ ] Phone on **mobile data** (not the PC's Wi-Fi) for the remote parts, or a different network.

## 1. First run and vault

- [ ] **First start:** the app asks for a new password. With fewer than 8 characters, Create stays
      disabled.
- [ ] **After Create:** the biometric offer appears (if the phone has one). Enable it.
- [ ] **Biometric unlock:** kill the app from recents and reopen. The biometric prompt shows;
      unlocking works.
- [ ] **Wrong password:** fall back to the password and type a wrong one → "Wrong password".
- [ ] **Recents thumbnail:** it is blank, or shows the app icon only (FLAG_SECURE).
- [ ] **Screenshot:** attempting one inside the app is blocked.

## 2. Pairing

- [ ] **Window closed by default:** before pairing, scanning an old or used QR fails
      ("pairing refused"). The window is closed.
- [ ] **Capture dialog:** `dishus pair` opens the Hyprland screen-share picker. Select the
      monitor(s). **Tick "Allow a restore token".**
- [ ] **QR + link:** after the picker, the CLI prints a QR + link and a 60 s countdown.
- [ ] **Scan:** scan in the app → the CLI prints the new phone; a desktop notification appears.
      The app lists the PC with "expires in 30 d".
- [ ] **`dishus list`:** shows the phone, `30d`. Its id matches *Settings → This phone's id*.
- [ ] **TTL:** run `dishus pair --skip-capture-setup`, wait > 60 s, then scan → refused.
- [ ] **Single use:** run `dishus pair` again and scan the *same* QR with a second app install, or
      paste the link twice → the second use is refused.

## 3. Connect, screen, input

- [ ] **Connect:** tap the PC. Time from tap to first picture: ____ s. Target: under ~3 s on
      mobile data.
- [ ] **No dialog after reboot:** the PC shows **no** portal dialog when the phone connects (the
      restore token worked). Reboot the PC (auto-login) and connect again: still no dialog.
- [ ] **Chip:** shows `direct` or `relay` and an RTT. Note both: ____ / ____ ms.
- [ ] **Picture:** sharp, correct aspect, cursor visible. Open the gear: encoder `nvh264enc`,
      resolution and fps shown.
- [ ] **Keyboard:** typing in a terminal gives the expected characters (PC layout applies).
  - [ ] Holding a key auto-repeats.
  - [ ] Shift tap → next letter uppercase, then Shift releases.
  - [ ] Shift long-press → locked (green) for several letters; tap again unlocks.
  - [ ] Ctrl+C in a terminal works.
  - [ ] Super+Enter (or your terminal bind) opens a terminal.
  - [ ] Alt+Tab-style binds work.
- [ ] **Blocked keys:** KEY_POWER/KEY_SLEEP are not on the keyboard. The PC must not suspend from
      anything the phone sends.
- [ ] **Touchpad:**
  - [ ] one-finger move moves the cursor smoothly
  - [ ] tap = left click
  - [ ] long-press = right click
  - [ ] two-finger tap = right click
  - [ ] double-tap-and-hold = drag (moving a window or selecting text)
  - [ ] two-finger vertical and horizontal scroll
  - [ ] pinch zooms the phone view and back to 1×
- [ ] **Monitors** (dual monitor only): switch monitor in quick settings without any dialog.
- [ ] **Quality settings:**
  - [ ] Resolution 720p → the picture gets softer and the bitrate drops.
  - [ ] FPS 30 / 60 / Adaptive → the fps in the gear sheet follows.
  - [ ] Adaptive climbs to 60 on a good link.
- [ ] **Memory during a session:** `ps -o rss,cmd -C dishus-daemon` → daemon ____ MB, worker
      ____ MB.
- [ ] **Memory after disconnect:** the worker process is gone.

## 4. Latency check

The in-app RTT is the control-channel ping only. Measure the real numbers:

1. **Glass-to-glass (video).** On the PC, open a big millisecond clock. For example run
   `while :; do printf '\r%s' "$(date +%S.%3N)"; done` in a terminal with a huge font, or open an
   online "ms stopwatch" page. Put the phone next to the monitor and take photos (or slow-motion
   video) of both screens with a *third* camera. Difference between the PC and phone readings:
   - [ ] LAN direct: ____ ms (expect roughly 40–100 ms)
   - [ ] mobile data, direct: ____ ms
   - [ ] via relay (set *Direct connections only* off and block UDP, or test on a hostile network):
     ____ ms
2. **Input-to-photon.** Film the phone and the monitor together with slow-motion video. Tap a key
   on the phone keyboard while a text editor is focused. Count frames from the finger touching to
   the character appearing *on the PC monitor*: ____ ms (frames × 1000 / video fps).
3. **Under load.** Play a full-screen video on the PC while viewing:
   - [ ] the fps in the gear sheet adapts and the stream recovers without a frozen or smeared
         picture (keyframe recovery)
   - [ ] typing stays responsive

## 5. Robustness

- [ ] **Wi-Fi ↔ mobile:** toggle Wi-Fi off and on during a session. The app shows "Reconnecting…"
      and resumes within a few seconds. No key is stuck afterwards: type, and check that Shift
      isn't held.
- [ ] **Background:** while holding a modifier (lock Ctrl), press Home on the phone. The session
      drops and **Ctrl is released on the PC**. Check by typing on the PC's real keyboard. Reopen
      the app → it auto-reconnects (and asks to unlock if auto-lock passed).
- [ ] **Dead phone:** kill the app process (or enable airplane mode) while holding a key. Within
      ~5 s the PC releases it; the notification says the phone disconnected.
- [ ] **Daemon restart:** `systemctl --user restart dishus` during a session → the app reconnects.
- [ ] **Second phone/session** (if you have a second device): the newer session replaces the older
      one; the older one shows "Another session took over".

## 6. Expired device

To avoid waiting 30 days, force the expiry:

```sh
systemctl --user stop dishus
# set expires_at of the phone to a past timestamp (keep the file 0600)
sed -i 's/^expires_at = .*/expires_at = 1000/' ~/.config/dishus/devices.toml   # single-device file
stat -c %a ~/.config/dishus/devices.toml   # must print 600
systemctl --user start dishus
```

- [ ] `dishus list` shows the phone as `EXPIRED`.
- [ ] The app cannot connect. Because you only changed the PC's copy, the phone still thinks the
      pairing is valid. It shows "Can't reach PC" and keeps retrying with backoff: the PC drops it
      silently, exactly like a stranger. The message "The pairing with this PC has expired" only
      appears when the phone's own expiry date has passed, as it does after a real 30 days.
- [ ] **Silent drop:** with `DISHUS_LOG=debug`, the journal shows the rejection at the gate and
      nothing from the session handler or the parser.
- [ ] **Renew:** `dishus renew <phone> --days 7` → the app connects again on its next retry
      (≤ 15 s). `dishus list` shows `7d`.
- [ ] **Renew from the phone:** in quick settings → *Renew 6 months* → notice "Pairing renewed".
      `dishus list` shows `60d`; the app's PC list shows "expires in 60 d".
- [ ] **Expiry mid-session:** set the expiry ~1 minute ahead, restart, connect, and wait. Within
      15 s after expiry the session ends and keys are released.

## 7. Revoked device and panic

- [ ] **Revoke:** during a session, `dishus revoke <phone>`. The phone shows "This phone's pairing
      was revoked or expired on the PC" immediately, with no retry loop. The PC shows a
      notification.
- [ ] **After revoke:** reconnect attempts fail. `dishus list` no longer shows it. Pair again to
      restore.
- [ ] **Kill:** during a session, `dishus kill`. The session ends ("The PC cut all sessions") and
      the phone does not reconnect.
- [ ] **Lockdown:** `dishus status` shows lockdown. Reboot the PC: still in lockdown, the phone
      can't connect, and `dishus pair` is refused.
- [ ] **Unlock:** `dishus unlock` → the phone connects again.
- [ ] **Kill with daemon hung:** run `kill -STOP $(pidof dishus-daemon)`, then `dishus kill`.
      After a 3 s IPC timeout it writes the lockdown flag itself and stops the unit (systemd
      sends SIGTERM + SIGCONT). Then:
  - [ ] `systemctl --user start dishus` → `dishus status` shows LOCKDOWN.
  - [ ] `dishus unlock` lifts it.

## 8. Stranger checks

- [ ] **Unpaired client:** a second phone or the desktop test client with a fresh identity
      connects to the PC id → the connection is refused, the PC shows no notification, and the
      journal (debug) shows only the gate rejection.
- [ ] **Open ports:** `ss -tlnp | grep dishus` shows **no** listening TCP socket. `ss -ulnp` shows
      only the QUIC UDP socket(s).
- [ ] **File permissions:** `ls -la ~/.config/dishus ~/.local/state/dishus` → files `-rw-------`,
      dirs `drwx------`.

## 9. Other desktops (if available)

- [ ] GNOME (Wayland): the portal dialog appears, the restore token persists, input works.
- [ ] KDE Plasma (Wayland): same.
- [ ] niri: same (needs `xdg-desktop-portal-gnome` for ScreenCast).
- [ ] X11 session: capture falls back to `ximagesrc` (`DISPLAY` must be imported into the user
      manager).
- [ ] Intel/AMD GPU: the encoder shows `vah264enc`/`vah264lpenc`. Note the worker memory.
