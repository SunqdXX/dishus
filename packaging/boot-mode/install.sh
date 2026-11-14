#!/bin/sh
# Install DiShus login-screen mode (Linux, SDDM with the X11 login screen).
#
#   sudo packaging/boot-mode/install.sh [--daemon PATH] [--owner USER]
#
# --daemon  the dishus-daemon to copy (default: dishus-daemon on PATH)
# --owner   who may revoke/kill/unlock without sudo (default: the user running sudo)
#
# Checks everything first and changes nothing if a check fails. It does not restart SDDM
# (that would end the graphical session): the change applies at the next login screen.
# Then allow a phone, from your session: dishus boot allow <phone>
# Undo all of it: sudo packaging/boot-mode/uninstall.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
LIB=/usr/local/lib/dishus-boot
DROPIN=/etc/sddm.conf.d/50-dishus-boot.conf
XSETUP=/usr/share/sddm/scripts/Xsetup

daemon=$(command -v dishus-daemon 2>/dev/null || true)
owner=${SUDO_USER:-}
while [ $# -gt 0 ]; do
    case $1 in
        --daemon) daemon=$2; shift 2 ;;
        --owner) owner=$2; shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

fail() { echo "install.sh: $*" >&2; echo "Nothing was changed." >&2; exit 1; }

# ---- checks (nothing changes until all pass) ----
[ "$(id -u)" = 0 ] || fail "run it with sudo"
[ -n "$daemon" ] && [ -x "$daemon" ] || fail "no dishus-daemon found; pass --daemon PATH"
"$daemon" --version >/dev/null 2>&1 || fail "$daemon does not run"
[ -n "$owner" ] && owner_uid=$(id -u "$owner" 2>/dev/null) || fail "no owner; pass --owner USER"
[ "$owner_uid" != 0 ] || fail "the owner must be your normal user, not root"
dm=$(readlink -f /etc/systemd/system/display-manager.service 2>/dev/null || true)
case $dm in */sddm.service) ;; *) fail "the display manager is not SDDM ($dm)" ;; esac
for t in xhost setfacl modprobe systemd-sysusers udevadm timeout; do
    command -v $t >/dev/null 2>&1 || fail "$t is missing (xhost: pacman -S xorg-xhost)"
done
confs=$(ls /usr/lib/sddm/sddm.conf.d/*.conf /etc/sddm.conf.d/*.conf /etc/sddm.conf 2>/dev/null | grep -vx "$DROPIN" || true)
for f in $confs; do
    grep -qiE '^[[:space:]]*DisplayServer[[:space:]]*=[[:space:]]*(wayland|x11-user)' "$f" &&
        fail "$f runs the login screen without root X11 (DisplayServer); login mode needs the X11 login screen"
    cmd=$(sed -nE 's/^[[:space:]]*DisplayCommand[[:space:]]*=[[:space:]]*//p' "$f")
    [ -z "$cmd" ] && continue
    # The packaged default (Arch: /usr/lib/sddm/sddm.conf.d/default.conf) is the stock
    # Xsetup, which sddm-xsetup runs first. Anything else, or any setting in /etc (it would
    # also override ours), is yours: not touched.
    case $f in
        /usr/lib/*) [ "$cmd" = "$XSETUP" ] && continue ;;
    esac
    fail "$f sets DisplayCommand=$cmd; not overriding it"
done
[ -f "$XSETUP" ] || echo "note: no /usr/share/sddm/scripts/Xsetup (fine, nothing to chain)"

# ---- install; on any error, undo what was done ----
trap 'echo "install.sh: failed; undoing" >&2; "$here/uninstall.sh" --quiet; exit 1' EXIT
install -d -m 755 "$LIB"
install -m 755 "$daemon" "$LIB/dishus-daemon"
install -m 755 "$here/sddm-xsetup" "$LIB/sddm-xsetup"
install -m 755 "$here/uinput-access" "$LIB/uinput-access"
if [ ! -d /etc/sysusers.d ]; then
    install -d -m 755 /etc/sysusers.d
    touch "$LIB/created-sysusers-d"
fi
install -m 644 "$here/dishus-boot.sysusers" /etc/sysusers.d/dishus-boot.conf
systemd-sysusers /etc/sysusers.d/dishus-boot.conf
sed "s/@OWNER_UID@/$owner_uid/" "$here/dishus-boot.service" > /etc/systemd/system/dishus-boot.service
chmod 644 /etc/systemd/system/dishus-boot.service
install -Dm 644 "$here/81-dishus-boot-uinput.rules" /etc/udev/rules.d/81-dishus-boot-uinput.rules
udevadm control --reload
# SDDM last, once the script it points to is in place.
if [ ! -d /etc/sddm.conf.d ]; then
    install -d -m 755 /etc/sddm.conf.d
    touch "$LIB/created-sddm-conf-d"
fi
install -m 644 "$here/50-dishus-boot.conf" "$DROPIN"
systemctl daemon-reload
systemctl enable dishus-boot.service
systemctl restart dishus-boot.service  # a reinstall runs the new binary
trap - EXIT

echo "Login-screen mode installed (owner: $owner, uid $owner_uid)."
echo "It applies at the next login screen (reboot, or log out). No phone is allowed yet:"
echo "  dishus boot allow <phone>     (from your session; see dishus list)"
echo "Turn it off:  sudo systemctl disable --now dishus-boot"
echo "Remove it:    sudo $here/uninstall.sh"
