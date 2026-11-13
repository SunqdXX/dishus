# Testing on Windows in a VM

A Windows 11 virtual machine on a Linux PC, so DiShus's Windows build can be installed, run and
checked without a Windows PC. Everything is driven from the Linux side with the `scripts/vm-*.sh`
scripts: copy a build in, run it on the Windows desktop, read its logs, take screenshots, and
throw all changes away again.

**Video doesn't work in the VM yet.** See [Known limits](#known-limits). Tested so far with
0.1.4: the installer, the firewall prompt, pairing, keyboard and mouse input (with the
`desktop_client` example as the phone), Caps Lock being switched back off when a session ends,
auto-start at login, `dishus status`, `dishus doctor`, the logs, and all of it again as a
standard (non-admin) user, with the same PC id after a restart. A real phone hasn't been tried
yet.

## How it works

- **The VM** runs under your own user's libvirt (`qemu:///session`). There's no root daemon, no
  network bridge and no firewall change on the host. libvirt keeps its own settings in
  `~/.config/libvirt`.
- **Files** live in one folder, `~/VMs/windows` by default (set `DISHUS_VM_DIR` to change it). It
  needs about 80 GB, so put it on a big disk:

  ```
  disk/         the Windows disk (win11.qcow2) and the overlay on top of it
  iso/          win11.iso, virtio-win.iso, OpenSSH-Win64.msi
  ssh/          the VM's own SSH key and known_hosts (your ~/.ssh is not used)
  config/       the Windows user's random password
  logs/         logs fetched by vm-logs.sh
  screenshots/  screenshots from vm-shot.sh
  ```

- **Network:** the VM reaches the internet through passt. Its SSH server is forwarded to
  `127.0.0.1:2222` on the host and nowhere else.
- **Install:** fully unattended. `scripts/vm/autounattend.xml` sets up the disk, a local admin
  user `dishus` with automatic login, and turns off BitLocker. On first login,
  `scripts/vm/setup.ps1` installs the virtio drivers and OpenSSH Server, allows key login, turns
  off sleep and the lock screen, and reboots.
- **Running things on the desktop:** an SSH session can't see the screen or send input, so it's
  no good for running DiShus. `vm-run.sh` creates a scheduled task for the logged-in user, set
  to "run only when the user is logged on", and starts it. The program then runs on the visible
  desktop as the normal (not elevated) user, like a double-click would. Its output goes to
  `C:\dishus\run-<time>.log`, one file per run, and `vm-run.sh` prints the name.
- **Snapshot:** "clean" is the freshly installed Windows, before any DiShus. `vm-reset.sh` goes
  back to it in about a minute.

## Setting it up (once)

1. Install the tools:

   ```sh
   sudo pacman -S --needed qemu-desktop edk2-ovmf libvirt virt-install swtpm passt libisoburn imagemagick
   ```

2. Download a Windows 11 ISO from Microsoft and save it as `~/VMs/windows/iso/win11.iso`.
   Tested with the regular multi-edition ISO (UK English); `vm-create.sh` reads the language
   from the ISO. The script installs Windows 11 Pro with Microsoft's generic
   install key, so Windows stays unactivated: it shows a watermark and locks the wallpaper
   settings, nothing else. For another edition or your own key, set `DISHUS_VM_EDITION` and
   `DISHUS_VM_KEY`.

3. Download the virtio drivers and the OpenSSH installer into the same folder:

   ```sh
   cd ~/VMs/windows/iso
   curl -LO https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso
   curl -L -o OpenSSH-Win64.msi https://github.com/PowerShell/Win32-OpenSSH/releases/download/10.4.0.0p1-Preview/OpenSSH-Win64-v10.4.0.0.msi
   ```

4. Create the VM. It starts installing straight away and nothing opens on your screen:

   ```sh
   scripts/vm-create.sh
   ```

   On a hard disk this takes about an hour. `scripts/vm-shot.sh` shows where it is.

5. When `scripts/vm-ssh.sh hostname` answers and a screenshot shows the desktop, take the
   "clean" snapshot. This also removes the install CDs:

   ```sh
   scripts/vm-finish.sh
   ```

## Testing a build

