# DiShus security design

DiShus hands a phone full keyboard and mouse control of a PC. A bug here is a remote-code-execution
bug, so the design aims to keep the attack surface reachable by strangers close to zero, and to
make it easy to cut a device off.

## Threat model

| Adversary | Can | Must not be able to |
|---|---|---|
| Anyone on the internet who knows or guesses the PC's EndpointId | send QUIC packets to the PC (directly or via a relay) | get any application bytes parsed, learn whether a device list exists, exhaust resources |
| Relay operator (n0's public relays or any configured relay) | see the endpoint IDs of both sides, the timing and the volume | read or modify traffic (end-to-end QUIC/TLS 1.3 between the two keys) |
| Someone who photographs the pairing QR | use it within 60 s, once, while the window is open | use it later, or a second time |
| A thief with the phone, locked | the encrypted vault blob | the phone's private key without the app password, or the Android Keystore key (hardware-bound) |
| A thief with the phone, unlocked | use it until auto-lock, or until the owner runs `dishus revoke` / `dishus kill` | keep access after revoke or kill |
| Other local users on the PC | — | talk to the daemon's control socket (0600 + peer-uid check), or read its keys (0600 in 0700 dirs) |
| A malicious or compromised paired phone | send keyboard and mouse input (this is the product) | power off or suspend the PC through keys, crash or memory-exhaust the daemon with malformed frames |

**Out of scope:**
- A compromised PC user account. It owns the daemon anyway.
- A compromised phone OS while the app is unlocked.
- The physical security of an auto-logged-in PC. Use a lock screen.

## Identities and transport

- Both sides have Ed25519 keys and are addressed by their public key (iroh `EndpointId`). QUIC
  with TLS 1.3 proves possession of the key during the handshake, so the peer ID the daemon sees
  *is* the authenticated identity. No passwords or bearer tokens are sent per connection.
- The PC's secret key: `~/.config/dishus/secret.key`, 32 bytes, `0600`.
- The phone's secret key is generated on the phone and never leaves it. See [Phone](#phone).
- The PC opens no listening TCP socket. It has one UDP socket for QUIC, on a random port unless
  `udp_port` is set. iroh's portmapper may request a UPnP/NAT-PMP mapping for that UDP port.

## Admission: before any application parsing

Admission happens in three layers:

1. **`incoming_filter` (before the QUIC handshake).**
   - Over a relay, iroh tells us the claimed peer ID up front. Unknown IDs are **silently
     ignored** unless the pairing window is open.
   - Direct UDP connections must first pass a QUIC Retry (address validation, so no amplification),
     then a per-IP rate limit of 30 handshakes per 60 s.
2. **`after_handshake` hook (TLS done, no stream accepted yet).**
   - `dishus/session/1`: the peer's TLS-proven ID must be on the allowlist, must not be expired,
     and the daemon must not be in lockdown.
   - `dishus/pair/1`: allowed only while the pairing window is open.
   - Anything else is closed with error code 0 and an empty reason. An outsider can't tell
     "unknown" from "expired" from "lockdown".
3. **Session handler.** It re-checks the allowlist (closing the race with a concurrent revoke)
   before reading the first frame.

So a stranger never reaches postcard decoding, uinput or the capture stack.

## Pairing

Starting a pairing:
- Pairing happens only when the user runs `dishus pair` at the PC; the window is closed by default.
- The token is 256 bits from the OS CSPRNG. It is valid for 60 s (configurable *down* to 10 s,
  never above 60), and for **one** successful use.
- Three wrong tokens close the window.
- The comparison is constant-time (`subtle::ConstantTimeEq`), and token buffers are zeroized.

The QR / `dishus://pair` link carries:
- The PC's EndpointId, so the phone authenticates the PC by key. A MITM would need the PC's
  secret key.
- The token.
- The PC's name, relay URLs and direct addresses (hints only, not trusted for identity).

What the PC stores:
- The ID the phone **proved during the TLS handshake**, not one claimed inside a message.
- The phone's name, sanitized, ≤ 64 characters.
- The expiry: 30 days by default, configurable 1–60.
- The device limit is 32.

## Expiry, renewal, revocation, panic

- **Expiry.** An expired device is treated exactly like an unknown one: dropped silently at the
  gate. A live session is also checked every 15 s and ended at expiry.
- **Renewal** (1–60 days from *now*):
  - from inside an authenticated session (phone → `RenewPairing`, in-app "Renew")
  - or at the PC: `dishus renew <device> [--days N]`
  - An already-expired device can only be renewed at the PC.
- **`dishus revoke`.** Removes the key and immediately ends that device's session with
  `Bye{Revoked}`. All held keys are released.
- **`dishus kill`.** Ends every session, releases all input, and enters **lockdown**. Lockdown is
  persisted to disk and survives daemon restarts and reboots until `dishus unlock`.
  - While locked down, every session *and* pairing connection is refused.
  - If the daemon is unresponsive, `kill` falls back to stopping the systemd unit.
- Desktop notifications announce pairing, connect, disconnect, revoke and kill.

## Protocol hardening

- **Size caps before allocation.** Each frame has a u32 length prefix, and the cap is checked
  before the buffer is allocated: 64 KiB control, 8 MiB per video frame, 512 bytes during pairing,
  256 KiB on the local IPC socket.
- **Strict decoding.** postcard rejects trailing bytes. Every message then runs `validate()`:
  - names: length and charset
  - keycodes: allow-list 1..=248, minus KEY_POWER, KEY_SLEEP, KEY_WAKEUP, KEY_SUSPEND
  - mouse deltas ≤ 4096, scroll ≤ 2400
  - stream parameters within range, renew days 1–60
- An invalid message ends the session; it is not ignored.
- **Timeouts.**
  - Hello within 10 s.
  - 5 s of silence (the phone pings every 1 s) ends the session **and releases every held key and
    button**. Released input is tracked per key, so nothing stays stuck.
  - Pairing exchange timeouts.
- **One session at a time.** A newer authenticated session replaces the older one.
- **No `unsafe`** (`unsafe_code = "deny"` workspace-wide). There is one exception: the
  Android-only JNI bootstrap in `crates/dishus-core/src/android.rs`. It hands the JavaVM and the
  Application context to iroh's DNS and network-interface code, and is unavoidable raw-pointer
  FFI. Secrets are zeroized (`zeroize`).
  Tokens are redacted in `Debug` output, and no secret is ever logged.

## Local attack surface

- **Control socket.** `$XDG_RUNTIME_DIR/dishus/control.sock`, mode `0600`. It also checks the
  peer's uid (SO_PEERCRED) and refuses other users.
- **Files.** Everything is written atomically with `0600` under `0700` directories. On read, files
  that are group/other-accessible or owned by someone else are refused.
- **Capture worker.** Capture and encoding (GStreamer, the GPU driver, the portal) run in a child
  process that exists only during a session and is killed with it. A media-stack crash cannot take
  down the process that owns uinput and the network endpoint.
- **systemd hardening** (`packaging/systemd/dishus.service`):
  - `NoNewPrivileges`, empty `CapabilityBoundingSet`
  - `ProtectSystem=strict`, `ProtectHome=read-only` with only the dishus dirs writable,
    `PrivateTmp`
  - kernel, clock and hostname protection
  - `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK`, `RestrictNamespaces`,
    `RestrictRealtime`, `RestrictSUIDSGID`
  - `SystemCallFilter=@system-service`, `LockPersonality`
  - Not set, deliberately:
    - `PrivateDevices`: it would hide `/dev/uinput` and the GPU.
    - `MemoryDenyWriteExecute`: the NVIDIA/CUDA user-space driver JIT-compiles and needs W+X pages.
- **uinput access.** Only the logged-in seat user gets it, through the optional udev `uaccess`
  rule; it is not world-writable.

## Phone

- **Vault.** The app password goes through Argon2id (64 MiB, t = 3, p = 1; the floor is 19 MiB,
  t = 2), then XChaCha20-Poly1305 with the parameter header as AAD.
  - It contains the phone's secret key and the paired PCs (ID, name, relays, addresses, expiry).
  - Passwords must be ≥ 8 characters.
- **Keystore wrap.** The sealed vault is encrypted again with an Android Keystore AES-256-GCM key
  (StrongBox-backed when the device has it). Copying `vault.bin` off the device is useless without
  the device's Keystore *and* the password.
- **Biometric unlock (optional).** The password is stored encrypted under a separate Keystore key
  that requires `BIOMETRIC_STRONG` authentication per use and is invalidated when enrolments change.
- **Auto-lock.**
  - The app re-locks after N minutes in the background (default 5; 0 = immediately). Locking drops
    the unlocked keys from the Rust core.
  - The session is closed as soon as the app goes to the background.
- **Screen and storage protection.**
  - `FLAG_SECURE`: no screenshots, screen recording or recents thumbnail. This is on by default.
    *Settings → Allow screenshots* turns it off for the unlocked screens, e.g. to screenshot the
    PC for someone. The lock screen is always protected.
  - `allowBackup=false` plus data-extraction rules excluding everything.
  - Storage in `noBackupFilesDir`, no cleartext traffic.
- **Permissions:** INTERNET, CAMERA (QR scan, prompted when used), USE_BIOMETRIC, VIBRATE. Nothing
  else.

## Supply chain

- CI runs `cargo audit` and `cargo deny check` (advisories, licences, sources, bans).
- One advisory is ignored with a recorded reason: RUSTSEC-2024-0436, `paste` unmaintained. It is
  build-time only, via iroh's dependencies.
- The lockfile is committed.

## Known weaknesses / not done

- **Relay metadata.** Relay operators see which keys talk to which, when, and how much. Self-host
  a relay, or set `disable_relays`, if that matters.
- **Unlocked phone.** Anyone holding an unlocked, connected phone can control the PC until
  auto-lock or revoke. That is inherent to the product.
- **Seat-wide input.** Input goes to the whole seat via uinput, not to one application.
- **Unreviewed Android path.** The Android security path has **not been compiled or tested on a
  device yet**. Treat it as unreviewed.
- **No external audit.**

## Reporting

Please report vulnerabilities privately (GitHub security advisory on the repository) rather than
in a public issue.
