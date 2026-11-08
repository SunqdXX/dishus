#!/usr/bin/env bash
# DiShus PC installer (from the release tarball). Installs for the current user only:
#   ~/.local/bin/dishus-daemon, ~/.local/bin/dishus, ~/.config/systemd/user/dishus.service
# It never runs sudo itself; if keyboard/mouse access (/dev/uinput) is missing it prints the
# commands to run.  Uninstall: ./install.sh --uninstall
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="$HOME/.local/bin"
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
bold() { printf '\033[1m%s\033[0m\n' "$*"; }
warn() { printf '\033[33m! %s\033[0m\n' "$*"; }
ok() { printf '\033[32m✓\033[0m %s\n' "$*"; }

if [ "${1:-}" = "--uninstall" ]; then
  systemctl --user disable --now dishus.service 2>/dev/null || true
  rm -f "$bin/dishus-daemon" "$bin/dishus" "$unit_dir/dishus.service"
  # xdph calls the picker for EVERY app's screen share; removing it while xdph.conf still
  # names it would break screen sharing everywhere.
  if grep -qs dishus-share-picker "${XDG_CONFIG_HOME:-$HOME/.config}/hypr/xdph.conf"; then
    warn "kept $bin/dishus-share-picker: your hypr/xdph.conf still uses it. Remove that line, then the file."
  else
    rm -f "$bin/dishus-share-picker"
  fi
  systemctl --user daemon-reload
  ok "removed. Your pairings and settings are kept in ~/.config/dishus and ~/.local/state/dishus"
  exit 0
fi

bold "DiShus installer"

# 1. Can the binaries run here at all?
if ! "$here/bin/dishus" --version >/dev/null 2>&1; then
  warn "the prebuilt binaries don't run on this system:"
  "$here/bin/dishus" --version 2>&1 | head -3 || true
  echo "  They need glibc ≥ 2.39 (Arch, Ubuntu 24.04+, Debian 13+, Fedora 40+) and GStreamer."
  echo "  On older systems build from source: https://github.com/SunqdXX/dishus#install-pc-from-source"
  exit 1
fi

# 2. Can this PC capture and encode video? `dishus-daemon --doctor` really test-encodes with
#    every encoder and prints the exact packages for this distro + GPU if something is missing.
if ! "$here/bin/dishus-daemon" --version >/dev/null 2>&1; then
  warn "dishus-daemon can't start: GStreamer itself is not installed:"
  "$here/bin/dishus-daemon" --version 2>&1 | head -2 | sed 's/^/    /' || true
  id_like=$(. /etc/os-release 2>/dev/null; echo "${ID:-} ${ID_LIKE:-}")
  case " $id_like " in
    *" arch "*)                echo "  sudo pacman -S --needed gstreamer gst-plugins-base gst-plugins-bad-libs gst-plugin-pipewire" ;;
    *" debian "*|*" ubuntu "*) echo "  sudo apt install gstreamer1.0-plugins-base gstreamer1.0-plugins-bad gstreamer1.0-pipewire" ;;
    *" fedora "*)              echo "  sudo dnf install gstreamer1 gstreamer1-plugins-base gstreamer1-plugins-bad-free pipewire-gstreamer" ;;
    *)                         echo "  install GStreamer 1.x with its base and \"bad\" plugins and the PipeWire plugin" ;;
  esac
  echo "  Then run ./install.sh again."
  exit 1
fi
if ! "$here/bin/dishus-daemon" --doctor; then
  echo
  warn "video can't work yet: run the command above, then ./install.sh again."
  exit 1
fi
echo

# 3. Install
install -Dm755 "$here/bin/dishus-daemon" "$bin/dishus-daemon"
install -Dm755 "$here/bin/dishus" "$bin/dishus"
if [ -f "$here/bin/dishus-share-picker" ]; then
  install -Dm755 "$here/bin/dishus-share-picker" "$bin/dishus-share-picker"
fi
mkdir -p "$unit_dir"
sed 's|^ExecStart=.*|ExecStart=%h/.local/bin/dishus-daemon|' "$here/dishus.service" > "$unit_dir/dishus.service"
ok "installed to $bin and $unit_dir"
case ":$PATH:" in *":$bin:"*) ;; *) warn "$bin is not in your PATH; add it to use the 'dishus' command" ;; esac

# 4. Keyboard/mouse access
if [ -w /dev/uinput ]; then
  ok "keyboard/mouse access (/dev/uinput) OK"
else
  warn "no access to /dev/uinput yet: the phone could watch but not type or click."
  echo "  Run these once (they let the user at this PC's screen create virtual input devices):"
  echo "    sudo install -Dm644 '$here/70-dishus-uinput.rules' /etc/udev/rules.d/70-dishus-uinput.rules"
  echo "    echo uinput | sudo tee /etc/modules-load.d/uinput.conf"
  echo "    sudo udevadm control --reload && sudo modprobe uinput && sudo udevadm trigger"
  echo "  then: systemctl --user restart dishus"
fi

# 4b. Hyprland: answer xdph's picker for phone requests (opt-in, we never edit your config)
if [ -x "$bin/dishus-share-picker" ] && command -v hyprctl >/dev/null 2>&1; then
  xdph="${XDG_CONFIG_HOME:-$HOME/.config}/hypr/xdph.conf"
  if ! grep -qs 'dishus-share-picker' "$xdph"; then
    warn "Hyprland: so the PC never shows a screen-share dialog when the phone connects, add to $xdph:"
    echo "    screencopy {"
    echo "        allow_token_by_default = true"
    echo "        custom_picker_binary = $bin/dishus-share-picker"
    echo "    }"
    echo "  then: systemctl --user restart xdg-desktop-portal-hyprland   (README: 'Hyprland: no screen-share dialog')"
  fi
fi

# 5. Start
systemctl --user daemon-reload
systemctl --user enable --now dishus.service
sleep 1
if "$bin/dishus" status >/dev/null 2>&1; then
  ok "daemon running"
else
  warn "the daemon did not start; see: journalctl --user -u dishus -e"
  exit 1
fi
echo
bold "Next: pair your phone"
echo "  1. Install the DiShus app on the phone (DiShus-*.apk from the same release)."
echo "  2. Here, run:  dishus pair"
echo "     Allow screen sharing in the dialog (tick 'remember' / 'allow restore token'),"
echo "     then scan the QR code with the app."
