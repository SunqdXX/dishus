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

# 2. GStreamer pieces
missing=()
have() { gst-inspect-1.0 "$1" >/dev/null 2>&1; }
command -v gst-inspect-1.0 >/dev/null || missing+=("gstreamer tools (gst-inspect-1.0)")
have pipewiresrc || missing+=("GStreamer PipeWire plugin")
have h264parse || missing+=("gst-plugins-bad (h264parse)")
enc=""
for e in nvh264enc vah264enc x264enc openh264enc; do have "$e" && { enc=$e; break; }; done
[ -n "$enc" ] || missing+=("an H.264 encoder: NVIDIA → gst-plugins-bad (nvcodec); Intel/AMD → gst-plugin-va; any CPU → x264 (gst-plugins-ugly)")
if [ ${#missing[@]} -gt 0 ]; then
  warn "missing:"; printf '    - %s\n' "${missing[@]}"
  cat <<'EOF'
  Arch:          sudo pacman -S gstreamer gst-plugins-base gst-plugins-bad gst-plugin-pipewire gst-plugins-ugly
  Ubuntu/Debian: sudo apt install gstreamer1.0-tools gstreamer1.0-plugins-bad gstreamer1.0-pipewire gstreamer1.0-plugins-ugly
  Fedora:        sudo dnf install gstreamer1-plugins-bad-free gstreamer1-plugins-ugly-free pipewire-gstreamer
EOF
  echo "  Then run ./install.sh again."
  exit 1
fi
ok "GStreamer found (encoder: $enc)"

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