```sh
virsh -c qemu:///session start dishus-win11      # start it (or vm-reset.sh for a fresh one)
scripts/vm-ssh.sh 'Stop-Computer -Force'         # shut it down when you're done

scripts/vm-reset.sh                 # fresh Windows, desktop up
scripts/vm-push.sh                  # copies the newest dist/*windows*.exe to C:\dishus\
scripts/vm-run.sh 'C:\dishus\DiShus-0.1.4-windows-x86_64.exe'
scripts/vm-shot.sh                  # screenshot, prints the PNG's path
scripts/vm-logs.sh                  # DiShus's logs and the run logs, into ~/VMs/windows/logs/<time>/
scripts/vm-ssh.sh 'Get-Process dishus*'   # any PowerShell command
```

To test as a standard user, create one over SSH, make it the auto-login user (registry
`Winlogon`: `AutoAdminLogon`, `DefaultUserName`, `DefaultPassword`), restart, and set
`DISHUS_VM_USER=<name>` for `vm-run.sh`. Its DiShus log is private to that user, so read it
through `vm-run.sh` (for example with `findstr`), not over SSH. `vm-reset.sh` removes the user
again.

DiShus commands like `dishus status` have to go through `vm-run.sh` too:

```sh
scripts/vm-run.sh 'C:\Users\dishus\AppData\Local\Programs\dishus\dishus.exe' status
```

Over SSH the daemon refuses them ("rejected control-pipe client: Access is denied" in its log).
That's on purpose: an SSH login is elevated, and the daemon only talks to programs running as
the same, non-elevated user.

The Windows build comes from CI (the `dishus-windows` artifact of a run), or from
`packaging\windows\build.ps1` on a Windows PC. It can't be built on Linux.

To click something on the VM's screen, send keys from the host and check with a screenshot after
each one. For example, the firewall prompt the first time DiShus runs: one Tab moves to Allow,
then Enter.

```sh
virsh -c qemu:///session send-key dishus-win11 KEY_TAB
virsh -c qemu:///session send-key dishus-win11 KEY_ENTER
```

## Without a phone

The `desktop_client` example can play the phone from the Linux side. The installer opens the
pairing window for 60 seconds and writes the link into its run log, so be quick:

```sh
export DISHUS_CLIENT_DIR=/tmp/dishus-vm-client    # keeps this pairing apart from any other
link=$(scripts/vm-ssh.sh "(Select-String -Path C:\dishus\run-*.log -Pattern 'dishus://pair' | Select -Last 1).Line.Trim()" | tr -d '\r')
cargo run -p dishus-core --example desktop_client -- pair "$link"
cargo run -p dishus-core --example desktop_client -- keys 35      # types h
cargo run -p dishus-core --example desktop_client -- mouse move:-15:120,right
```

Each `keys` or `mouse` call is one session. Linux key codes: 35 is H, 30 is A, 58 is Caps Lock.
Mouse moves are relative and Windows speeds them up (a move of 15 went about 37 pixels).
A window to type into: `scripts/vm-run.sh 'C:\Windows\System32\cmd.exe' /k` shows what's typed.
Notepad doesn't work for this: on a fresh VM it starts but never shows a window.

## Known limits

- **No video.** DiShus can't create its graphics device in the VM: `dishus doctor` says
  "creating D3D11 device: ... not supported (0x887A0004)", with both the virtio and the plain
  VGA display. The VM has no real GPU, only Windows' software renderer ("Microsoft Basic Render
  Driver"), and that has no video processor. DiShus asks for one
  (`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`) because it converts the picture to NV12 on the GPU.
  Without that flag the device is created and Desktop Duplication delivers frames, so a fix
  needs a CPU fallback for the conversion. A real PC without a GPU driver would hit the same.
- **No GPU encoders.** NVENC, Quick Sync and AMF can't be tested here; the VM has no GPU.
- **The lock screen** can't be tested: auto-login skips it.
- **A standard user** gets the firewall prompt too, and Allow then asks for an admin password.
  Cancelling it is fine: pairing and input still worked.
- **The SFTP server** in this OpenSSH build quits right after starting, so the scripts use
  legacy scp (`scp -O`).
- **"Press any key to boot from CD":** `vm-create.sh` presses F12 for 12 seconds to get past it.
  Enter can't be used: the extra presses reach Windows Setup and click Cancel.
- **The snapshot** covers the disk only, not the UEFI settings or the TPM. That's fine because
  BitLocker is off.

## Keep it private

`logs/` and `screenshots/` can contain pairing links. A pairing link includes the PC's public and
local IP addresses. Don't paste them into issues or commit them. The VM folder is outside the
repository for that reason.
