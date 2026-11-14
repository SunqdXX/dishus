#!/bin/sh
# Remove DiShus login-screen mode completely: service, user, its data (allowed phones, its
# key, limits), the uinput access and the SDDM change. Safe to run more than once.
#
#   sudo packaging/boot-mode/uninstall.sh
set -u
LIB=/usr/local/lib/dishus-boot
quiet=no
[ "${1:-}" = --quiet ] && quiet=yes
[ "$(id -u)" = 0 ] || { echo "uninstall.sh: run it with sudo" >&2; exit 1; }
say() { [ $quiet = yes ] || echo "$@"; }

# SDDM first: back to its normal Xsetup at the next login screen.
rm -f /etc/sddm.conf.d/50-dishus-boot.conf
if [ -f "$LIB/created-sddm-conf-d" ]; then
    rmdir /etc/sddm.conf.d 2>/dev/null
fi
systemctl disable --now dishus-boot.service 2>/dev/null
# the unit and any overrides made for it (systemctl edit dishus-boot)
rm -rf /etc/systemd/system/dishus-boot.service /etc/systemd/system/dishus-boot.service.d
systemctl daemon-reload
systemctl reset-failed dishus-boot.service 2>/dev/null
rm -f /etc/udev/rules.d/81-dishus-boot-uinput.rules
udevadm control --reload 2>/dev/null
if [ -e /dev/uinput ] && getfacl -p /dev/uinput 2>/dev/null | grep -q '^user:dishus-boot:'; then
    setfacl -x u:dishus-boot /dev/uinput
    # Nothing else named left (e.g. your own user's entry)? Then drop the leftover mask too.
    getfacl -p --omit-header /dev/uinput 2>/dev/null | grep -qE '^(user|group):[^:]+:' ||
        setfacl -b /dev/uinput
fi
rm -f /etc/sysusers.d/dishus-boot.conf
if [ -f "$LIB/created-sysusers-d" ]; then
    rmdir /etc/sysusers.d 2>/dev/null
fi
rm -rf "$LIB" /var/lib/dishus-boot /run/dishus-boot
if id dishus-boot >/dev/null 2>&1; then
    userdel dishus-boot 2>/dev/null
fi
getent group dishus-boot >/dev/null 2>&1 && groupdel dishus-boot 2>/dev/null
say "Login-screen mode removed. The login screen is back to normal at its next start."
exit 0
